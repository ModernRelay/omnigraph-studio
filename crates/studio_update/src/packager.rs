//! The release installer: `cargo-packager-updater` 0.2.3, whose blocking calls run on a thread
//! of their own. Studio reads the manifest itself and builds the crate's `Update` from it, so
//! the crate's `check` (no size limit, and Debian's certificate paths set for the whole process
//! on Linux) never runs (spec 017).

use std::cell::Cell;
use std::time::Duration;

use cargo_packager_updater::http::{HeaderMap, HeaderValue, header::USER_AGENT};
use cargo_packager_updater::{Config, Error, Update, UpdateFormat};
use futures::channel::oneshot;
use futures::future::BoxFuture;

use crate::{InstallError, Installer, Job};

/// How long a download may take in all.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Installs with `cargo-packager-updater`.
pub struct PackagerInstaller;

impl Installer for PackagerInstaller {
    fn download(
        &self,
        job: &Job,
        progress: Box<dyn Fn(u8) + Send>,
    ) -> BoxFuture<'static, Result<Vec<u8>, InstallError>> {
        let update = update(job);
        on_thread(move || {
            let update = update?;
            let received = Cell::new(0u64);
            let said = Cell::new(None);
            update
                .download_extended(
                    |chunk, total| {
                        received.set(received.get() + chunk as u64);
                        let Some(total) = total.filter(|total| *total > 0) else {
                            return;
                        };
                        let percent = (received.get() * 100 / total).min(100) as u8;
                        if said.replace(Some(percent)) != Some(percent) {
                            progress(percent);
                        }
                    },
                    || {},
                )
                .map_err(classify)
        })
    }

    fn install(&self, job: &Job, bytes: Vec<u8>) -> BoxFuture<'static, Result<(), InstallError>> {
        let update = update(job);
        on_thread(move || {
            update?
                .install(bytes)
                .map_err(|error| InstallError::Failed(error.to_string()))
        })
    }
}

/// The crate's `Update` for a job: a macOS bundle (`format: app`) put where Studio runs.
fn update(job: &Job) -> Result<Update, InstallError> {
    let download_url = job
        .update
        .url
        .parse()
        .map_err(|error| InstallError::Failed(format!("the update's address: {error}")))?;
    let mut headers = HeaderMap::new();
    let agent = format!("Omnigraph-Studio/{}", job.running);
    if let Ok(agent) = HeaderValue::from_str(&agent) {
        headers.insert(USER_AGENT, agent);
    }
    Ok(Update {
        config: Config {
            pubkey: job.public_key.clone(),
            ..Config::default()
        },
        body: None,
        current_version: job.running.to_string(),
        version: job.version.to_string(),
        date: None,
        target: "macos".into(),
        extract_path: job.bundle.clone(),
        download_url,
        signature: job.update.signature.clone(),
        timeout: Some(DOWNLOAD_TIMEOUT),
        headers,
        format: UpdateFormat::App,
    })
}

/// A signature that does not decode or does not verify refuses the file; anything else is a
/// download that did not finish.
fn classify(error: Error) -> InstallError {
    match error {
        Error::Minisign(_) | Error::Base64(_) | Error::SignatureUtf8(_) => {
            InstallError::Refused(error.to_string())
        }
        error => InstallError::Failed(error.to_string()),
    }
}

/// Runs a blocking call on a thread of its own, so no executor thread waits on the network.
fn on_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, InstallError> + Send + 'static,
) -> BoxFuture<'static, Result<T, InstallError>> {
    let (sender, receiver) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("studio-update".into())
        .spawn(move || {
            sender.send(work()).ok();
        });
    Box::pin(async move {
        spawned.map_err(|error| InstallError::Failed(error.to_string()))?;
        receiver
            .await
            .unwrap_or_else(|_| Err(InstallError::Failed("the update stopped".into())))
    })
}
