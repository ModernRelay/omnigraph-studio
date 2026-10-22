//! Studio checks for, downloads, verifies and installs its own updates (spec 017).
//!
//! Layer 3 (`docs/architecture.md`): the only crate that makes requests not to an Omnigraph
//! server, and only to Studio's own releases.

pub mod install;
pub mod manifest;
mod packager;

pub use packager::PackagerInstaller;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{AsyncReadExt as _, FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use http_client::{AsyncBody, HttpClient};
use semver::Version;

use install::Blocked;
use manifest::{PlatformUpdate, Release};

/// How long after launch a release build first checks.
pub const FIRST_CHECK: Duration = Duration::from_secs(30);
/// How often it checks while it runs.
pub const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);
/// How long a check waits for the manifest.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the answer to a check the person asked for stays in the status bar.
pub const SAID_FOR: Duration = Duration::from_secs(10);

/// What the updater has to say, which the status bar shows (spec 017, "What the person sees").
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    /// Nothing to say: not checked yet, up to date, or checking in the background.
    Idle,
    /// Checking because the person asked.
    Checking,
    /// The person asked, and this is the newest version.
    UpToDate,
    Downloading {
        version: Version,
        percent: Option<u8>,
    },
    Installing {
        version: Version,
    },
    /// Installed; the next launch runs it.
    Ready {
        version: Version,
    },
    /// Newer, and not installed here: the person downloads it. `reason` says why, when Studio
    /// would have installed it.
    Available {
        version: Version,
        reason: Option<String>,
    },
    /// A check the person asked for failed.
    Failed {
        message: String,
    },
    /// The person asked in a development build.
    Off,
}

/// Where this Studio runs, read once at launch.
#[derive(Debug, Clone)]
pub struct Options {
    pub running: Version,
    /// `macos-aarch64`, `linux-x86_64`, …; `None` on a platform Studio does not build.
    pub platform: Option<String>,
    /// An installed Studio, not a build or a test: only it checks.
    pub release_build: bool,
    /// The `.app` bundle on macOS.
    pub bundle: Option<PathBuf>,
    /// The update key's public half; without it Studio installs nothing.
    pub public_key: Option<String>,
}

/// One update to download and install.
#[derive(Debug, Clone)]
pub struct Job {
    pub version: Version,
    pub running: Version,
    pub update: PlatformUpdate,
    pub bundle: PathBuf,
    pub public_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InstallError {
    /// The file is not a genuine update: never fetched again this run.
    Refused(String),
    /// The download or install did not finish.
    Failed(String),
}

/// Downloads, verifies and installs, off the main thread: `cargo-packager-updater` in a
/// release, a fake in tests.
pub trait Installer: Send + Sync + 'static {
    /// The update's bytes, once its signature verifies; `progress` hears whole percents.
    fn download(
        &self,
        job: &Job,
        progress: Box<dyn Fn(u8) + Send>,
    ) -> BoxFuture<'static, Result<Vec<u8>, InstallError>>;
    /// Puts the downloaded bundle in place of the running one.
    fn install(&self, job: &Job, bytes: Vec<u8>) -> BoxFuture<'static, Result<(), InstallError>>;
}

/// Checks Studio's releases and installs a newer one (spec 017).
pub struct Updater {
    options: Options,
    http: Arc<dyn HttpClient>,
    installer: Arc<dyn Installer>,
    state: UpdateState,
    /// The newest version a check found.
    newest: Option<Version>,
    /// Versions refused or not installed this run, with what was said: never fetched again.
    refused: HashMap<Version, String>,
    /// Whether the person asked for the work under way, so its outcome is said.
    asked: bool,
    work: Option<Task<()>>,
    poll: Option<Task<()>>,
    clear: Option<Task<()>>,
}

struct GlobalUpdater(Entity<Updater>);

impl Global for GlobalUpdater {}

impl Updater {
    pub fn new(
        options: Options,
        http: Arc<dyn HttpClient>,
        installer: Arc<dyn Installer>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            options,
            http,
            installer,
            state: UpdateState::Idle,
            newest: None,
            refused: HashMap::new(),
            asked: false,
            work: None,
            poll: None,
            clear: None,
        }
    }

    /// The updater `init` made, if it ran.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalUpdater>()
            .map(|global| global.0.clone())
    }

    pub fn state(&self) -> &UpdateState {
        &self.state
    }

    /// The version running.
    pub fn running(&self) -> &Version {
        &self.options.running
    }

    /// Checks on a schedule while `on` (`studio.auto_update`): half a minute after launch, then
    /// hourly. A development build never does.
    pub fn set_automatic(&mut self, on: bool, cx: &mut Context<Self>) {
        if !on {
            self.poll = None;
            return;
        }
        if self.poll.is_some() || !self.options.release_build {
            return;
        }
        self.poll = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK).await;
            loop {
                if this.update(cx, |this, cx| this.start(false, cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        }));
    }

    /// The person's Check for Updates: its outcome is said, and joins any check under way.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        if !self.options.release_build {
            self.say(UpdateState::Off, cx);
            return;
        }
        self.start(true, cx);
    }

    /// The release notes of the newer version found, or of this one.
    pub fn release_notes_url(&self) -> String {
        let version = self
            .newest
            .as_ref()
            .filter(|newest| manifest::is_newer(newest, &self.options.running))
            .unwrap_or(&self.options.running);
        manifest::release_page(version)
    }

    /// The newest release's download for this platform, under its version-free name.
    pub fn download_url(&self) -> String {
        manifest::download_url(self.options.platform.as_deref().unwrap_or_default())
    }

    fn start(&mut self, asked: bool, cx: &mut Context<Self>) {
        self.asked |= asked;
        if self.work.is_some() {
            if asked && self.state == UpdateState::Idle {
                self.set(UpdateState::Checking, cx);
            }
            return;
        }
        if matches!(self.state, UpdateState::Ready { .. }) {
            return;
        }
        if asked {
            self.set(UpdateState::Checking, cx);
        }
        let http = self.http.clone();
        let platform = self.options.platform.clone().unwrap_or_default();
        let timeout = cx.background_executor().timer(CHECK_TIMEOUT);
        self.work = Some(cx.spawn(async move |this, cx| {
            let fetched = futures::select_biased! {
                fetched = fetch(http, platform).fuse() => fetched,
                _ = timeout.fuse() => Err(anyhow!("no answer in 15 seconds")),
            };
            this.update(cx, |this, cx| this.checked(fetched, cx)).ok();
        }));
    }

    fn checked(&mut self, fetched: Result<Release>, cx: &mut Context<Self>) {
        let release = match fetched {
            Ok(release) => release,
            Err(error) => {
                log::warn!("update check failed: {error:#}");
                return self.finish(
                    UpdateState::Failed {
                        message: format!("Couldn't check for updates: {error}."),
                    },
                    cx,
                );
            }
        };
        let version = release.version.clone();
        self.newest = Some(version.clone());
        if !manifest::is_newer(&version, &self.options.running) {
            return self.finish(UpdateState::UpToDate, cx);
        }
        if let Some(reason) = self.refused.get(&version) {
            let reason = Some(reason.clone());
            return self.end(UpdateState::Available { version, reason }, cx);
        }
        let job = match (
            &release.update,
            &self.options.bundle,
            &self.options.public_key,
        ) {
            (Some(update), Some(bundle), Some(key)) => Job {
                version: version.clone(),
                running: self.options.running.clone(),
                update: update.clone(),
                bundle: bundle.clone(),
                public_key: key.clone(),
            },
            _ => {
                return self.end(
                    UpdateState::Available {
                        version,
                        reason: None,
                    },
                    cx,
                );
            }
        };
        if let Some(blocked) = install::blocked(&job.bundle) {
            let reason = Some(match blocked {
                Blocked::DiskImage | Blocked::Translocated => {
                    "Move Omnigraph Studio to Applications to update it.".to_owned()
                }
                Blocked::NotWritable(folder) => {
                    format!("Studio can't replace itself in {}.", folder.display())
                }
            });
            return self.end(UpdateState::Available { version, reason }, cx);
        }
        self.download(job, cx);
    }

    fn download(&mut self, job: Job, cx: &mut Context<Self>) {
        let version = job.version.clone();
        self.set(
            UpdateState::Downloading {
                version: version.clone(),
                percent: None,
            },
            cx,
        );
        let (sender, mut percents) = mpsc::unbounded();
        let download = self.installer.download(
            &job,
            Box::new(move |percent| {
                sender.unbounded_send(percent).ok();
            }),
        );
        let installer = self.installer.clone();
        self.work = Some(cx.spawn(async move |this, cx| {
            let mut download = download.fuse();
            let downloaded = loop {
                futures::select_biased! {
                    downloaded = download => break downloaded,
                    percent = percents.select_next_some() => {
                        this.update(cx, |this, cx| this.progress(percent, cx)).ok();
                    }
                }
            };
            let bytes = match downloaded {
                Ok(bytes) => bytes,
                Err(InstallError::Refused(error)) => {
                    log::warn!("update {version} refused: {error}");
                    let reason = format!(
                        "The {version} download isn't signed for Omnigraph Studio and was not installed."
                    );
                    this.update(cx, |this, cx| this.refuse(version, reason, cx)).ok();
                    return;
                }
                Err(InstallError::Failed(error)) => {
                    log::warn!("update {version} download failed: {error}");
                    let message = format!(
                        "The download of {version} didn't finish. Studio will try again at the next check."
                    );
                    this.update(cx, |this, cx| this.finish(UpdateState::Failed { message }, cx))
                        .ok();
                    return;
                }
            };
            // The bundle's own version, read off the main thread: the archive is tens of MB.
            let (found, bytes) = cx
                .background_executor()
                .spawn(async move { (install::bundle_version(&bytes), bytes) })
                .await;
            let reason = match found {
                Ok(found) if found == version.to_string() => None,
                Ok(found) => Some(format!(
                    "The {version} download contains {found} and was not installed."
                )),
                Err(error) => Some(format!(
                    "The {version} download is not a Studio update ({error}) and was not installed."
                )),
            };
            if let Some(reason) = reason {
                this.update(cx, |this, cx| this.refuse(version, reason, cx)).ok();
                return;
            }
            let installing = UpdateState::Installing {
                version: version.clone(),
            };
            if this.update(cx, |this, cx| this.set(installing, cx)).is_err() {
                return;
            }
            let installed = installer.install(&job, bytes).await;
            this.update(cx, |this, cx| match installed {
                Ok(()) => this.end(UpdateState::Ready { version }, cx),
                Err(InstallError::Refused(error) | InstallError::Failed(error)) => {
                    log::warn!("update {version} install failed: {error}");
                    let reason = format!("Couldn't install {version}: {error}.");
                    this.refuse(version, reason, cx);
                }
            })
            .ok();
        }));
    }

    fn progress(&mut self, percent: u8, cx: &mut Context<Self>) {
        if let UpdateState::Downloading { version, .. } = &self.state {
            let version = version.clone();
            self.set(
                UpdateState::Downloading {
                    version,
                    percent: Some(percent),
                },
                cx,
            );
        }
    }

    /// Remembers that `version` is not to be installed this run, and offers its download.
    fn refuse(&mut self, version: Version, reason: String, cx: &mut Context<Self>) {
        self.refused.insert(version.clone(), reason.clone());
        self.end(
            UpdateState::Available {
                version,
                reason: Some(reason),
            },
            cx,
        );
    }

    /// Ends the work under way with an outcome said only to a person who asked.
    fn finish(&mut self, said: UpdateState, cx: &mut Context<Self>) {
        let asked = self.asked;
        self.work = None;
        self.asked = false;
        if asked {
            self.say(said, cx);
        } else {
            self.set(UpdateState::Idle, cx);
        }
    }

    /// Ends the work under way with a state that stays.
    fn end(&mut self, state: UpdateState, cx: &mut Context<Self>) {
        self.work = None;
        self.asked = false;
        self.set(state, cx);
    }

    /// Shows `state` for [`SAID_FOR`], then nothing.
    fn say(&mut self, state: UpdateState, cx: &mut Context<Self>) {
        self.set(state.clone(), cx);
        self.clear = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAID_FOR).await;
            this.update(cx, |this, cx| {
                if this.state == state {
                    this.set(UpdateState::Idle, cx);
                }
            })
            .ok();
        }));
    }

    fn set(&mut self, state: UpdateState, cx: &mut Context<Self>) {
        if self.state != state {
            self.clear = None;
            self.state = state;
            cx.notify();
        }
    }
}

/// The update key's public half: base64 of the minisign public key, as `cargo packager signer
/// generate` writes it to its `.pub` file. Empty until the release key is made (spec 017, open
/// question 2): Studio then tells of a newer version and installs nothing.
const UPDATE_KEY: &str = include_str!("../update-key.pub");

fn key_from(text: &str) -> Option<String> {
    let key = text.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

/// Starts the updater for this Studio: after `graph_store::init`, whose HTTP client it sends
/// through, and `studio_settings::init`, whose `studio.auto_update` it follows.
pub fn init(cx: &mut App) {
    let installed = std::env::current_exe()
        .ok()
        .is_some_and(|exe| studio_settings::is_release_build(&exe));
    // A bundle's path, asked only of an installed Studio on macOS: GPUI gives the executable's
    // folder for a bare binary, and a test platform has no answer.
    let bundle = (installed && cfg!(target_os = "macos"))
        .then(|| cx.app_path().ok())
        .flatten()
        .filter(|path| path.extension().is_some_and(|extension| extension == "app"));
    let release_build = installed && (cfg!(not(target_os = "macos")) || bundle.is_some());
    let options = Options {
        running: Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("the workspace version is semver"),
        platform: cargo_packager_updater::target(),
        release_build,
        bundle,
        public_key: key_from(UPDATE_KEY),
    };
    if release_build {
        cx.background_spawn(async {
            install::clean_leftovers(&std::env::temp_dir(), std::time::SystemTime::now());
        })
        .detach();
    }
    let http = cx.http_client();
    init_with(options, http, Arc::new(PackagerInstaller), cx);
}

fn init_with(
    options: Options,
    http: Arc<dyn HttpClient>,
    installer: Arc<dyn Installer>,
    cx: &mut App,
) {
    let updater = cx.new(|cx| Updater::new(options, http, installer, cx));
    cx.set_global(GlobalUpdater(updater.clone()));
    let automatic = |cx: &App| {
        cx.try_global::<studio_settings::StudioSettings>()
            .is_none_or(|settings| settings.studio.auto_update)
    };
    let on = automatic(cx);
    updater.update(cx, |updater, cx| updater.set_automatic(on, cx));
    cx.observe_global::<studio_settings::StudioSettings>(move |cx| {
        let on = automatic(cx);
        updater.update(cx, |updater, cx| updater.set_automatic(on, cx));
    })
    .detach();
}

/// Reads the newest release's manifest through Studio's HTTP client, at most
/// [`manifest::MAX_MANIFEST_BYTES`].
async fn fetch(http: Arc<dyn HttpClient>, platform: String) -> Result<Release> {
    let mut response = http.get(manifest::FEED, AsyncBody::default(), true).await?;
    if !response.status().is_success() {
        bail!("the release host answered {}", response.status());
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .take(manifest::MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > manifest::MAX_MANIFEST_BYTES {
        bail!("the manifest is larger than 64 KiB");
    }
    manifest::parse(&bytes, &platform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::oneshot;
    use futures::future::BoxFuture;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const MAC: &str = "macos-aarch64";

    fn manifest(version: &str) -> String {
        format!(
            r#"{{"version": "{version}", "platforms": {{"{MAC}": {{
                "url": "https://github.com/ModernRelay/omnigraph-studio/releases/download/v{version}/Omnigraph-Studio-{version}-arm64.app.tar.gz",
                "signature": "c2ln", "format": "app"}}}}}}"#
        )
    }

    /// An update archive whose bundle says `version`.
    fn archive(version: &str) -> Vec<u8> {
        let plist = format!(
            "<plist><dict><key>CFBundleShortVersionString</key><string>{version}</string></dict></plist>"
        );
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(plist.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(
            &mut header,
            "Omnigraph Studio.app/Contents/Info.plist",
            plist.as_bytes(),
        )
        .unwrap();
        let mut gz = tar.into_inner().unwrap();
        gz.flush().unwrap();
        gz.finish().unwrap()
    }

    /// A release host answering every request with `body`, counting the requests.
    fn host(
        body: impl Fn() -> String + Send + Sync + 'static,
    ) -> (Arc<dyn HttpClient>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let body = Arc::new(body);
        let http = FakeHttpClient::create(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            let body = body();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        (http, count)
    }

    /// An installer that serves `archive`, or refuses, and records what it installed. A
    /// download given a `gate` waits for it after reporting 50 %.
    #[derive(Default)]
    struct FakeInstaller {
        archive: Mutex<Option<Vec<u8>>>,
        refuse: Mutex<Option<InstallError>>,
        gate: Mutex<Option<oneshot::Receiver<()>>>,
        downloads: AtomicUsize,
        installed: Mutex<Vec<PathBuf>>,
    }

    impl Installer for FakeInstaller {
        fn download(
            &self,
            _job: &Job,
            progress: Box<dyn Fn(u8) + Send>,
        ) -> BoxFuture<'static, Result<Vec<u8>, InstallError>> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let result = match self.refuse.lock().unwrap().clone() {
                Some(error) => Err(error),
                None => Ok(self.archive.lock().unwrap().clone().unwrap_or_default()),
            };
            let gate = self.gate.lock().unwrap().take();
            progress(50);
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.await.ok();
                }
                result
            })
        }

        fn install(
            &self,
            job: &Job,
            _bytes: Vec<u8>,
        ) -> BoxFuture<'static, Result<(), InstallError>> {
            self.installed.lock().unwrap().push(job.bundle.clone());
            Box::pin(async { Ok(()) })
        }
    }

    fn options(platform: &str, bundle: &Path) -> Options {
        Options {
            running: Version::new(0, 2, 1),
            platform: Some(platform.into()),
            release_build: true,
            bundle: Some(bundle.to_path_buf()),
            public_key: Some("a-key".into()),
        }
    }

    fn updater(
        options: Options,
        http: Arc<dyn HttpClient>,
        installer: Arc<FakeInstaller>,
        cx: &mut TestAppContext,
    ) -> Entity<Updater> {
        cx.new(|cx| Updater::new(options, http, installer, cx))
    }

    fn state(updater: &Entity<Updater>, cx: &mut TestAppContext) -> UpdateState {
        updater.read_with(cx, |updater, _| updater.state().clone())
    }

    fn bundle() -> tempfile::TempDir {
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir(folder.path().join("Omnigraph Studio.app")).unwrap();
        folder
    }

    #[gpui::test]
    async fn a_release_build_checks_half_a_minute_after_launch_then_hourly(
        cx: &mut TestAppContext,
    ) {
        let folder = bundle();
        let (http, requests) = host(|| manifest("0.2.1"));
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            Arc::default(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.set_automatic(true, cx));
        cx.executor().advance_clock(Duration::from_secs(29));
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            state(&updater, cx),
            UpdateState::Idle,
            "a background check is silent"
        );
        cx.executor().advance_clock(Duration::from_secs(60 * 60));
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn with_automatic_updates_off_studio_checks_only_when_asked(cx: &mut TestAppContext) {
        let folder = bundle();
        let (http, requests) = host(|| manifest("0.2.1"));
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            Arc::default(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.set_automatic(false, cx));
        cx.executor()
            .advance_clock(Duration::from_secs(2 * 60 * 60));
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(state(&updater, cx), UpdateState::UpToDate);
        cx.executor().advance_clock(SAID_FOR);
        cx.run_until_parked();
        assert_eq!(state(&updater, cx), UpdateState::Idle);
    }

    #[gpui::test]
    async fn a_development_build_never_checks(cx: &mut TestAppContext) {
        let folder = bundle();
        let (http, requests) = host(|| manifest("0.3.0"));
        let mut options = options(MAC, &folder.path().join("Omnigraph Studio.app"));
        options.release_build = false;
        let updater = updater(options, http, Arc::default(), cx);
        updater.update(cx, |updater, cx| updater.set_automatic(true, cx));
        cx.executor()
            .advance_clock(Duration::from_secs(2 * 60 * 60));
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(state(&updater, cx), UpdateState::Off);
    }

    #[gpui::test]
    async fn a_newer_version_is_downloaded_checked_and_installed(cx: &mut TestAppContext) {
        let folder = bundle();
        let app = folder.path().join("Omnigraph Studio.app");
        let (http, _) = host(|| manifest("0.3.0"));
        let installer = Arc::new(FakeInstaller::default());
        *installer.archive.lock().unwrap() = Some(archive("0.3.0"));
        let (open, gate) = oneshot::channel();
        *installer.gate.lock().unwrap() = Some(gate);
        let updater = updater(options(MAC, &app), http, installer.clone(), cx);
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        assert_eq!(
            state(&updater, cx),
            UpdateState::Downloading {
                version: Version::new(0, 3, 0),
                percent: Some(50)
            }
        );
        open.send(()).unwrap();
        cx.run_until_parked();
        assert_eq!(
            state(&updater, cx),
            UpdateState::Ready {
                version: Version::new(0, 3, 0)
            }
        );
        assert_eq!(*installer.installed.lock().unwrap(), [app]);
    }

    #[gpui::test]
    async fn an_update_whose_bundle_says_another_version_is_not_installed(cx: &mut TestAppContext) {
        let folder = bundle();
        let (http, _) = host(|| manifest("0.3.0"));
        let installer = Arc::new(FakeInstaller::default());
        *installer.archive.lock().unwrap() = Some(archive("0.2.9"));
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            installer.clone(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        let UpdateState::Available { version, reason } = state(&updater, cx) else {
            panic!("{:?}", state(&updater, cx));
        };
        assert_eq!(version, Version::new(0, 3, 0));
        assert_eq!(
            reason.as_deref(),
            Some("The 0.3.0 download contains 0.2.9 and was not installed.")
        );
        assert!(installer.installed.lock().unwrap().is_empty());
    }

    #[gpui::test]
    async fn a_refused_download_is_not_fetched_again_for_that_version(cx: &mut TestAppContext) {
        let folder = bundle();
        let (http, _) = host(|| manifest("0.3.0"));
        let installer = Arc::new(FakeInstaller::default());
        *installer.refuse.lock().unwrap() =
            Some(InstallError::Refused("signature mismatch".into()));
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            installer.clone(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.set_automatic(true, cx));
        cx.executor().advance_clock(FIRST_CHECK);
        cx.run_until_parked();
        let UpdateState::Available { reason, .. } = state(&updater, cx) else {
            panic!("{:?}", state(&updater, cx));
        };
        assert_eq!(
            reason.as_deref(),
            Some("The 0.3.0 download isn't signed for Omnigraph Studio and was not installed.")
        );
        cx.executor().advance_clock(CHECK_EVERY);
        cx.run_until_parked();
        assert_eq!(installer.downloads.load(Ordering::SeqCst), 1);
    }

    #[gpui::test]
    async fn a_failed_download_waits_for_the_next_check(cx: &mut TestAppContext) {
        let folder = bundle();
        let (http, _) = host(|| manifest("0.3.0"));
        let installer = Arc::new(FakeInstaller::default());
        *installer.refuse.lock().unwrap() = Some(InstallError::Failed("connection reset".into()));
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            installer.clone(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.set_automatic(true, cx));
        cx.executor().advance_clock(FIRST_CHECK);
        cx.run_until_parked();
        assert_eq!(
            state(&updater, cx),
            UpdateState::Idle,
            "said only when asked"
        );
        cx.executor().advance_clock(CHECK_EVERY);
        cx.run_until_parked();
        assert_eq!(installer.downloads.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn a_bundle_studio_cannot_replace_offers_the_download(cx: &mut TestAppContext) {
        let (http, _) = host(|| manifest("0.3.0"));
        let installer = Arc::new(FakeInstaller::default());
        let updater = updater(
            options(
                MAC,
                Path::new("/Volumes/Omnigraph Studio/Omnigraph Studio.app"),
            ),
            http,
            installer.clone(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        let UpdateState::Available { reason, .. } = state(&updater, cx) else {
            panic!("{:?}", state(&updater, cx));
        };
        assert_eq!(
            reason.as_deref(),
            Some("Move Omnigraph Studio to Applications to update it.")
        );
        assert_eq!(installer.downloads.load(Ordering::SeqCst), 0);
    }

    #[gpui::test]
    async fn linux_and_a_build_without_the_update_key_only_tell(cx: &mut TestAppContext) {
        let folder = bundle();
        let app = folder.path().join("Omnigraph Studio.app");
        let mut no_key = options(MAC, &app);
        no_key.public_key = None;
        for options in [options("linux-x86_64", &app), no_key] {
            let (http, _) = host(|| manifest("0.3.0"));
            let installer = Arc::new(FakeInstaller::default());
            let updater = updater(options, http, installer.clone(), cx);
            updater.update(cx, |updater, cx| updater.check(cx));
            cx.run_until_parked();
            assert_eq!(
                state(&updater, cx),
                UpdateState::Available {
                    version: Version::new(0, 3, 0),
                    reason: None
                }
            );
            assert_eq!(installer.downloads.load(Ordering::SeqCst), 0);
        }
    }

    #[gpui::test]
    async fn a_check_that_fails_is_said_only_when_asked(cx: &mut TestAppContext) {
        let folder = bundle();
        let (http, _) = host(|| "<html>Not Found</html>".into());
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            Arc::default(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.set_automatic(true, cx));
        cx.executor().advance_clock(FIRST_CHECK);
        cx.run_until_parked();
        assert_eq!(state(&updater, cx), UpdateState::Idle);
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        let UpdateState::Failed { message } = state(&updater, cx) else {
            panic!("{:?}", state(&updater, cx));
        };
        assert!(
            message.starts_with("Couldn't check for updates: "),
            "{message}"
        );
    }

    #[gpui::test]
    async fn a_manifest_over_its_cap_is_refused(cx: &mut TestAppContext) {
        let folder = bundle();
        let (http, _) = host(|| format!("{} {}", manifest("0.3.0"), " ".repeat(64 * 1024)));
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            Arc::default(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        assert!(matches!(state(&updater, cx), UpdateState::Failed { .. }));
    }

    #[gpui::test]
    async fn a_host_that_does_not_answer_is_given_up_on(cx: &mut TestAppContext) {
        let folder = bundle();
        let http = FakeHttpClient::create(|_| futures::future::pending());
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            Arc::default(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        assert_eq!(state(&updater, cx), UpdateState::Checking);
        cx.executor().advance_clock(CHECK_TIMEOUT);
        cx.run_until_parked();
        assert!(matches!(state(&updater, cx), UpdateState::Failed { .. }));
    }

    #[gpui::test]
    async fn asking_during_a_download_joins_it(cx: &mut TestAppContext) {
        let folder = bundle();
        let (http, requests) = host(|| manifest("0.3.0"));
        let installer = Arc::new(FakeInstaller::default());
        *installer.archive.lock().unwrap() = Some(archive("0.3.0"));
        let (open, gate) = oneshot::channel();
        *installer.gate.lock().unwrap() = Some(gate);
        let updater = updater(
            options(MAC, &folder.path().join("Omnigraph Studio.app")),
            http,
            installer.clone(),
            cx,
        );
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        open.send(()).unwrap();
        cx.run_until_parked();
        assert!(matches!(state(&updater, cx), UpdateState::Ready { .. }));
        assert_eq!(installer.downloads.load(Ordering::SeqCst), 1);
    }

    #[gpui::test]
    async fn the_setting_starts_and_stops_the_schedule(cx: &mut TestAppContext) {
        cx.update(studio_settings::init_test);
        let folder = bundle();
        let (http, requests) = host(|| manifest("0.2.1"));
        cx.update(|cx| {
            init_with(
                options(MAC, &folder.path().join("Omnigraph Studio.app")),
                http,
                Arc::new(FakeInstaller::default()),
                cx,
            )
        });
        assert!(cx.update(|cx| Updater::global(cx).is_some()));
        cx.executor().advance_clock(FIRST_CHECK);
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 1, "on by default");
        cx.update(|cx| {
            cx.update_global::<studio_settings::StudioSettings, _>(|settings, _| {
                settings.studio.auto_update = false;
            })
        });
        cx.executor().advance_clock(2 * CHECK_EVERY);
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 1, "off stops the schedule");
    }

    #[test]
    fn an_empty_key_file_means_no_key() {
        assert_eq!(key_from(""), None);
        assert_eq!(key_from(" \n"), None);
        assert_eq!(key_from("dW50cnVzdGVk\n").as_deref(), Some("dW50cnVzdGVk"));
    }

    #[gpui::test]
    async fn the_release_notes_and_download_follow_the_newer_version(cx: &mut TestAppContext) {
        let (http, _) = host(|| manifest("0.3.0"));
        let updater = updater(
            options("linux-x86_64", Path::new("/opt/studio")),
            http,
            Arc::default(),
            cx,
        );
        updater.read_with(cx, |updater, _| {
            assert!(
                updater
                    .release_notes_url()
                    .ends_with("/releases/tag/v0.2.1")
            );
        });
        updater.update(cx, |updater, cx| updater.check(cx));
        cx.run_until_parked();
        updater.read_with(cx, |updater, _| {
            assert!(
                updater
                    .release_notes_url()
                    .ends_with("/releases/tag/v0.3.0")
            );
            assert!(
                updater
                    .download_url()
                    .ends_with("/omnigraph-studio-linux-x86_64.tar.gz")
            );
        });
    }
}
