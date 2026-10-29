//! Every server Studio knows: the server targets the Omnigraph CLI lists, and, for development
//! and tests, one from the environment. Studio keeps no server list or token of its own
//! (Omnigraph RFC 1017): a server is added as a CLI target, through the CLI
//! ([`ConnectionRegistry::add_target`]). The targets are listed off the main thread, at start and
//! whenever the CLI's files change: a start shows its windows at once, and they find the CLI's
//! servers once the first listing lands ([`ConnectionRegistry::when_listed`]).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};

use crate::cli_config::{self, CliAuth, CliServer, CliTargets, CliTransport, FileStamp, Platform};
use crate::cli_tokens::{self, CliTokenSource, Token, TokenSource};
use crate::connection::{Connection, ServerEntry, ServerSource};
use crate::sign_in::{self, PlatformKeychain, SignInContext};

/// How often the CLI's files are checked for a change, as Studio's settings file is.
const OPERATOR_POLL: Duration = Duration::from_secs(2);

/// Makes the HTTP client for a CLI target whose `transport` names a proxy or its own certificate
/// authority.
type TransportHttp =
    Arc<dyn Fn(&CliTransport) -> anyhow::Result<Arc<dyn HttpClient>> + Send + Sync>;

/// The clients made for a listing's transports ([`prepare_transports`]), or why one could not be.
type Prepared = Vec<(CliTransport, Result<Arc<dyn HttpClient>, String>)>;

/// Makes the token source of a CLI server whose token the CLI gives; none for an anonymous one.
type TokenSources = Arc<dyn Fn(&CliServer) -> Option<Arc<dyn TokenSource>> + Send + Sync>;

/// Lists the targets of an operator home with the first CLI Studio can drive of the places
/// given, as they land on the platform given ([`cli_config::read_targets`]); run off the main
/// thread.
type Lister = Arc<
    dyn Fn(Option<PathBuf>, Vec<PathBuf>, Platform) -> BoxFuture<'static, CliTargets> + Send + Sync,
>;

/// The graph ids opened on each CLI target, by the target's name, which Studio remembers so the
/// Open dialog offers them again when a token may not list the server's graphs. It names no
/// server: the CLI's targets are the only ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct KnownGraphs {
    #[serde(default)]
    targets: BTreeMap<String, Vec<String>>,
}

/// What a CLI server was when the CLI's targets were last listed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CliKept {
    /// Its revision ([`CliServer::revision`]): one that changes asks the CLI for its token again.
    revision: String,
    /// How its requests reach it, which its connection's client was made with.
    transport: CliTransport,
}

pub struct ConnectionRegistry {
    pub connections: Vec<Entity<Connection>>,
    /// Why the environment's server could not be added at start, which stays one of the
    /// [`Self::notices`] for the run.
    environment_notice: Option<String>,
    /// What the person is told about the servers: the environment's server not added, then
    /// what the CLI's targets could not give when last listed. One list, each said as it
    /// happened.
    notices: Vec<String>,
    http: Arc<dyn HttpClient>,
    transported: TransportHttp,
    token_sources: TokenSources,
    lister: Lister,
    /// Whether the CLI gives Studio a token here: the system Studio runs on, but in a test. Where
    /// it gives none (Windows), a target that takes a token is refused as it is listed, and Add
    /// server takes no token.
    platform: Platform,
    /// The operator home the CLI reads (`$OMNIGRAPH_HOME`, else `~/.omnigraph`).
    operator_dir: Option<PathBuf>,
    /// The files whose change lists the targets again (the operator file and the token files
    /// the targets name), and their stamp as the last listing saw them.
    watched: Vec<PathBuf>,
    operator_stamp: Vec<Option<FileStamp>>,
    /// The places Studio looks for the CLI, and their stamp: a CLI installed, upgraded or
    /// removed, or another named in the settings, lists the targets and asks every CLI server's
    /// token again.
    cli_places: Vec<PathBuf>,
    cli_stamp: Vec<Option<FileStamp>>,
    /// Follows the settings' `omnigraph_cli`, the CLI Studio looks for first
    /// ([`Self::follow_cli_setting`]).
    _cli_setting: Option<gpui::Subscription>,
    /// Each CLI server as last listed, by its address.
    cli_kept: HashMap<String, CliKept>,
    /// The CLI's targets have been listed once since Studio started; until then the CLI's
    /// servers are not known, and what looks for one waits ([`Self::when_listed`]).
    listed: bool,
    listing_waiters: Vec<oneshot::Sender<()>>,
    /// The graph ids remembered for each CLI target, as last read or saved.
    known: KnownGraphs,
    /// What a CLI server's connection signs in with when its server asks (spec 016): `None` in
    /// a test that does not set it ([`Self::set_sign_in_for_tests`]).
    sign_in: Option<SignInContext>,
    /// Whether the remembered graphs are written to the data directory: Studio's, not a
    /// test's.
    saving: bool,
    _watch: Option<Task<()>>,
}

struct GlobalRegistry(Entity<ConnectionRegistry>);

impl Global for GlobalRegistry {}

fn known_graphs_path() -> PathBuf {
    studio_settings::data_dir().join("known_graphs.json")
}

/// The graph ids remembered at `path`: none when the file is not there or cannot be read, which
/// the next save replaces.
fn load_known_graphs(path: &Path) -> KnownGraphs {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// A target name for a server added at `url` with no name given: its host, each character a
/// target name cannot hold made `-`, starting with a letter as a name must.
fn name_for(url: &str) -> String {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default();
    let name: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    if name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        name
    } else {
        format!("server-{name}")
    }
}

/// Whether `name` is a CLI target name: `[A-Za-z][A-Za-z0-9_.-]*`.
fn valid_target_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// The HTTP client for a CLI target whose `transport` names a proxy (`mode: url`) or its own
/// certificate authority (`tls.ca`), as the Omnigraph CLI makes its own (`operator_transport.rs`):
/// with `append` the bundle's certificates are trusted with the system's roots, with `replace`
/// alone. The reqwest Zed pins has no bundled web roots to leave out. Requests go through the
/// proxy, else as the system's settings say; Zed's client also leaves out the hosts in
/// `NO_PROXY`.
pub(crate) fn transport_client(transport: &CliTransport) -> anyhow::Result<Arc<dyn HttpClient>> {
    let proxy = transport
        .proxy
        .as_deref()
        .map(http_client::Url::parse)
        .transpose()?;
    let Some(roots) = &transport.roots else {
        return Ok(Arc::new(
            reqwest_client::ReqwestClient::proxy_and_user_agent(proxy, crate::USER_AGENT)?,
        ));
    };
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static(crate::USER_AGENT),
    );
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(30))
        .tls_built_in_root_certs(!roots.replace)
        .tls_built_in_native_certs(!roots.replace);
    for certificate in reqwest::Certificate::from_pem_bundle(&roots.bundle)? {
        builder = builder.add_root_certificate(certificate);
    }
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?.no_proxy(reqwest::NoProxy::from_env()));
    }
    Ok(Arc::new(reqwest_client::ReqwestClient::from(
        builder.build()?,
    )))
}

/// The clients the transports of `targets` need that `held` does not hold already, made off the
/// main thread: one that trusts the system's roots too reads them from the system's store,
/// which can take a while. A failure is kept as its message, said when the target is applied.
fn prepare_transports(
    targets: &CliTargets,
    held: &[CliTransport],
    make: &TransportHttp,
) -> Prepared {
    let mut prepared: Prepared = Vec::new();
    for server in targets.servers.iter().flatten() {
        let transport = &server.transport;
        if *transport == CliTransport::default()
            || held.contains(transport)
            || prepared.iter().any(|(made, _)| made == transport)
        {
            continue;
        }
        prepared.push((
            transport.clone(),
            make(transport).map_err(|error| error.to_string()),
        ));
    }
    prepared
}

/// What a target's `transport` names, after "CLI target 'name'".
fn describe_transport(transport: &CliTransport) -> String {
    let roots = transport
        .roots
        .as_ref()
        .map(|roots| format!("its own certificate authority ({})", roots.file.display()));
    let proxy = transport
        .proxy
        .as_ref()
        .map(|proxy| format!("the proxy {proxy}"));
    match (roots, proxy) {
        (Some(roots), Some(proxy)) => format!("trusts {roots} and goes through {proxy}"),
        (Some(roots), None) => format!("trusts {roots}"),
        // Only a target with a transport is described: one with neither uses Studio's client.
        (None, proxy) => format!("names {}", proxy.unwrap_or_default()),
    }
}

/// The token source of a CLI server: `credentials export` for its target, with the CLI Studio
/// finds where it looks for one.
fn cli_token_source(server: &CliServer) -> Option<Arc<dyn TokenSource>> {
    match &server.auth {
        CliAuth::Anonymous => None,
        CliAuth::Token(credential) => {
            Some(Arc::new(CliTokenSource::for_server(server, credential)))
        }
    }
}

/// The targets of the operator home in `dir`, as the CLI lists them, landed on `platform`.
fn list_with_the_cli(
    dir: Option<PathBuf>,
    places: Vec<PathBuf>,
    platform: Platform,
) -> BoxFuture<'static, CliTargets> {
    async move { cli_config::read_targets(dir.as_deref(), &places, platform).await }.boxed()
}

impl ConnectionRegistry {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalRegistry>().0.clone()
    }

    /// The registry, once Studio has set one up (not in every test).
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalRegistry>()
            .map(|global| global.0.clone())
    }

    pub(crate) fn init(http: Arc<dyn HttpClient>, cx: &mut App) {
        let registry = cx.new(|cx| Self::load(http, cx));
        cx.set_global(GlobalRegistry(registry));
    }

    /// Sets up a registry with no CLI and no servers, whose targets count as listed, for the
    /// tests of crates that look for a server in it. Its servers answer a health check and a
    /// graph list (of none), and every other request with a 404; it returns the host of each
    /// request sent, in order. Servers join it with [`Self::add_server_for_tests`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn init_for_tests(cx: &mut App) -> (Entity<Self>, Arc<std::sync::Mutex<Vec<String>>>) {
        let asked: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log = asked.clone();
        let http = http_client::FakeHttpClient::create(move |request| {
            let host = request.uri().host().unwrap_or_default().to_owned();
            let path = request.uri().path().to_owned();
            log.lock().unwrap().push(host);
            async move {
                let (status, body) = if path.ends_with("/healthz") {
                    (
                        200,
                        r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#,
                    )
                } else if path.ends_with("/graphs") {
                    (200, r#"{"graphs":[]}"#)
                } else {
                    (404, r#"{"error":"not found","code":"not_found"}"#)
                };
                Ok(http_client::Response::builder()
                    .status(status)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(http_client::AsyncBody::from(body))
                    .unwrap())
            }
        });
        let registry = cx.new(|_| Self::empty(http, None));
        cx.set_global(GlobalRegistry(registry.clone()));
        (registry, asked)
    }

    /// Adds a server named `name` at `url`, given `token`, as the environment's server is.
    #[cfg(any(test, feature = "test-support"))]
    pub fn add_server_for_tests(
        &mut self,
        name: &str,
        url: &str,
        token: Option<String>,
        cx: &mut Context<Self>,
    ) -> Entity<Connection> {
        let entry = ServerEntry {
            name: name.into(),
            url: url.into(),
            source: ServerSource::Environment,
            known_graphs: Vec::new(),
        };
        self.add_connection(entry, token, cx)
            .expect("a usable server address")
    }

    /// Holds the CLI's targets as not listed yet (`false`), as at a start, or lists them now
    /// (`true`), so what waited for them goes on: for the tests of what a start opens.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_listed_for_tests(&mut self, listed: bool, cx: &mut Context<Self>) {
        if listed {
            self.finish_listing(cx);
        } else {
            self.listed = false;
        }
    }

    /// Takes `platform` as the system Studio runs on, for the tests of what Studio does where
    /// the CLI gives it no token (Windows).
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_sign_in_for_tests(&mut self, context: SignInContext) {
        self.sign_in = Some(context);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_platform_for_tests(&mut self, platform: Platform) {
        self.platform = platform;
    }

    /// Whether the CLI gives Studio a token here: the system Studio runs on, but in a test. Add
    /// server's token field follows it.
    pub fn platform(&self) -> Platform {
        self.platform
    }

    fn load(http: Arc<dyn HttpClient>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::empty(http, cli_config::operator_dir());
        this.transported = Arc::new(transport_client);
        this.known = load_known_graphs(&known_graphs_path());
        this.saving = true;
        this.sign_in = Some(SignInContext {
            elsewhere: this.http.clone(),
            keychain: PlatformKeychain::start(cx),
            turns: sign_in::KeychainTurns::default(),
            identities: sign_in::studio_identities(),
        });

        if let Ok(url) = std::env::var("OMNIGRAPH_STUDIO_SERVER") {
            let url = graph_client::server_root(&url).unwrap_or(url);
            let token = std::env::var("OMNIGRAPH_STUDIO_TOKEN").ok();
            let graphs = std::env::var("OMNIGRAPH_STUDIO_GRAPH")
                .ok()
                .into_iter()
                .collect();
            let entry = ServerEntry {
                name: "environment".into(),
                url,
                source: ServerSource::Environment,
                known_graphs: graphs,
            };
            if let Err(error) = this.add_connection(entry, token, cx) {
                this.environment_notice = Some(format!(
                    "The server OMNIGRAPH_STUDIO_SERVER names was not added: {error}"
                ));
                this.notices = this.environment_notice.iter().cloned().collect();
            }
        }

        this.token_sources = Arc::new(cli_token_source);
        this.follow_cli_setting(cx);
        this.start_listing(cx);
        this
    }

    /// Takes the CLI the settings name (`omnigraph_cli`) as the one Studio looks for first, for
    /// its servers and for the agent ([`cli_tokens::candidates`]), now and whenever the
    /// settings change. A change moves the places Studio looks, which the next check of the
    /// CLI's files sees: it lists the targets with that CLI and asks every token again.
    fn follow_cli_setting(&mut self, cx: &mut Context<Self>) {
        self.take_cli_setting(cx);
        self._cli_setting = Some(cx.observe_global::<studio_settings::StudioSettings>(
            |this, cx| this.take_cli_setting(cx),
        ));
    }

    fn take_cli_setting(&mut self, cx: &mut Context<Self>) {
        let named = cx
            .try_global::<studio_settings::StudioSettings>()
            .and_then(|settings| settings.omnigraph_cli.clone());
        cli_tokens::set_configured(named.as_deref());
        self.cli_places = cli_tokens::candidates();
    }

    fn empty(http: Arc<dyn HttpClient>, operator_dir: Option<PathBuf>) -> Self {
        let same = http.clone();
        Self {
            connections: Vec::new(),
            environment_notice: None,
            notices: Vec::new(),
            http,
            transported: Arc::new(move |_| Ok(same.clone())),
            token_sources: Arc::new(|_| None),
            lister: Arc::new(list_with_the_cli),
            platform: Platform::CURRENT,
            operator_dir,
            watched: Vec::new(),
            operator_stamp: Vec::new(),
            cli_places: Vec::new(),
            cli_stamp: Vec::new(),
            _cli_setting: None,
            cli_kept: HashMap::new(),
            listed: true,
            listing_waiters: Vec::new(),
            known: KnownGraphs::default(),
            sign_in: None,
            saving: false,
            _watch: None,
        }
    }

    /// What the person is told about the servers, oldest first: the environment's server when
    /// it could not be added, then what the CLI's targets could not give when last listed (no
    /// CLI Studio can drive, a configuration the CLI refuses, a target Studio does not connect
    /// to, the other targets on an address). Each says what happened, and the workspace shows
    /// each new one in a toast.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Whether the CLI's targets have been listed since Studio started, so its servers are
    /// known.
    pub fn listed(&self) -> bool {
        self.listed
    }

    /// Resolves once the CLI's targets have been listed since Studio started, at once when
    /// they have been: the windows a start reopens, a link and the Open dialog look for the
    /// CLI's servers then. A start lists them off the main thread, so its windows show first.
    pub fn when_listed(&mut self) -> impl Future<Output = ()> + use<> {
        let waiting = (!self.listed).then(|| {
            let (listed, waiting) = oneshot::channel();
            self.listing_waiters.push(listed);
            waiting
        });
        async move {
            if let Some(waiting) = waiting {
                waiting.await.ok();
            }
        }
    }

    /// Lists the CLI's targets off the main thread, at once and then whenever their files
    /// change ([`Self::watch_cli_targets`]); until the first listing lands, the registry is not
    /// [`Self::listed`].
    fn start_listing(&mut self, cx: &mut Context<Self>) {
        self.listed = false;
        self.watched = self
            .operator_dir
            .iter()
            .map(|dir| cli_config::config_path(dir))
            .collect();
        self.operator_stamp = Vec::new();
        self.watch_cli_targets(cx);
    }

    /// The first listing landed: what waited for it goes on.
    fn finish_listing(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.listed, true) {
            return;
        }
        for listed in self.listing_waiters.drain(..) {
            listed.send(()).ok();
        }
        cx.notify();
    }

    /// Keeps the files `targets` names as the watched ones, with the stamps the listing took
    /// of them: the operator file's just before the CLI read it, a token file's as its target's
    /// revision holds it. A file named for the first time is not listed again for being new,
    /// only when it changes after the listing saw it.
    fn keep_stamp(&mut self, targets: &CliTargets) {
        let (watched, stamps) = targets.watched.iter().cloned().unzip();
        self.watched = watched;
        self.operator_stamp = stamps;
    }

    /// Lists the CLI's targets at once, then again whenever the operator file, a token file a
    /// target names, or the CLI where Studio looks for it changes, so a target added there, a
    /// changed address or a rotated token reaches Studio without a restart; a CLI installed,
    /// upgraded or removed also asks every CLI server's token again. Each look and each listing
    /// runs off the main thread. The CLI's secure store cannot be watched: a token kept there
    /// is asked for again on connect, after a 401 and on Refresh.
    fn watch_cli_targets(&mut self, cx: &mut Context<Self>) {
        let dir = self.operator_dir.clone();
        let lister = self.lister.clone();
        self._watch = Some(cx.spawn(async move |this, cx| {
            let mut first = true;
            loop {
                if !std::mem::take(&mut first) {
                    cx.background_executor().timer(OPERATOR_POLL).await;
                }
                let Ok((watched, places, listing, platform)) = this.update(cx, |this, _| {
                    (
                        this.watched.clone(),
                        this.cli_places.clone(),
                        !this.listed,
                        this.platform,
                    )
                }) else {
                    break;
                };
                let (stamp, cli_stamp, places) = cx
                    .background_executor()
                    .spawn(async move {
                        (
                            cli_config::stamp(&watched),
                            cli_config::stamp(&places),
                            places,
                        )
                    })
                    .await;
                let Ok((listed_files, cli_moved)) = this.update(cx, |this, _| {
                    (this.operator_stamp != stamp, this.cli_stamp != cli_stamp)
                }) else {
                    break;
                };
                if !listing && !listed_files && !cli_moved {
                    continue;
                }
                let Ok((held, make)) = this.update(cx, |this, _| this.transport_maker()) else {
                    break;
                };
                let lister = lister.clone();
                let dir = dir.clone();
                let (targets, prepared) = cx
                    .background_executor()
                    .spawn(async move {
                        let targets = lister(dir, places, platform).await;
                        let prepared = prepare_transports(&targets, &held, &make);
                        (targets, prepared)
                    })
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        // A CLI that moved since the start's listing asks every token again.
                        let asks_again = cli_moved && !listing;
                        this.cli_stamp = cli_stamp;
                        this.keep_stamp(&targets);
                        let changed = this.apply_cli_targets(targets, prepared, cx);
                        if asks_again {
                            this.ask_cli_tokens_again(cx);
                        }
                        if changed {
                            this.save(cx);
                            cx.notify();
                        }
                        this.finish_listing(cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
    }

    /// Asks every connection whose token the CLI gives for it again.
    fn ask_cli_tokens_again(&mut self, cx: &mut Context<Self>) {
        for connection in &self.connections {
            connection.update(cx, |connection, cx| connection.refresh_token(cx));
        }
    }

    /// The transports the CLI connections hold, and what makes a client for another, for a
    /// listing to prepare off the main thread ([`prepare_transports`]).
    fn transport_maker(&self) -> (Vec<CliTransport>, TransportHttp) {
        let held = self
            .cli_kept
            .values()
            .map(|kept| kept.transport.clone())
            .collect();
        (held, self.transported.clone())
    }

    /// The client for `transport`: the one prepared beside the listing, else made here (a
    /// listing applied without preparing, as in tests).
    fn transport_http(
        &self,
        transport: &CliTransport,
        prepared: &mut Prepared,
    ) -> Result<Arc<dyn HttpClient>, String> {
        if *transport == CliTransport::default() {
            return Ok(self.http.clone());
        }
        match prepared.iter().position(|(made, _)| made == transport) {
            Some(at) => prepared.swap_remove(at).1,
            None => (self.transported)(transport).map_err(|error| error.to_string()),
        }
    }

    /// Brings the CLI's servers in line with the targets the CLI lists, and says whether
    /// anything changed. An address is one server (spec 003), so a CLI server is its address: a
    /// target renamed on the same endpoint renames the connection in place, which the windows
    /// open on it keep, each graph's saved state staying under the name it was opened with until
    /// it is opened again ([`crate::GraphSession::overrides_key`]); its sign-in, the old name's,
    /// is dropped ([`Connection::rename`]). A new address is a new connection, with the graphs
    /// Studio remembered for its target's name, or for the name at the address it left; an
    /// address no longer listed leaves the list (a window open on it keeps its connection); a
    /// target whose entry or token file changed asks the CLI for its token again; a target's
    /// graph is added. Targets that could not be listed change no server. What could not be
    /// listed or used, a server that cannot be added included, is a change only when it differs
    /// from what was kept.
    fn apply_cli_targets(
        &mut self,
        targets: CliTargets,
        mut prepared: Prepared,
        cx: &mut Context<Self>,
    ) -> bool {
        let CliTargets {
            servers,
            mut errors,
            watched: _,
            managed: _,
        } = targets;
        let Some(servers) = servers else {
            return self.keep_notices(errors);
        };
        let mut changed = false;
        // An address still listed keeps its connection, which a changed transport moves to its
        // new client below. One no longer listed is retired: the windows holding it send
        // nothing more, so none keeps a token or a trust its target no longer names.
        let named: Vec<&String> = servers.iter().map(|server| &server.url).collect();
        let mut left: Vec<ServerEntry> = Vec::new();
        let mut gone = Vec::new();
        self.connections.retain(|connection| {
            let server = &connection.read(cx).server;
            let stays = server.source != ServerSource::Cli || named.contains(&&server.url);
            if !stays {
                left.push(server.clone());
                gone.push(connection.clone());
            }
            stays
        });
        for connection in gone {
            connection.update(cx, |connection, cx| {
                let why = format!(
                    "The CLI no longer lists the target '{}' at {}",
                    connection.server.name, connection.server.url
                );
                connection.retire(why, cx);
            });
        }
        changed |= !left.is_empty();
        self.cli_kept.retain(|url, _| named.contains(&url));
        for server in servers {
            let url = server.url.clone();
            // An address Studio reaches already as the environment's server is that connection:
            // a second would open its graphs twice, and window restore and links would each find
            // either.
            let other = self.connections.iter().find_map(|connection| {
                let entry = &connection.read(cx).server;
                (entry.url == url && entry.source != ServerSource::Cli).then(|| entry.name.clone())
            });
            if let Some(other) = other {
                errors.push(format!(
                    "CLI target '{}' is at {url}, which Studio connects to already as \
                     '{other}'; Studio keeps one connection per address",
                    server.name
                ));
                continue;
            }
            let kept = CliKept {
                revision: server.revision.clone(),
                transport: server.transport.clone(),
            };
            let existing = self.connections.iter().find(|connection| {
                let entry = &connection.read(cx).server;
                entry.source == ServerSource::Cli && entry.url == url
            });
            if let Some(connection) = existing.cloned() {
                let moved = self
                    .cli_kept
                    .get(&url)
                    .is_some_and(|held| held.transport != server.transport);
                if moved {
                    match self.transport_http(&server.transport, &mut prepared) {
                        Ok(http) => connection
                            .update(cx, |connection, cx| connection.replace_http(http, cx)),
                        Err(error) => {
                            let why = format!(
                                "CLI target '{}' {}, which Studio cannot use: {error}",
                                server.name,
                                describe_transport(&server.transport)
                            );
                            errors.push(why.clone());
                            connection.update(cx, |connection, cx| connection.retire(why, cx));
                            self.connections.retain(|held| *held != connection);
                            self.cli_kept.remove(&url);
                            changed = true;
                            continue;
                        }
                    }
                }
                connection.update(cx, |connection, cx| {
                    if connection.server.name != server.name {
                        connection.rename(server.name.clone(), cx);
                        changed = true;
                    }
                    for graph in &server.graphs {
                        if !connection.server.known_graphs.contains(graph) {
                            connection.remember_graph(graph);
                            changed = true;
                        }
                    }
                });
                if self.cli_kept.get(&url) != Some(&kept) {
                    let source = (self.token_sources)(&server);
                    connection.update(cx, |connection, cx| connection.take_token_from(source, cx));
                    self.cli_kept.insert(url, kept);
                    changed = true;
                }
                continue;
            }
            // New: a connection of its own, with the graphs remembered for it.
            let mut graphs = server.graphs.clone();
            let remembered = left
                .iter()
                .find(|entry| entry.name == server.name)
                .map(|entry| entry.known_graphs.clone())
                .or_else(|| self.known.targets.get(&server.name).cloned())
                .unwrap_or_default();
            for graph in remembered {
                if !graphs.contains(&graph) {
                    graphs.push(graph);
                }
            }
            let entry = ServerEntry {
                name: server.name.clone(),
                url: url.clone(),
                source: ServerSource::Cli,
                known_graphs: graphs,
            };
            let http = match self.transport_http(&server.transport, &mut prepared) {
                Ok(http) => http,
                Err(error) => {
                    errors.push(format!(
                        "CLI target '{}' {}, which Studio cannot use: {error}",
                        server.name,
                        describe_transport(&server.transport)
                    ));
                    continue;
                }
            };
            let source = (self.token_sources)(&server);
            match Connection::new(entry, None, http) {
                Ok(connection) => {
                    let connection = connection
                        .with_token_source(source)
                        .with_sign_in(self.sign_in.clone());
                    self.connections.push(cx.new(|_| connection));
                    self.cli_kept.insert(url, kept);
                    changed = true;
                }
                Err(error) => errors.push(format!(
                    "CLI target '{}' cannot be reached: {error}",
                    server.name
                )),
            }
        }
        let notices_changed = self.keep_notices(errors);
        changed || notices_changed
    }

    /// Keeps `errors`, what the CLI's targets could not give, as the notices after the
    /// environment's, and says whether the notices changed.
    fn keep_notices(&mut self, errors: Vec<String>) -> bool {
        let notices: Vec<String> = self
            .environment_notice
            .iter()
            .cloned()
            .chain(errors)
            .collect();
        let changed = notices != self.notices;
        self.notices = notices;
        changed
    }

    fn add_connection(
        &mut self,
        entry: ServerEntry,
        token: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<Entity<Connection>, String> {
        let connection =
            Connection::new(entry, token, self.http.clone()).map_err(|error| error.to_string())?;
        let connection = cx.new(|_| connection);
        self.connections.push(connection.clone());
        Ok(connection)
    }

    /// Adds the server at `url` as the CLI target `name` (its host when none is given), through
    /// the CLI: `target add`, and `credentials set` for a token, which the CLI keeps in its own
    /// secure store. Studio writes neither the CLI's file nor the token. The targets are listed
    /// again at once, and the new target's connection is the answer. An address Studio connects
    /// to already is that connection, and nothing is added. A refusal is the CLI's own words.
    /// Where the CLI gives Studio no token (Windows), a token is refused before the CLI is asked:
    /// Studio could not use the target it would add. The Open dialog disables its token field
    /// there, so this refusal is what stands behind it.
    pub fn add_target(
        &mut self,
        name: String,
        url: String,
        token: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Connection>, String>> {
        let url = match graph_client::server_root(&url) {
            Ok(url) => url,
            Err(error) => return Task::ready(Err(error)),
        };
        if let Some(existing) = self
            .connections
            .iter()
            .find(|connection| connection.read(cx).server.url == url)
            .cloned()
        {
            return Task::ready(Ok(existing));
        }
        let name = match name.trim() {
            "" => name_for(&url),
            name => name.to_owned(),
        };
        if !valid_target_name(&name) {
            return Task::ready(Err(format!(
                "“{name}” cannot be a CLI target's name: it starts with a letter and holds only \
                 letters, digits, `.`, `_` and `-`"
            )));
        }
        let token = token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .map(Token::typed);
        let platform = self.platform;
        if token.is_some() && !platform.exports_tokens() {
            return Task::ready(Err(format!(
                "{}, so Studio on Windows cannot use a server that needs a token yet; leave the \
                 token empty to add one that needs none",
                cli_config::PIPE_ONLY
            )));
        }
        let dir = self.operator_dir.clone();
        let places = self.cli_places.clone();
        let lister = self.lister.clone();
        let (held, make) = self.transport_maker();
        cx.spawn(async move |this, cx| {
            let (added, targets, prepared) = {
                let (name, url) = (name.clone(), url.clone());
                cx.background_executor()
                    .spawn(async move {
                        let added =
                            cli_tokens::add_server_target(&places, &name, &url, token).await;
                        let targets = lister(dir, places, platform).await;
                        let prepared = prepare_transports(&targets, &held, &make);
                        (added, targets, prepared)
                    })
                    .await
            };
            this.update(cx, |this, cx| {
                this.keep_stamp(&targets);
                if this.apply_cli_targets(targets, prepared, cx) {
                    this.save(cx);
                    cx.notify();
                }
                added?;
                this.find(&name, cx).ok_or_else(|| {
                    format!("The CLI added the target '{name}', but does not list it")
                })
            })
            .map_err(|error| error.to_string())?
        })
    }

    /// Replaces a server's token for this run and reconnects: for a CLI server until Refresh or
    /// a change to its target asks the CLI again, since Studio writes none of the CLI's files.
    pub fn update_token(
        &mut self,
        connection: &Entity<Connection>,
        token: String,
        cx: &mut Context<Self>,
    ) {
        let token = token.trim().to_owned();
        connection.update(cx, |connection, cx| connection.set_token(Some(token), cx));
    }

    /// Writes the graph ids remembered for each CLI target to the data directory.
    pub fn save(&mut self, cx: &App) {
        let mut known = self.known.clone();
        for connection in &self.connections {
            let server = &connection.read(cx).server;
            if server.source == ServerSource::Cli {
                known
                    .targets
                    .insert(server.name.clone(), server.known_graphs.clone());
            }
        }
        if known == self.known {
            return;
        }
        self.known = known;
        if !self.saving {
            return;
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.known) {
            studio_settings::write_atomic_in_turn(
                known_graphs_path(),
                text.into_bytes(),
                studio_settings::Access::Private,
            );
        }
    }

    pub fn find(&self, name: &str, cx: &App) -> Option<Entity<Connection>> {
        self.connections
            .iter()
            .find(|c| c.read(cx).server.name == name)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use gpui::TestAppContext;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};

    use super::*;
    use crate::cli_config::CliCredential;
    use crate::cli_tokens::Token;

    /// The CLI as the tests' token sources stand in for it: the token each target's export
    /// gives, and every target asked, in order.
    #[derive(Default)]
    struct FakeCli {
        tokens: Mutex<HashMap<String, String>>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeCli {
        fn give(&self, target: &str, token: &str) {
            self.tokens
                .lock()
                .unwrap()
                .insert(target.into(), token.into());
        }

        fn asked(&self) -> Vec<String> {
            std::mem::take(&mut self.asked.lock().unwrap())
        }
    }

    struct FakeExport {
        target: String,
        cli: Arc<FakeCli>,
    }

    impl TokenSource for FakeExport {
        fn token(
            &self,
            _: Arc<AtomicBool>,
        ) -> BoxFuture<'static, Result<Token, cli_tokens::NoToken>> {
            self.cli.asked.lock().unwrap().push(self.target.clone());
            let token = self.cli.tokens.lock().unwrap().get(&self.target).cloned();
            let target = self.target.clone();
            async move {
                token
                    .map(|token| Token::new(&token))
                    .ok_or_else(|| cli_tokens::NoToken::lasting(format!("no token for '{target}'")))
            }
            .boxed()
        }
    }

    /// A server answering every request, and every authorization header it was sent.
    fn answering() -> (Arc<dyn HttpClient>, Arc<Mutex<Vec<String>>>) {
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = sent.clone();
        let http = FakeHttpClient::create(move |request| {
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            seen.lock().unwrap().push(authorization);
            let body = if request.uri().path().ends_with("/healthz") {
                r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#
            } else {
                r#"{"graphs":[]}"#
            };
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(body))
                    .unwrap())
            }
        });
        (http, sent)
    }

    /// The token sources of CLI servers whose tokens `cli` gives.
    fn exported_by(cli: &Arc<FakeCli>) -> TokenSources {
        let cli = cli.clone();
        Arc::new(move |server: &CliServer| match server.auth {
            CliAuth::Anonymous => None,
            CliAuth::Token(_) => Some(Arc::new(FakeExport {
                target: server.name.clone(),
                cli: cli.clone(),
            }) as Arc<dyn TokenSource>),
        })
    }

    /// A registry whose CLI servers take their tokens from `cli`.
    fn registry_with(
        http: Arc<dyn HttpClient>,
        cli: &Arc<FakeCli>,
        cx: &mut TestAppContext,
    ) -> Entity<ConnectionRegistry> {
        cx.new(|_| {
            let mut registry = ConnectionRegistry::empty(http, None);
            registry.token_sources = exported_by(cli);
            registry
        })
    }

    /// A server whose token the CLI gives, at `url`, of revision `revision`.
    fn server(name: &str, url: &str, revision: &str) -> CliServer {
        CliServer {
            name: name.into(),
            url: url.into(),
            endpoint: format!("{url}/"),
            graphs: Vec::new(),
            auth: CliAuth::Token(CliCredential::Store),
            transport: CliTransport::default(),
            alternatives: Vec::new(),
            revision: revision.into(),
        }
    }

    fn files(servers: Vec<CliServer>) -> CliTargets {
        CliTargets {
            servers: Some(servers),
            ..CliTargets::default()
        }
    }

    fn apply(
        registry: &Entity<ConnectionRegistry>,
        files: CliTargets,
        cx: &mut TestAppContext,
    ) -> bool {
        registry.update(cx, |registry, cx| {
            registry.apply_cli_targets(files, Vec::new(), cx)
        })
    }

    fn find(
        registry: &Entity<ConnectionRegistry>,
        name: &str,
        cx: &mut TestAppContext,
    ) -> Option<Entity<Connection>> {
        registry.read_with(cx, |registry, cx| registry.find(name, cx))
    }

    /// The authorization a health read on `connection` sends now.
    fn token_sent(
        connection: &Entity<Connection>,
        sent: &Arc<Mutex<Vec<String>>>,
        cx: &mut TestAppContext,
    ) -> Option<String> {
        let client = connection.read_with(cx, |connection, _| connection.client().clone());
        let health = cx
            .background_executor
            .spawn(async move { client.health().await });
        cx.run_until_parked();
        drop(health);
        sent.lock().unwrap().pop()
    }

    /// The CLI's servers follow its target list without a restart: a server's token is asked of
    /// the CLI when it connects, not when it is listed, and again when its target changes; a
    /// token typed into Studio stays until then; a new address is a new connection, servers
    /// come and go, and targets that cannot be listed keep every server and say why. An address
    /// is one connection: a CLI target at the address of the environment's server is said, not
    /// connected.
    #[gpui::test]
    async fn cli_servers_follow_the_cli_files(cx: &mut TestAppContext) {
        let (http, sent) = answering();
        let cli = Arc::new(FakeCli::default());
        cli.give("a", "one");
        let registry = registry_with(http, &cli, cx);

        let both = || {
            files(vec![
                server("a", "http://a.test", "1"),
                server("b", "http://b.test", "1"),
            ])
        };
        assert!(apply(&registry, both(), cx));
        let a = find(&registry, "a", cx).unwrap();
        assert!(find(&registry, "b", cx).is_some());
        assert!(!apply(&registry, both(), cx));
        assert_eq!(
            cli.asked(),
            Vec::<String>::new(),
            "nothing asked until connected"
        );

        a.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(cli.asked(), ["a"]);
        assert_eq!(token_sent(&a, &sent, cx).as_deref(), Some("Bearer one"));

        // A changed target (its token file rewritten, say) asks again; the same connection
        // sends the new token.
        cli.give("a", "two");
        assert!(apply(
            &registry,
            files(vec![
                server("a", "http://a.test", "2"),
                server("b", "http://b.test", "1"),
            ]),
            cx
        ));
        cx.run_until_parked();
        assert_eq!(cli.asked(), ["a"]);
        assert_eq!(find(&registry, "a", cx), Some(a.clone()));
        assert_eq!(token_sent(&a, &sent, cx).as_deref(), Some("Bearer two"));

        // A token typed into Studio stays while the target does not change.
        registry.update(cx, |registry, cx| {
            registry.update_token(&a, "typed".into(), cx)
        });
        cx.run_until_parked();
        apply(
            &registry,
            files(vec![server("a", "http://a.test", "2")]),
            cx,
        );
        cx.run_until_parked();
        assert_eq!(token_sent(&a, &sent, cx).as_deref(), Some("Bearer typed"));
        assert!(cli.asked().is_empty());

        // Adding an address listed already is that server again, and nothing is asked of the
        // CLI; a name a target cannot have is refused before the CLI is asked.
        let count = registry.read_with(cx, |registry, _| registry.connections.len());
        let again = registry.update(cx, |registry, cx| {
            registry.add_target("mine".into(), "http://a.test".into(), None, cx)
        });
        assert_eq!(again.await, Ok(a.clone()));
        assert_eq!(
            registry.read_with(cx, |registry, _| registry.connections.len()),
            count
        );
        let refused = registry.update(cx, |registry, cx| {
            registry.add_target("1st".into(), "https://new.test".into(), None, cx)
        });
        assert!(
            refused
                .await
                .unwrap_err()
                .contains("cannot be a CLI target's name")
        );

        // A new address is a new connection; a server no longer named leaves; a new one joins.
        assert!(apply(
            &registry,
            files(vec![
                server("a", "http://a2.test", "2"),
                server("c", "http://c.test", "1"),
            ]),
            cx
        ));
        let moved = find(&registry, "a", cx).unwrap();
        assert_ne!(moved, a);
        assert_eq!(
            moved.read_with(cx, |connection, _| connection.server.url.clone()),
            "http://a2.test"
        );
        assert!(find(&registry, "b", cx).is_none());
        assert!(find(&registry, "c", cx).is_some());

        // Targets that cannot be listed keep every server and say why.
        assert!(apply(
            &registry,
            CliTargets {
                servers: None,
                errors: vec!["the CLI refuses its configuration".into()],
                ..CliTargets::default()
            },
            cx
        ));
        assert!(find(&registry, "a", cx).is_some() && find(&registry, "c", cx).is_some());
        registry.read_with(cx, |registry, _| {
            assert_eq!(registry.notices(), ["the CLI refuses its configuration"])
        });

        // A server Studio cannot reach is said once; the same file read again changes nothing.
        let with_bad = || {
            files(vec![
                server("a", "http://a2.test", "2"),
                server("bad", "http://bad host", "1"),
                server("c", "http://c.test", "1"),
            ])
        };
        assert!(apply(&registry, with_bad(), cx));
        assert!(find(&registry, "bad", cx).is_none());
        registry.read_with(cx, |registry, _| {
            let said = registry.notices();
            assert_eq!(said.len(), 1, "{said:?}");
            assert!(
                said[0].starts_with("CLI target 'bad' cannot be reached: "),
                "{said:?}"
            );
        });
        assert!(!apply(&registry, with_bad(), cx), "nothing else changed");

        // The environment's server keeps its address: a CLI target naming it later is not a
        // second connection, and is said; one whose CLI server moved there drops its old one.
        let mine = registry.update(cx, |registry, cx| {
            let entry = ServerEntry {
                name: "mine".into(),
                url: "http://d.test".into(),
                source: ServerSource::Environment,
                known_graphs: Vec::new(),
            };
            registry.add_connection(entry, None, cx).unwrap()
        });
        let at_d = |cx: &mut TestAppContext| {
            registry.read_with(cx, |registry, cx| {
                registry
                    .connections
                    .iter()
                    .filter(|connection| connection.read(cx).server.url == "http://d.test")
                    .count()
            })
        };
        assert!(apply(
            &registry,
            files(vec![
                server("a", "http://d.test", "2"),
                server("c", "http://c.test", "1"),
            ]),
            cx
        ));
        assert_eq!(at_d(cx), 1);
        assert!(find(&registry, "a", cx).is_none());
        assert_eq!(find(&registry, "mine", cx), Some(mine));
        registry.read_with(cx, |registry, _| {
            let said: Vec<_> = registry
                .notices()
                .iter()
                .filter(|error| error.contains("one connection per address"))
                .collect();
            assert_eq!(said.len(), 1, "{:?}", registry.notices());
            assert!(said[0].contains("'mine'"));
        });
    }

    /// A CLI target renamed on the same endpoint is the same server: its connection, which
    /// open windows hold, takes the new name and asks the CLI as the new target. A target moved
    /// to a new address takes the graphs remembered for its name, and the connection at the old
    /// address is retired. One that names another proxy, or a certificate authority of its own
    /// or another bundle of one, keeps its connection, moved to a client made with them; one
    /// whose transport Studio cannot use is retired.
    #[gpui::test]
    async fn a_renamed_cli_target_keeps_its_connection(cx: &mut TestAppContext) {
        let (http, sent) = answering();
        let cli = Arc::new(FakeCli::default());
        cli.give("old", "one");
        cli.give("new", "two");
        let registry = registry_with(http.clone(), &cli, cx);
        let at_a = |cx: &mut TestAppContext| {
            registry.read_with(cx, |registry, cx| {
                registry
                    .connections
                    .iter()
                    .filter(|connection| connection.read(cx).server.url == "http://a.test")
                    .cloned()
                    .collect::<Vec<_>>()
            })
        };
        // Listed as the CLI lists them on Unix: the same entry under another name.
        let listed = |name: &str| {
            let mut targets = serde_json::Map::new();
            targets.insert(
                name.to_owned(),
                serde_json::json!({"kind": "server", "endpoint": "http://a.test/",
                    "auth": {"provider": "token", "source": {"store": {}}}}),
            );
            cli_config::servers_from(&targets, Platform::Unix)
        };
        apply(&registry, listed("old"), cx);
        let open = at_a(cx).pop().unwrap();
        open.update(cx, |connection, cx| {
            connection.remember_graph("knowledge");
            connection.connect(cx);
        });
        cx.run_until_parked();
        assert_eq!(token_sent(&open, &sent, cx).as_deref(), Some("Bearer one"));

        let renamed = || listed("new");
        assert!(apply(&registry, renamed(), cx));
        cx.run_until_parked();
        assert_eq!(at_a(cx), vec![open.clone()], "one connection, the one held");
        open.read_with(cx, |connection, _| {
            assert_eq!(connection.server.name, "new");
            assert_eq!(connection.server.known_graphs, ["knowledge"]);
        });
        assert_eq!(cli.asked(), ["old", "new"]);
        assert_eq!(token_sent(&open, &sent, cx).as_deref(), Some("Bearer two"));
        assert!(!apply(&registry, renamed(), cx));

        // Moved to another address, it is a new connection with the graphs it had. The one at
        // the old address, which a window may still hold, sends nothing more: no token, and no
        // request.
        assert!(apply(
            &registry,
            files(vec![server("new", "http://b.test", "renamed")]),
            cx
        ));
        assert!(at_a(cx).is_empty());
        let moved = find(&registry, "new", cx).unwrap();
        moved.read_with(cx, |connection, _| {
            assert_eq!(connection.server.url, "http://b.test");
            assert_eq!(connection.server.known_graphs, ["knowledge"]);
        });
        open.read_with(cx, |connection, _| {
            assert!(matches!(
                connection.state,
                crate::ConnectionState::Unreachable(_)
            ));
        });
        sent.lock().unwrap().clear();
        assert_eq!(
            token_sent(&open, &sent, cx),
            None,
            "the old address is sent nothing"
        );

        // Through a proxy now, or trusting a certificate authority of its own: the same
        // connection, which every window holding it reaches through its new client, kept while
        // the bundle the listing read stays the same and moved again with another.
        let made = Arc::new(AtomicUsize::new(0));
        let counted = made.clone();
        let through_http = http.clone();
        registry.update(cx, |registry, _| {
            registry.transported = Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(through_http.clone())
            });
        });
        let mut proxied = server("new", "http://b.test", "renamed");
        proxied.transport.proxy = Some("http://proxy.test:8080".into());
        assert!(apply(&registry, files(vec![proxied]), cx));
        assert_eq!(find(&registry, "new", cx).unwrap(), moved);
        assert_eq!(
            made.load(Ordering::SeqCst),
            1,
            "a client made for the proxy"
        );
        moved.read_with(cx, |connection, _| {
            assert_eq!(connection.server.known_graphs, ["knowledge"]);
        });
        let mut rooted = server("new", "http://b.test", "renamed");
        rooted.transport.roots = Some(cli_config::CliRoots {
            replace: false,
            file: PathBuf::from("/ca.pem"),
            bundle: b"first".to_vec(),
        });
        assert!(apply(&registry, files(vec![rooted.clone()]), cx));
        assert_eq!(made.load(Ordering::SeqCst), 2);
        assert!(!apply(&registry, files(vec![rooted.clone()]), cx));
        assert_eq!(
            made.load(Ordering::SeqCst),
            2,
            "the same bundle keeps its client"
        );
        rooted.transport.roots.as_mut().unwrap().bundle = b"second".to_vec();
        assert!(apply(&registry, files(vec![rooted.clone()]), cx));
        assert_eq!(made.load(Ordering::SeqCst), 3);
        assert_eq!(find(&registry, "new", cx).unwrap(), moved);

        // A transport Studio cannot use retires the connection, said once, and lists no server.
        registry.update(cx, |registry, _| {
            registry.transported = Arc::new(|_| Err(anyhow::anyhow!("not a certificate")));
        });
        rooted.transport.roots.as_mut().unwrap().bundle = b"third".to_vec();
        assert!(apply(&registry, files(vec![rooted]), cx));
        assert!(find(&registry, "new", cx).is_none());
        moved.read_with(cx, |connection, _| {
            assert!(matches!(
                connection.state,
                crate::ConnectionState::Unreachable(_)
            ));
        });
    }

    /// A server whose target is anonymous asks the CLI nothing and sends no token; one that
    /// turns anonymous drops its token, and one that turns to a token asks the CLI.
    #[gpui::test]
    async fn an_anonymous_cli_target_asks_the_cli_nothing(cx: &mut TestAppContext) {
        let (http, sent) = answering();
        let cli = Arc::new(FakeCli::default());
        let registry = registry_with(http, &cli, cx);
        let mut open = server("open", "http://127.0.0.1:8787", "1");
        open.auth = CliAuth::Anonymous;
        apply(&registry, files(vec![open]), cx);
        let connection = find(&registry, "open", cx).unwrap();
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, crate::ConnectionState::Ready);
            assert!(connection.token_ready(), "nothing to wait for");
        });
        // Refresh asks no CLI of an anonymous target either.
        connection.update(cx, |connection, cx| connection.refresh_token(cx));
        cx.run_until_parked();
        assert!(cli.asked().is_empty());
        assert_eq!(token_sent(&connection, &sent, cx).as_deref(), Some(""));

        cli.give("open", "now-keyed");
        apply(
            &registry,
            files(vec![server("open", "http://127.0.0.1:8787", "2")]),
            cx,
        );
        cx.run_until_parked();
        assert_eq!(cli.asked(), ["open"]);
        assert_eq!(
            token_sent(&connection, &sent, cx).as_deref(),
            Some("Bearer now-keyed")
        );
        let mut anonymous = server("open", "http://127.0.0.1:8787", "3");
        anonymous.auth = CliAuth::Anonymous;
        apply(&registry, files(vec![anonymous]), cx);
        cx.run_until_parked();
        assert!(cli.asked().is_empty());
        assert_eq!(token_sent(&connection, &sent, cx).as_deref(), Some(""));
    }

    /// Where the CLI gives Studio no token (Windows, whose CLI writes a token only to a Unix
    /// pipe and refuses every export), the CLI's targets are listed all the same, through the
    /// registry's own listing: an anonymous target connects and sends no token, and each that
    /// takes one is refused with why, as a direct target is, and is neither connected nor asked
    /// for a token. Add server refuses a token before the CLI is asked. The same listing on Unix
    /// lands every server target.
    #[gpui::test]
    async fn without_token_export_only_anonymous_targets_connect(cx: &mut TestAppContext) {
        let (http, sent) = answering();
        let cli = Arc::new(FakeCli::default());
        cli.give("stored", "secret-stored");
        let targets: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "disk": {"kind": "direct", "scope": "graph", "uri": "file:///tmp/graph.omni"},
                "envy": {"kind": "server", "endpoint": "https://env.example.com/",
                    "auth": {"provider": "token", "source": {"env": "OMNIGRAPH_STUDIO_TEST"}}},
                "filed": {"kind": "server", "endpoint": "https://file.example.com/",
                    "auth": {"provider": "token", "source": {"file": "/tokens/filed"}}},
                "helped": {"kind": "server", "endpoint": "https://exec.example.com/",
                    "auth": {"provider": "exec", "command": ["/bin/false"], "timeout": "5s"}},
                "open": {"kind": "server", "endpoint": "http://127.0.0.1:8787/",
                    "auth": {"provider": "anonymous"}, "default_graph": "knowledge"},
                "stored": {"kind": "server", "endpoint": "https://store.example.com/",
                    "auth": {"provider": "token", "source": {"store": {}}}},
            }))
            .unwrap();
        // The platform each listing was given.
        let given: Arc<Mutex<Vec<Platform>>> = Arc::default();
        let dir = std::env::temp_dir().join(format!("studio-no-export-{}", std::process::id()));
        let registry = cx.new(|cx| {
            let mut registry = ConnectionRegistry::empty(http, Some(dir.clone()));
            registry.platform = Platform::Other;
            registry.token_sources = exported_by(&cli);
            let (targets, given) = (targets.clone(), given.clone());
            registry.lister = Arc::new(move |_, _, platform| {
                given.lock().unwrap().push(platform);
                let listed = cli_config::servers_from(&targets, platform);
                async move { listed }.boxed()
            });
            registry.start_listing(cx);
            registry
        });
        cx.run_until_parked();
        assert_eq!(*given.lock().unwrap(), [Platform::Other]);
        let names = |cx: &mut TestAppContext| {
            registry.read_with(cx, |registry, cx| {
                let connections = registry.connections.iter();
                connections
                    .map(|connection| connection.read(cx).server.name.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert!(registry.read_with(cx, |registry, _| registry.listed()));
        assert_eq!(names(cx), ["open"]);

        let open = find(&registry, "open", cx).unwrap();
        open.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        open.read_with(cx, |connection, _| {
            assert_eq!(connection.state, crate::ConnectionState::Ready);
            assert_eq!(connection.server.known_graphs, ["knowledge"]);
        });
        assert_eq!(token_sent(&open, &sent, cx).as_deref(), Some(""));

        registry.read_with(cx, |registry, _| {
            let notices = registry.notices();
            for name in ["envy", "filed", "helped", "stored"] {
                let refused = format!(
                    "CLI target '{name}' takes a token, and the Omnigraph CLI gives a token only \
                     through a Unix pipe, so Studio on Windows cannot use this target yet"
                );
                assert!(notices.contains(&refused), "{name}: {notices:?}");
            }
            assert!(
                notices
                    .iter()
                    .any(|notice| notice.starts_with("CLI target 'disk' opens storage directly")),
                "{notices:?}"
            );
            assert_eq!(notices.len(), 5, "{notices:?}");
        });
        assert!(cli.asked().is_empty(), "no token asked for");

        let refused = registry.update(cx, |registry, cx| {
            let token = Some("secret-typed".to_owned());
            registry.add_target("new".into(), "https://new.test".into(), token, cx)
        });
        assert_eq!(
            refused.await.unwrap_err(),
            "the Omnigraph CLI gives a token only through a Unix pipe, so Studio on Windows \
             cannot use a server that needs a token yet; leave the token empty to add one that \
             needs none"
        );
        assert_eq!(given.lock().unwrap().len(), 1, "the CLI was not asked");
        assert_eq!(names(cx), ["open"]);

        // On Unix the same targets land on servers, each asking the CLI for its token when it
        // connects, and the token file one names is watched; on Windows nothing reads it.
        let watched = |listed: &CliTargets| -> Vec<PathBuf> {
            listed
                .watched
                .iter()
                .map(|(path, _)| path.clone())
                .collect()
        };
        let unix = cli_config::servers_from(&targets, Platform::Unix);
        assert_eq!(watched(&unix), [PathBuf::from("/tokens/filed")]);
        let landed: Vec<String> = unix.servers.unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(landed, ["envy", "filed", "helped", "open", "stored"]);
        assert_eq!(unix.errors.len(), 1, "{:?}", unix.errors);
        let other = cli_config::servers_from(&targets, Platform::Other);
        assert_eq!(watched(&other), Vec::<PathBuf>::new());
    }

    /// The graph ids opened on a CLI target are remembered by its name: a target listed later
    /// knows them, and a save keeps every CLI target's. The environment's server is not
    /// remembered. A file that cannot be read remembers nothing.
    #[gpui::test]
    async fn graphs_are_remembered_by_target(cx: &mut TestAppContext) {
        let (http, _) = answering();
        let cli = Arc::new(FakeCli::default());
        let registry = registry_with(http, &cli, cx);
        registry.update(cx, |registry, _| {
            registry
                .known
                .targets
                .insert("a".into(), vec!["remembered".into()]);
        });
        apply(
            &registry,
            files(vec![server("a", "http://a.test", "1")]),
            cx,
        );
        let a = find(&registry, "a", cx).unwrap();
        a.update(cx, |connection, _| {
            assert_eq!(connection.server.known_graphs, ["remembered"]);
            connection.remember_graph("typed");
        });
        registry.update(cx, |registry, cx| {
            let entry = ServerEntry {
                name: "environment".into(),
                url: "http://env.test".into(),
                source: ServerSource::Environment,
                known_graphs: vec!["env".into()],
            };
            registry.add_connection(entry, None, cx).unwrap();
            registry.save(cx);
            assert_eq!(
                registry.known.targets.get("a").cloned(),
                Some(vec!["remembered".to_owned(), "typed".to_owned()])
            );
            assert!(!registry.known.targets.contains_key("environment"));
        });

        let dir = std::env::temp_dir().join(format!("studio-known-graphs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_graphs.json");
        assert_eq!(load_known_graphs(&path), KnownGraphs::default());
        std::fs::write(&path, r#"{"targets":{"a":["g"]}}"#).unwrap();
        assert_eq!(
            load_known_graphs(&path).targets.get("a").cloned(),
            Some(vec!["g".to_owned()])
        );
        std::fs::write(&path, "{").unwrap();
        assert_eq!(load_known_graphs(&path), KnownGraphs::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A CLI listing that answers when the test gives it its targets, and how many listings
    /// were asked.
    #[derive(Clone, Default)]
    struct HeldListing {
        answers: Arc<Mutex<Vec<oneshot::Receiver<CliTargets>>>>,
        asked: Arc<AtomicUsize>,
    }

    impl HeldListing {
        /// The next listing's answer, given by sending on what this returns.
        fn next(&self) -> oneshot::Sender<CliTargets> {
            let (give, given) = oneshot::channel();
            self.answers.lock().unwrap().push(given);
            give
        }

        fn lister(&self) -> Lister {
            let held = self.clone();
            Arc::new(move |_, _, _| {
                held.asked.fetch_add(1, Ordering::SeqCst);
                let given = {
                    let mut answers = held.answers.lock().unwrap();
                    (!answers.is_empty()).then(|| answers.remove(0))
                };
                async move {
                    match given {
                        Some(given) => given.await.unwrap_or_default(),
                        None => CliTargets {
                            servers: Some(Vec::new()),
                            ..CliTargets::default()
                        },
                    }
                }
                .boxed()
            })
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    /// A start lists the CLI's targets off the main thread: the registry is made at once,
    /// knowing none of the CLI's servers, and what waits for them goes on once the listing
    /// lands. Before, the start waited on the main thread for the CLI's seven commands.
    #[gpui::test]
    async fn a_start_lists_the_targets_off_the_main_thread(cx: &mut TestAppContext) {
        let (http, _) = answering();
        let listing = HeldListing::default();
        let answer = listing.next();
        let dir = std::env::temp_dir().join(format!("studio-listing-{}", std::process::id()));
        let registry = cx.new(|cx| {
            let mut registry = ConnectionRegistry::empty(http, Some(dir.clone()));
            registry.lister = listing.lister();
            registry.start_listing(cx);
            registry
        });
        let listed = Arc::new(AtomicBool::new(false));
        let waited = registry.update(cx, |registry, _| registry.when_listed());
        let heard = listed.clone();
        cx.background_executor
            .spawn(async move {
                waited.await;
                heard.store(true, Ordering::SeqCst);
            })
            .detach();
        cx.run_until_parked();
        assert_eq!(listing.asked(), 1, "listed at once");
        registry.read_with(cx, |registry, _| {
            assert!(!registry.listed());
            assert!(registry.connections.is_empty());
        });
        assert!(!listed.load(Ordering::SeqCst), "waiting for the listing");

        answer
            .send(files(vec![server("a", "http://a.test", "1")]))
            .ok();
        cx.run_until_parked();
        assert!(listed.load(Ordering::SeqCst));
        registry.read_with(cx, |registry, cx| {
            assert!(registry.listed());
            assert!(registry.find("a", cx).is_some());
        });
        // Listed already: what asks now goes on at once.
        let again = registry.update(cx, |registry, _| registry.when_listed());
        assert_eq!(again.now_or_never(), Some(()));
    }

    /// A listing that names a token file for the first time keeps the stamp it took of it: the
    /// next checks list nothing until a file changes, then list once. Before, a new token file
    /// was kept unstamped, and every start listed its targets again at the first check.
    #[gpui::test]
    async fn a_listing_keeps_the_stamps_it_took(cx: &mut TestAppContext) {
        let (http, _) = answering();
        let listing = HeldListing::default();
        let dir = std::env::temp_dir().join(format!("studio-stamps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = cli_config::config_path(&dir);
        let token = dir.join("token");
        std::fs::write(&config, "version: 1\n").unwrap();
        std::fs::write(&token, "one").unwrap();
        // As `read_targets` takes them: the operator file and the token file a target names.
        let listed = |config: &Path, token: &Path| CliTargets {
            servers: Some(vec![server("a", "http://a.test", "1")]),
            watched: [config, token]
                .iter()
                .map(|path| {
                    (
                        path.to_path_buf(),
                        cli_config::stamp(&[path.to_path_buf()])[0],
                    )
                })
                .collect(),
            ..CliTargets::default()
        };
        listing.next().send(listed(&config, &token)).ok();
        let registry = cx.new(|cx| {
            let mut registry = ConnectionRegistry::empty(http, Some(dir.clone()));
            registry.lister = listing.lister();
            registry.start_listing(cx);
            registry
        });
        cx.run_until_parked();
        assert_eq!(listing.asked(), 1);
        for _ in 0..3 {
            cx.executor().advance_clock(OPERATOR_POLL);
            cx.run_until_parked();
        }
        assert_eq!(listing.asked(), 1, "nothing changed");
        registry.read_with(cx, |registry, _| {
            assert_eq!(registry.watched, [config.clone(), token.clone()])
        });

        std::fs::write(&token, "rotated").unwrap();
        listing.next().send(listed(&config, &token)).ok();
        cx.executor().advance_clock(OPERATOR_POLL);
        cx.run_until_parked();
        assert_eq!(listing.asked(), 2, "the token file changed");
        cx.executor().advance_clock(OPERATOR_POLL);
        cx.run_until_parked();
        assert_eq!(listing.asked(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The CLI the settings name (`omnigraph_cli`) is where Studio looks first, its `~` taken
    /// as the home folder, for its servers and for the agent alike (`cli_tokens::candidates`).
    /// Naming another lists the targets again at the next check; an empty setting names none.
    #[gpui::test]
    async fn the_cli_setting_is_where_studio_looks_first(cx: &mut TestAppContext) {
        let (http, _) = answering();
        let listing = HeldListing::default();
        let set = |named: Option<&str>, cx: &mut TestAppContext| {
            let settings = studio_settings::StudioSettings {
                omnigraph_cli: named.map(str::to_owned),
                ..Default::default()
            };
            cx.update(|cx| cx.set_global(settings));
            cx.run_until_parked();
        };
        set(None, cx);
        let dir = std::env::temp_dir().join(format!("studio-cli-setting-{}", std::process::id()));
        let registry = cx.new(|cx| {
            let mut registry = ConnectionRegistry::empty(http, Some(dir.clone()));
            registry.lister = listing.lister();
            registry.follow_cli_setting(cx);
            registry.start_listing(cx);
            registry
        });
        cx.run_until_parked();
        assert_eq!(listing.asked(), 1);
        let places = |cx: &mut TestAppContext| {
            registry.read_with(cx, |registry, _| registry.cli_places.clone())
        };
        assert_eq!(places(cx), cli_tokens::candidates());
        assert_eq!(cli_tokens::configured(), None);

        set(Some("~/tools/omnigraph"), cx);
        let named = std::env::home_dir().unwrap().join("tools/omnigraph");
        assert_eq!(cli_tokens::configured(), Some(named.clone()));
        let looked = places(cx);
        assert_eq!(looked[0], named);
        assert_eq!(looked.iter().filter(|place| **place == named).count(), 1);
        cx.executor().advance_clock(OPERATOR_POLL);
        cx.run_until_parked();
        assert_eq!(listing.asked(), 2, "listed again with the CLI named");

        set(Some(""), cx);
        assert_eq!(cli_tokens::configured(), None);
        assert!(!places(cx).contains(&named));
    }

    /// What the person is told about the servers is one list, each notice saying what
    /// happened: the environment's server not added stays first through every listing, then
    /// what the CLI's targets could not give, which a listing replaces.
    #[gpui::test]
    async fn the_notices_are_one_list(cx: &mut TestAppContext) {
        let (http, _) = answering();
        let cli = Arc::new(FakeCli::default());
        let registry = registry_with(http, &cli, cx);
        registry.update(cx, |registry, _| {
            registry.environment_notice =
                Some("The server OMNIGRAPH_STUDIO_SERVER names was not added: bad".into());
        });
        let refused = CliTargets {
            servers: None,
            errors: vec!["The Omnigraph CLI (…) could not list its targets: refused".into()],
            ..CliTargets::default()
        };
        assert!(apply(&registry, refused, cx));
        let listed = CliTargets {
            servers: Some(Vec::new()),
            errors: vec!["CLI target 'disk' opens storage directly".into()],
            ..CliTargets::default()
        };
        assert!(apply(&registry, listed.clone(), cx));
        registry.read_with(cx, |registry, _| {
            assert_eq!(
                registry.notices(),
                [
                    "The server OMNIGRAPH_STUDIO_SERVER names was not added: bad",
                    "CLI target 'disk' opens storage directly",
                ]
            )
        });
        assert!(!apply(&registry, listed, cx), "the same notices again");
    }

    /// A server added with no name takes its host as its target's name, made into one a target
    /// can have.
    #[test]
    fn a_server_added_with_no_name_is_named_by_its_host() {
        assert_eq!(name_for("https://graph.example.com"), "graph.example.com");
        assert_eq!(name_for("http://127.0.0.1:8787"), "server-127.0.0.1");
        assert_eq!(name_for("http://[::1]:8787"), "server----1-");
        for name in ["a", "graph.example.com", "dev_2", "x-y"] {
            assert!(valid_target_name(name), "{name}");
        }
        for name in ["", "1st", "-x", "a b", "a/b"] {
            assert!(!valid_target_name(name), "{name}");
        }
    }

    /// A CLI target that trusts its own certificate authority is reached with it, as the CLI
    /// reaches it: trusted with the system's roots (`append`) or alone (`replace`). Studio's own
    /// client, which trusts the system's roots, is refused at the handshake, and so is a bundle
    /// of another authority.
    #[test]
    fn a_target_is_reached_with_its_own_certificate_authority() {
        let authority = crate::test_tls::Authority::new("the server's own");
        let url = format!("https://127.0.0.1:{}/healthz", authority.serve());
        let trusting = |pem: &str, replace| CliTransport {
            proxy: None,
            roots: Some(cli_config::CliRoots {
                replace,
                file: PathBuf::from("ca.pem"),
                bundle: pem.as_bytes().to_vec(),
            }),
        };
        let status = |http: Arc<dyn HttpClient>| {
            smol::block_on(http.get(&url, AsyncBody::default(), false))
                .map(|response| response.status().as_u16())
        };
        for replace in [false, true] {
            let http = transport_client(&trusting(&authority.pem, replace)).unwrap();
            assert_eq!(status(http).unwrap(), 200, "replace: {replace}");
        }
        let another = crate::test_tls::Authority::new("another");
        for replace in [false, true] {
            let http = transport_client(&trusting(&another.pem, replace)).unwrap();
            assert!(
                status(http).is_err(),
                "another authority, replace: {replace}"
            );
        }
        let own: Arc<dyn HttpClient> =
            Arc::new(reqwest_client::ReqwestClient::user_agent(crate::USER_AGENT).unwrap());
        assert!(
            status(own).is_err(),
            "Studio's own client does not trust it"
        );
    }
}
