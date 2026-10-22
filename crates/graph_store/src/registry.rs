//! Every server Studio knows: the server targets the OmniGraph CLI lists, and, for development
//! and tests, one from the environment. Studio keeps no server list or token of its own
//! (OmniGraph RFC 1017): a server is added as a CLI target, through the CLI
//! ([`ConnectionRegistry::add_target`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};

use crate::cli_config::{self, CliAlias, CliAuth, CliServer, CliTargets, FileStamp, ManagedTarget};
use crate::cli_tokens::{self, CliTokenSource, Token, TokenSource};
use crate::connection::{Connection, ServerEntry, ServerSource};

/// How often the CLI's files are checked for a change, as Studio's settings file is.
const OPERATOR_POLL: Duration = Duration::from_secs(2);

/// Makes the HTTP client for a CLI target whose `transport.proxy` names a proxy URL.
type ProxiedHttp = Arc<dyn Fn(&str) -> anyhow::Result<Arc<dyn HttpClient>> + Send + Sync>;

/// Makes the token source of a CLI server whose token the CLI gives; none for an anonymous one.
type TokenSources = Arc<dyn Fn(&CliServer) -> Option<Arc<dyn TokenSource>> + Send + Sync>;

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
    /// The proxy its requests go through, which its connection's client was made with.
    proxy: Option<String>,
}

pub struct ConnectionRegistry {
    pub connections: Vec<Entity<Connection>>,
    /// The environment's server, when it could not be added at start.
    start_errors: Vec<String>,
    http: Arc<dyn HttpClient>,
    proxied: ProxiedHttp,
    token_sources: TokenSources,
    /// The operator home the CLI reads (`$OMNIGRAPH_HOME`, else `~/.omnigraph`).
    operator_dir: Option<PathBuf>,
    /// The files whose change lists the targets again (the operator file and the token files
    /// the targets name), and their stamp before the last listing.
    watched: Vec<PathBuf>,
    operator_stamp: Vec<Option<FileStamp>>,
    /// The places Studio looks for the CLI, and their stamp: a CLI installed, upgraded or
    /// removed lists the targets and asks every CLI server's token again.
    cli_places: Vec<PathBuf>,
    cli_stamp: Vec<Option<FileStamp>>,
    /// Each CLI server as last listed, by its address.
    cli_kept: HashMap<String, CliKept>,
    /// What the CLI's targets could not give when last listed.
    cli_errors: Vec<String>,
    /// The managed targets the CLI last listed, refused until Studio has its own sign-in.
    managed: Vec<ManagedTarget>,
    /// The graph ids remembered for each CLI target, as last read or saved.
    known: KnownGraphs,
    /// Whether the remembered graphs are written to the data directory: Studio's, not a
    /// test's.
    saving: bool,
    _watch: Option<Task<()>>,
}

struct GlobalRegistry(Entity<ConnectionRegistry>);

impl Global for GlobalRegistry {}

/// Studio's data directory; one rule for the whole app, kept in `studio_settings`.
pub use studio_settings::data_dir;

fn known_graphs_path() -> PathBuf {
    data_dir().join("known_graphs.json")
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

/// The HTTP client for a CLI target whose requests go through the proxy at `url`
/// (`transport.proxy`, `mode: url`). Zed's client also leaves out the hosts in `NO_PROXY`.
fn proxied_client(url: &str) -> anyhow::Result<Arc<dyn HttpClient>> {
    let proxy = http_client::Url::parse(url)?;
    Ok(Arc::new(
        reqwest_client::ReqwestClient::proxy_and_user_agent(Some(proxy), crate::USER_AGENT)?,
    ))
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

    fn load(http: Arc<dyn HttpClient>, cx: &mut Context<Self>) -> Self {
        let operator_dir = cli_config::operator_dir();
        let mut this = Self::empty(http, operator_dir.clone());
        this.proxied = Arc::new(proxied_client);
        this.known = load_known_graphs(&known_graphs_path());
        this.saving = true;

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
                this.start_errors
                    .push(format!("OMNIGRAPH_STUDIO_SERVER: {error}"));
            }
        }

        // Listed before the windows a start reopens look for the CLI's servers, within
        // `START_TIMEOUT`; each asks the CLI for its token when it connects. The stamp is taken
        // before the listing.
        this.token_sources = Arc::new(cli_token_source);
        this.watched = operator_dir
            .iter()
            .map(|dir| cli_config::config_path(dir))
            .collect();
        let stamp = cli_config::stamp(&this.watched);
        this.cli_places = cli_tokens::candidates();
        this.cli_stamp = cli_config::stamp(&this.cli_places);
        match smol::block_on(cli_config::read_targets_at_start(
            operator_dir.as_deref(),
            &this.cli_places,
        )) {
            Some(targets) => {
                this.keep_stamp(stamp, &targets.watched);
                this.apply_cli_targets(targets, cx);
            }
            None => this.listing_unfinished(),
        }
        this.watch_cli_targets(cx);
        this
    }

    fn empty(http: Arc<dyn HttpClient>, operator_dir: Option<PathBuf>) -> Self {
        let same = http.clone();
        Self {
            connections: Vec::new(),
            start_errors: Vec::new(),
            http,
            proxied: Arc::new(move |_| Ok(same.clone())),
            token_sources: Arc::new(|_| None),
            operator_dir,
            watched: Vec::new(),
            operator_stamp: Vec::new(),
            cli_places: Vec::new(),
            cli_stamp: Vec::new(),
            cli_kept: HashMap::new(),
            cli_errors: Vec::new(),
            managed: Vec::new(),
            known: KnownGraphs::default(),
            saving: false,
            _watch: None,
        }
    }

    /// What the CLI's targets could not give when last listed: no CLI Studio can drive, a
    /// configuration the CLI refuses, a target Studio does not connect to, and the alternatives
    /// on an address.
    pub fn cli_errors(&self) -> &[String] {
        &self.cli_errors
    }

    /// The environment's server, when it could not be added at start, and why.
    pub fn start_errors(&self) -> &[String] {
        &self.start_errors
    }

    /// The managed targets the CLI lists, as `target inspect --resolved` describes them: refused
    /// (each said in [`Self::cli_errors`]) until Studio has its own sign-in, which spec 016 builds
    /// from these. Studio holds no token for them.
    pub fn managed_targets(&self) -> &[ManagedTarget] {
        &self.managed
    }

    /// The CLI's aliases bound to `graph` on the CLI server named `server`: none, since the CLI
    /// has no command that lists its aliases and Studio reads no operator configuration itself
    /// (spec 003).
    pub fn aliases_for(&self, _server: &str, _graph: &str) -> Vec<CliAlias> {
        Vec::new()
    }

    /// Notes that the CLI did not list its targets within the bound at start: the operator file
    /// stays watched, with no stamp, so the first check lists them.
    fn listing_unfinished(&mut self) {
        self.operator_stamp = Vec::new();
        self.keep_cli_errors(vec![format!(
            "the OmniGraph CLI did not list its targets within {} s of Studio's start; Studio \
             lists them again",
            cli_config::START_TIMEOUT.as_secs()
        )]);
    }

    /// Keeps `stamp`, taken of the watched files before the listing, as the stamp of the files
    /// the listing names (`watched`). A file named for the first time (a new token file) was
    /// not stamped: the next check finds the stamp changed and lists once more, which changes
    /// nothing unless that file changed meanwhile.
    fn keep_stamp(&mut self, stamp: Vec<Option<FileStamp>>, watched: &[PathBuf]) {
        if self.watched == watched {
            self.operator_stamp = stamp;
        } else {
            self.watched = watched.to_vec();
            self.operator_stamp = Vec::new();
        }
    }

    /// Lists the CLI's targets again whenever the operator file, a token file a target names, or
    /// the CLI where Studio looks for it changes, so a target added there, a changed address or
    /// a rotated token reaches Studio without a restart; a CLI installed, upgraded or removed
    /// also asks every CLI server's token again. The CLI's secure store cannot be watched: a
    /// token kept there is asked for again on connect, after a 401 and on Refresh.
    fn watch_cli_targets(&mut self, cx: &mut Context<Self>) {
        let dir = self.operator_dir.clone();
        self._watch = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(OPERATOR_POLL).await;
                let Ok((watched, places)) = this.update(cx, |this, _| {
                    (this.watched.clone(), this.cli_places.clone())
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
                if !listed_files && !cli_moved {
                    continue;
                }
                // The stamp is taken before the listing: a write during it changes the next one.
                let at = dir.clone();
                let targets = cx
                    .background_executor()
                    .spawn(async move { cli_config::read_targets(at.as_deref(), &places).await })
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        this.cli_stamp = cli_stamp;
                        this.keep_stamp(stamp, &targets.watched);
                        let changed = this.apply_cli_targets(targets, cx);
                        if cli_moved {
                            this.ask_cli_tokens_again(cx);
                        }
                        if changed {
                            this.save(cx);
                            cx.notify();
                        }
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

    /// Brings the CLI's servers in line with the targets the CLI lists, and says whether
    /// anything changed. An address is one server (spec 003), so a CLI server is its address: a
    /// target renamed on the same endpoint renames the connection in place, which the windows
    /// open on it keep. A new address is a new connection, with the graphs Studio remembered for
    /// its target's name, or for the name at the address it left; an
    /// address no longer listed leaves the list (a window open on it keeps its connection); a
    /// target whose entry or token file changed asks the CLI for its token again; a target's
    /// graph is added. Targets that could not be listed change no server. What could not be
    /// listed or used, a server that cannot be added included, is a change only when it differs
    /// from what was kept.
    fn apply_cli_targets(&mut self, targets: CliTargets, cx: &mut Context<Self>) -> bool {
        let CliTargets {
            servers,
            mut errors,
            watched: _,
            managed,
        } = targets;
        let Some(servers) = servers else {
            return self.keep_cli_errors(errors);
        };
        let mut changed = managed != self.managed;
        self.managed = managed;
        // An address that keeps its server keeps its connection only with the same proxy: the
        // client was made with it.
        let named: Vec<(&String, &Option<String>)> = servers
            .iter()
            .map(|server| (&server.url, &server.proxy))
            .collect();
        let kept = &self.cli_kept;
        let mut left: Vec<ServerEntry> = Vec::new();
        self.connections.retain(|connection| {
            let server = &connection.read(cx).server;
            let stays = server.source != ServerSource::Cli
                || named.iter().any(|(url, proxy)| {
                    **url == server.url
                        && kept
                            .get(&server.url)
                            .is_none_or(|kept| kept.proxy == **proxy)
                });
            if !stays {
                left.push(server.clone());
            }
            stays
        });
        changed |= !left.is_empty();
        self.cli_kept
            .retain(|url, _| left.iter().all(|entry| entry.url != *url));
        self.cli_kept
            .retain(|url, _| named.iter().any(|(named, _)| *named == url));
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
                proxy: server.proxy.clone(),
            };
            let existing = self.connections.iter().find(|connection| {
                let entry = &connection.read(cx).server;
                entry.source == ServerSource::Cli && entry.url == url
            });
            if let Some(connection) = existing.cloned() {
                connection.update(cx, |connection, cx| {
                    if connection.server.name != server.name {
                        connection.server.name = server.name.clone();
                        changed = true;
                        cx.notify();
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
            let http = match &server.proxy {
                Some(proxy) => match (self.proxied)(proxy) {
                    Ok(http) => http,
                    Err(error) => {
                        errors.push(format!(
                            "CLI target '{}' names the proxy {proxy}, which Studio cannot use: \
                             {error}",
                            server.name
                        ));
                        continue;
                    }
                },
                None => self.http.clone(),
            };
            let source = (self.token_sources)(&server);
            match Connection::new(entry, None, http) {
                Ok(connection) => {
                    let connection = connection.with_token_source(source);
                    self.connections.push(cx.new(|_| connection));
                    self.cli_kept.insert(url, kept);
                    changed = true;
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        let errors_changed = self.keep_cli_errors(errors);
        changed || errors_changed
    }

    /// Keeps `errors` as what the CLI's targets could not give, and says whether they changed.
    fn keep_cli_errors(&mut self, errors: Vec<String>) -> bool {
        let changed = errors != self.cli_errors;
        self.cli_errors = errors;
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
                "“{name}” cannot be a CLI target's name: it starts with a letter and holds only                  letters, digits, `.`, `_` and `-`"
            )));
        }
        let token = token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .map(Token::typed);
        let dir = self.operator_dir.clone();
        let places = self.cli_places.clone();
        cx.spawn(async move |this, cx| {
            let (added, targets) = {
                let (name, url) = (name.clone(), url.clone());
                cx.background_executor()
                    .spawn(async move {
                        let added =
                            cli_tokens::add_server_target(&places, &name, &url, token).await;
                        let targets = cli_config::read_targets(dir.as_deref(), &places).await;
                        (added, targets)
                    })
                    .await
            };
            this.update(cx, |this, cx| {
                this.keep_stamp(cli_config::stamp(&targets.watched), &targets.watched);
                if this.apply_cli_targets(targets, cx) {
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

    use futures::FutureExt as _;
    use futures::future::BoxFuture;
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
        fn token(&self) -> BoxFuture<'static, Result<Token, String>> {
            self.cli.asked.lock().unwrap().push(self.target.clone());
            let token = self.cli.tokens.lock().unwrap().get(&self.target).cloned();
            let target = self.target.clone();
            async move {
                token
                    .map(|token| Token::new(&token))
                    .ok_or_else(|| format!("no token for '{target}'"))
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

    /// A registry whose CLI servers take their tokens from `cli`.
    fn registry_with(
        http: Arc<dyn HttpClient>,
        cli: &Arc<FakeCli>,
        cx: &mut TestAppContext,
    ) -> Entity<ConnectionRegistry> {
        let cli = cli.clone();
        cx.new(|_| {
            let mut registry = ConnectionRegistry::empty(http, None);
            registry.token_sources = Arc::new(move |server: &CliServer| match server.auth {
                CliAuth::Anonymous => None,
                CliAuth::Token(_) => Some(Arc::new(FakeExport {
                    target: server.name.clone(),
                    cli: cli.clone(),
                }) as Arc<dyn TokenSource>),
            });
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
            proxy: None,
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
        registry.update(cx, |registry, cx| registry.apply_cli_targets(files, cx))
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
    /// is one connection: a CLI target at the address of a server added in Studio is said, not
    /// connected. The CLI lists no aliases, so none is offered.
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
            assert_eq!(registry.cli_errors(), ["the CLI refuses its configuration"])
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
        registry.read_with(cx, |registry, _| assert_eq!(registry.cli_errors().len(), 1));
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
                .cli_errors()
                .iter()
                .filter(|error| error.contains("one connection per address"))
                .collect();
            assert_eq!(said.len(), 1, "{:?}", registry.cli_errors());
            assert!(said[0].contains("'mine'"));
        });

        registry.read_with(cx, |registry, _| {
            assert!(registry.aliases_for("c", "g").is_empty())
        });
    }

    /// A CLI target renamed on the same endpoint is the same server: its connection, which
    /// open windows hold, takes the new name and asks the CLI as the new target. A target moved
    /// to a new address takes the graphs remembered for its name. One that names another proxy
    /// is a new connection, since its client is made with it.
    #[gpui::test]
    async fn a_renamed_cli_target_keeps_its_connection(cx: &mut TestAppContext) {
        let (http, sent) = answering();
        let cli = Arc::new(FakeCli::default());
        cli.give("old", "one");
        cli.give("new", "two");
        let registry = registry_with(http, &cli, cx);
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
        // Listed as the CLI lists them: the same entry under another name.
        let listed = |name: &str| {
            let mut targets = serde_json::Map::new();
            targets.insert(
                name.to_owned(),
                serde_json::json!({"kind": "server", "endpoint": "http://a.test/",
                    "auth": {"provider": "token", "source": {"store": {}}}}),
            );
            cli_config::servers_from(&targets)
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

        // Moved to another address, it is a new connection with the graphs it had.
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

        // Through a proxy now: a connection of its own.
        let mut proxied = server("new", "http://b.test", "renamed");
        proxied.proxy = Some("http://proxy.test:8080".into());
        assert!(apply(&registry, files(vec![proxied]), cx));
        let through = find(&registry, "new", cx).unwrap();
        assert_ne!(through, moved);
        through.read_with(cx, |connection, _| {
            assert_eq!(connection.server.known_graphs, ["knowledge"]);
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
            assert!(!connection.token_from_cli());
        });
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

    /// A start past its bound for the listing keeps the operator file watched, unstamped, so
    /// the first check lists the targets, rather than never again.
    #[gpui::test]
    async fn a_start_past_its_bound_lists_at_the_first_check(cx: &mut TestAppContext) {
        let (http, _) = answering();
        let dir = std::env::temp_dir().join(format!("studio-unlisted-{}", std::process::id()));
        let registry = cx.new(|_| ConnectionRegistry::empty(http, Some(dir.clone())));
        registry.update(cx, |registry, cx| {
            registry.watched = vec![cli_config::config_path(&dir)];
            registry.listing_unfinished();
            registry.watch_cli_targets(cx);
        });
        let said = |cx: &mut TestAppContext| {
            registry.read_with(cx, |registry, _| registry.cli_errors().to_vec())
        };
        assert!(said(cx)[0].contains("did not list its targets within 5 s"));
        cx.executor().advance_clock(OPERATOR_POLL);
        cx.run_until_parked();
        // Listed with no CLI where it looks (a test's registry has no places): said so now.
        let after = said(cx);
        assert!(
            after[0].contains("found no OmniGraph CLI it can drive"),
            "{after:?}"
        );
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
}
