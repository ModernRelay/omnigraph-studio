//! Every server Studio knows: from the CLI config, added in Studio, or from the environment.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};

use crate::cli_config::{self, CliAlias, OperatorFiles};
use crate::connection::{Connection, ServerEntry, ServerSource};

/// The keychain user name Studio stores server tokens under.
const KEYCHAIN_USER: &str = "omnigraph-studio";

/// How often the CLI's files are checked for a change, as Studio's settings file is.
const OPERATOR_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SavedServers {
    #[serde(default)]
    servers: Vec<SavedServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedServer {
    name: String,
    url: String,
    /// `cli` for a CLI server whose remembered graphs Studio keeps, `studio` for one added here.
    source: String,
    #[serde(default)]
    graphs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryEvent {
    ServersChanged,
}

pub struct ConnectionRegistry {
    pub connections: Vec<Entity<Connection>>,
    /// Servers named in the environment or saved by Studio that could not be added at start.
    start_errors: Vec<String>,
    http: Arc<dyn HttpClient>,
    /// Where the CLI's files are (`$OMNIGRAPH_HOME`, else `~/.omnigraph`), and their stamp
    /// when last read.
    operator_dir: Option<PathBuf>,
    operator_stamp: Vec<Option<(SystemTime, u64)>>,
    /// The token each CLI server took from the CLI's files when they were last read. Only a
    /// token changed there replaces the connection's, so one typed into Studio for this run
    /// stays until then.
    cli_tokens: HashMap<String, Option<String>>,
    /// What the CLI's files could not give when last read.
    cli_errors: Vec<String>,
    /// The CLI config's aliases, as last read.
    aliases: Vec<CliAlias>,
    /// Servers saved by Studio and not connected this run, since another connection has their
    /// address (the environment's): they stay in the saved list.
    kept_saved: Vec<SavedServer>,
    _watch: Option<Task<()>>,
}

impl EventEmitter<RegistryEvent> for ConnectionRegistry {}

struct GlobalRegistry(Entity<ConnectionRegistry>);

impl Global for GlobalRegistry {}

/// Studio's data directory; one rule for the whole app, kept in `studio_settings`.
pub use studio_settings::data_dir;

fn saved_path() -> PathBuf {
    data_dir().join("servers.json")
}

fn load_saved() -> SavedServers {
    std::fs::read_to_string(saved_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
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
        let saved = load_saved();

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

        // Studio's own servers before the CLI's: an address is one connection (spec 003), and
        // one added here keeps it when a CLI target comes to name it too. Each address was
        // saved as `add_server` checked it, scheme included.
        for saved in saved
            .servers
            .iter()
            .filter(|s| s.source == "studio")
            .cloned()
        {
            if let Some(other) = this.connected_at(&saved.url, cx) {
                this.start_errors.push(format!(
                    "{}: Studio connects to {} already as '{other}', and keeps one connection \
                     per address; the server stays saved",
                    saved.name, saved.url
                ));
                this.kept_saved.push(saved);
                continue;
            }
            let read = cx.read_credentials(&saved.url);
            let entry = ServerEntry {
                name: saved.name,
                url: saved.url,
                source: ServerSource::Studio,
                known_graphs: saved.graphs,
            };
            // Listed at once, so the windows a start reopens find it and a save made while the
            // keychain is read keeps it; it connects once its token is in.
            let name = entry.name.clone();
            let connection = match this.add_connection(entry, None, cx) {
                Ok(connection) => connection,
                Err(error) => {
                    this.start_errors.push(format!("{name}: {error}"));
                    continue;
                }
            };
            let wait = connection.update(cx, |connection, _| connection.await_token());
            cx.spawn(async move |_, cx| {
                let read = read.await;
                connection.update(cx, |connection, cx| connection.token_read(wait, read, cx));
            })
            .detach();
        }

        this.operator_stamp = cli_config::stamp(operator_dir.as_deref());
        this.apply_operator_files(
            cli_config::read_operator(operator_dir.as_deref()),
            &saved,
            cx,
        );
        this.watch_operator_files(cx);
        this
    }

    /// The name of the connection at `url`, when there is one.
    fn connected_at(&self, url: &str, cx: &App) -> Option<String> {
        self.connections.iter().find_map(|connection| {
            let server = &connection.read(cx).server;
            (server.url == url).then(|| server.name.clone())
        })
    }

    fn empty(http: Arc<dyn HttpClient>, operator_dir: Option<PathBuf>) -> Self {
        Self {
            connections: Vec::new(),
            start_errors: Vec::new(),
            http,
            operator_dir,
            operator_stamp: Vec::new(),
            cli_tokens: HashMap::new(),
            cli_errors: Vec::new(),
            aliases: Vec::new(),
            kept_saved: Vec::new(),
            _watch: None,
        }
    }

    /// What the CLI's files could not give when last read: a config that does not parse, or
    /// credentials other users can read.
    pub fn cli_errors(&self) -> &[String] {
        &self.cli_errors
    }

    /// The servers named in the environment or saved by Studio that could not be added at
    /// start, each by its name and why.
    pub fn start_errors(&self) -> &[String] {
        &self.start_errors
    }

    /// The CLI config's aliases bound to `graph` on the CLI server named `server`, by name.
    pub fn aliases_for(&self, server: &str, graph: &str) -> Vec<CliAlias> {
        self.aliases
            .iter()
            .filter(|alias| alias.server == server && alias.graph.as_deref() == Some(graph))
            .cloned()
            .collect()
    }

    /// Reads the CLI's files again whenever their stamp changes, so a server added there, a
    /// changed address or a token from `omnigraph login` reaches Studio without a restart.
    fn watch_operator_files(&mut self, cx: &mut Context<Self>) {
        let dir = self.operator_dir.clone();
        self._watch = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(OPERATOR_POLL).await;
                let at = dir.clone();
                let stamp = cx
                    .background_executor()
                    .spawn(async move { cli_config::stamp(at.as_deref()) })
                    .await;
                let Ok(unchanged) = this.update(cx, |this, _| this.operator_stamp == stamp) else {
                    break;
                };
                if unchanged {
                    continue;
                }
                // The stamp is taken before the read: a write during it changes the next one.
                let at = dir.clone();
                let files = cx
                    .background_executor()
                    .spawn(async move { cli_config::read_operator(at.as_deref()) })
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        this.operator_stamp = stamp;
                        if this.apply_operator_files(files, &SavedServers::default(), cx) {
                            this.save(cx);
                            cx.emit(RegistryEvent::ServersChanged);
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

    /// Brings the CLI's servers in line with what its files say, and says whether anything
    /// changed. A new server is added, with the graphs Studio remembered for it (`saved`); one
    /// no longer named leaves the list (a window open on it keeps its connection); one at a new
    /// address is a new connection; a token changed in the files replaces the connection's; a
    /// profile's graph is added. Files that could not be read change no server. What they could
    /// not give, a server they name that cannot be added included, is a change only when it
    /// differs from what was kept.
    fn apply_operator_files(
        &mut self,
        files: OperatorFiles,
        saved: &SavedServers,
        cx: &mut Context<Self>,
    ) -> bool {
        let OperatorFiles {
            servers,
            aliases,
            mut errors,
        } = files;
        let Some(servers) = servers else {
            return self.keep_cli_errors(errors);
        };
        let mut changed = false;
        if aliases != self.aliases {
            self.aliases = aliases;
            changed = true;
        }
        let named: Vec<String> = servers
            .iter()
            .map(|(server, _)| server.name.clone())
            .collect();
        let before = self.connections.len();
        self.connections.retain(|connection| {
            let server = &connection.read(cx).server;
            server.source != ServerSource::Cli || named.contains(&server.name)
        });
        changed |= self.connections.len() != before;
        self.cli_tokens.retain(|name, _| named.contains(name));
        for (server, token) in servers {
            let url = graph_client::server_root(&server.url).unwrap_or(server.url);
            let existing = self.connections.iter().position(|connection| {
                let entry = &connection.read(cx).server;
                entry.source == ServerSource::Cli && entry.name == server.name
            });
            if let Some(ix) = existing
                && self.connections[ix].read(cx).server.url == url
            {
                let connection = self.connections[ix].clone();
                connection.update(cx, |connection, _| {
                    for graph in &server.graphs {
                        if !connection.server.known_graphs.contains(graph) {
                            connection.remember_graph(graph);
                            changed = true;
                        }
                    }
                });
                if self.cli_tokens.get(&server.name) != Some(&token) {
                    self.cli_tokens.insert(server.name.clone(), token.clone());
                    connection.update(cx, |connection, cx| connection.set_token(token, cx));
                    changed = true;
                }
                continue;
            }
            // An address Studio reaches already as a server of its own (added in Studio, or the
            // environment's) is that connection: a second would open its graphs twice, and the
            // graph keys, window restore and links would each find either. The CLI server's
            // connection at its old address, if any, goes.
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
                if let Some(ix) = existing {
                    self.connections.remove(ix);
                    changed = true;
                }
                self.cli_tokens.remove(&server.name);
                continue;
            }
            // New, or at another address: a connection of its own. The graphs remembered for
            // the name come along.
            let mut graphs = server.graphs.clone();
            let remembered = match existing {
                Some(ix) => self.connections[ix].read(cx).server.known_graphs.clone(),
                None => saved
                    .servers
                    .iter()
                    .find(|saved| saved.source == "cli" && saved.name == server.name)
                    .map(|saved| saved.graphs.clone())
                    .unwrap_or_default(),
            };
            for graph in remembered {
                if !graphs.contains(&graph) {
                    graphs.push(graph);
                }
            }
            let entry = ServerEntry {
                name: server.name.clone(),
                url,
                source: ServerSource::Cli,
                known_graphs: graphs,
            };
            match Connection::new(entry, token.clone(), self.http.clone()) {
                Ok(connection) => {
                    let connection = cx.new(|_| connection);
                    match existing {
                        Some(ix) => self.connections[ix] = connection,
                        None => self.connections.push(connection),
                    }
                    self.cli_tokens.insert(server.name, token);
                    changed = true;
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        let errors_changed = self.keep_cli_errors(errors);
        changed || errors_changed
    }

    /// Keeps `errors` as what the CLI's files could not give, and says whether they changed.
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

    /// Adds a server by URL. The token goes to the OS keychain; the list to Studio's data
    /// directory.
    pub fn add_server(
        &mut self,
        name: String,
        url: String,
        token: Option<String>,
        graphs: Vec<String>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Entity<Connection>> {
        let url = graph_client::server_root(&url).map_err(anyhow::Error::msg)?;
        // An address listed already is that server: a second connection to it would open its
        // graphs twice, each window writing the same saved tabs over the other's.
        if let Some(existing) = self
            .connections
            .iter()
            .find(|connection| connection.read(cx).server.url == url)
            .cloned()
        {
            if let Some(token) = token {
                self.update_token(&existing, token, cx);
            }
            return Ok(existing);
        }
        let entry = ServerEntry {
            name,
            url: url.clone(),
            source: ServerSource::Studio,
            known_graphs: graphs,
        };
        let connection = self
            .add_connection(entry, token.clone(), cx)
            .map_err(anyhow::Error::msg)?;
        if let Some(token) = &token {
            Self::save_token(&connection, &url, token, cx);
        }
        self.save(cx);
        cx.emit(RegistryEvent::ServersChanged);
        cx.notify();
        Ok(connection)
    }

    /// Replaces a server's token and reconnects. A server Studio added keeps it in the keychain;
    /// for a CLI server it lasts this run, since Studio never writes the CLI's files.
    pub fn update_token(
        &mut self,
        connection: &Entity<Connection>,
        token: String,
        cx: &mut Context<Self>,
    ) {
        let token = token.trim().to_owned();
        let (url, source) = {
            let server = &connection.read(cx).server;
            (server.url.clone(), server.source)
        };
        connection.update(cx, |connection, cx| {
            connection.set_token(Some(token.clone()), cx)
        });
        if source == ServerSource::Studio {
            Self::save_token(connection, &url, &token, cx);
        }
    }

    /// Saves `token` for the server at `url` in the OS keychain. A save that fails is kept on
    /// the connection ([`Connection::token_unsaved`]): the token lasts this run only.
    fn save_token(connection: &Entity<Connection>, url: &str, token: &str, cx: &mut Context<Self>) {
        let write = cx.write_credentials(url, KEYCHAIN_USER, token.as_bytes());
        let connection = connection.clone();
        cx.spawn(async move |_, cx| {
            let saved = write.await.map_err(|error| error.to_string());
            connection.update(cx, |connection, cx| connection.token_saved(saved, cx));
        })
        .detach();
    }

    /// Writes Studio-added servers and remembered graph ids to the data directory, those not
    /// connected this run among them.
    pub fn save(&self, cx: &App) {
        let servers = self
            .connections
            .iter()
            .filter_map(|connection| {
                let server = &connection.read(cx).server;
                let source = match server.source {
                    ServerSource::Studio => "studio",
                    ServerSource::Cli => "cli",
                    ServerSource::Environment => return None,
                };
                Some(SavedServer {
                    name: server.name.clone(),
                    url: server.url.clone(),
                    source: source.into(),
                    graphs: server.known_graphs.clone(),
                })
            })
            .chain(self.kept_saved.iter().cloned())
            .collect();
        let saved = SavedServers { servers };
        if let Ok(text) = serde_json::to_string_pretty(&saved) {
            studio_settings::write_atomic_in_turn(
                saved_path(),
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

    use gpui::TestAppContext;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};

    use super::*;
    use crate::cli_config::CliServer;

    fn files(servers: &[(&str, &str, Option<&str>)]) -> OperatorFiles {
        OperatorFiles {
            servers: Some(
                servers
                    .iter()
                    .map(|(name, url, token)| {
                        let server = CliServer {
                            name: (*name).into(),
                            url: (*url).into(),
                            graphs: Vec::new(),
                        };
                        (server, token.map(str::to_owned))
                    })
                    .collect(),
            ),
            aliases: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// The CLI's servers follow its files without a restart: a token changed there reaches the
    /// same connection, a token typed into Studio stays until the files change theirs, a new
    /// address is a new connection, servers come and go, and files that cannot be read keep
    /// every server and say why. An address is one connection: a CLI target at the address of
    /// a server added in Studio is said, not connected.
    #[gpui::test]
    async fn cli_servers_follow_the_cli_files(cx: &mut TestAppContext) {
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
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT)
                    .body(AsyncBody::from(r#"{"status":"ok","version":"0.14.0"}"#))
                    .unwrap())
            }
        });
        let registry = cx.new(|_| ConnectionRegistry::empty(http, None));
        let apply = |cx: &mut TestAppContext, files: OperatorFiles| {
            registry.update(cx, |registry, cx| {
                registry.apply_operator_files(files, &SavedServers::default(), cx)
            })
        };
        let find = |cx: &mut TestAppContext, name: &str| {
            registry.read_with(cx, |registry, cx| registry.find(name, cx))
        };
        let token_sent = |cx: &mut TestAppContext, connection: &Entity<Connection>| {
            let client = connection.read_with(cx, |connection, _| connection.client().clone());
            let health = cx
                .background_executor
                .spawn(async move { client.health().await });
            cx.run_until_parked();
            drop(health);
            sent.lock().unwrap().pop()
        };

        assert!(apply(
            cx,
            files(&[
                ("a", "http://a.test", Some("one")),
                ("b", "http://b.test", None)
            ])
        ));
        let a = find(cx, "a").unwrap();
        assert!(find(cx, "b").is_some());
        assert!(!apply(
            cx,
            files(&[
                ("a", "http://a.test", Some("one")),
                ("b", "http://b.test", None)
            ])
        ));

        // A token changed in the files: the same connection sends it.
        assert!(apply(
            cx,
            files(&[
                ("a", "http://a.test", Some("two")),
                ("b", "http://b.test", None)
            ])
        ));
        assert_eq!(find(cx, "a"), Some(a.clone()));
        assert_eq!(token_sent(cx, &a).as_deref(), Some("Bearer two"));

        // A token typed into Studio stays while the files keep theirs.
        registry.update(cx, |registry, cx| {
            registry.update_token(&a, "typed".into(), cx)
        });
        apply(cx, files(&[("a", "http://a.test", Some("two"))]));
        assert_eq!(token_sent(cx, &a).as_deref(), Some("Bearer typed"));

        // Adding an address listed already is that server again, not a second connection.
        let count = registry.read_with(cx, |registry, _| registry.connections.len());
        let again = registry.update(cx, |registry, cx| {
            registry
                .add_server("mine".into(), "http://a.test".into(), None, Vec::new(), cx)
                .unwrap()
        });
        assert_eq!(again, a);
        assert_eq!(
            registry.read_with(cx, |registry, _| registry.connections.len()),
            count
        );

        // A new address is a new connection; a server no longer named leaves; a new one joins.
        assert!(apply(
            cx,
            files(&[
                ("a", "http://a2.test", Some("two")),
                ("c", "http://c.test", None)
            ])
        ));
        let moved = find(cx, "a").unwrap();
        assert_ne!(moved, a);
        assert_eq!(
            moved.read_with(cx, |connection, _| connection.server.url.clone()),
            "http://a2.test"
        );
        assert!(find(cx, "b").is_none());
        assert!(find(cx, "c").is_some());

        // A config that does not parse keeps every server and says why.
        assert!(apply(
            cx,
            OperatorFiles {
                servers: None,
                aliases: Vec::new(),
                errors: vec!["could not parse config.yaml".into()],
            }
        ));
        assert!(find(cx, "a").is_some() && find(cx, "c").is_some());
        registry.read_with(cx, |registry, _| {
            assert_eq!(registry.cli_errors(), ["could not parse config.yaml"])
        });

        // A server at an address that cannot be one is said once; the same files read again
        // change nothing.
        let with_bad = || {
            files(&[
                ("a", "http://a2.test", Some("two")),
                ("bad", "http://bad host", None),
                ("c", "http://c.test", None),
            ])
        };
        assert!(apply(cx, with_bad()));
        assert!(find(cx, "bad").is_none());
        registry.read_with(cx, |registry, _| assert_eq!(registry.cli_errors().len(), 1));
        assert!(!apply(cx, with_bad()), "nothing else changed");

        // A server added in Studio keeps its address: a CLI target naming it later is not a
        // second connection, and is said; one whose CLI server moved there drops its old one.
        let mine = registry.update(cx, |registry, cx| {
            registry
                .add_server("mine".into(), "http://d.test".into(), None, Vec::new(), cx)
                .unwrap()
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
            cx,
            files(&[
                ("a", "http://d.test", Some("two")),
                ("c", "http://c.test", None),
                ("d", "http://d.test/", None),
            ])
        ));
        assert_eq!(at_d(cx), 1);
        assert!(find(cx, "a").is_none() && find(cx, "d").is_none());
        assert_eq!(find(cx, "mine"), Some(mine));
        registry.read_with(cx, |registry, _| {
            let said: Vec<_> = registry
                .cli_errors()
                .iter()
                .filter(|error| error.contains("one connection per address"))
                .collect();
            assert_eq!(said.len(), 2, "{:?}", registry.cli_errors());
            assert!(said.iter().all(|error| error.contains("'mine'")));
        });

        // Aliases are found by server and graph; one that names no graph is under none.
        let alias = |name: &str, graph: Option<&str>| CliAlias {
            name: name.into(),
            server: "c".into(),
            graph: graph.map(str::to_owned),
            query: "find".into(),
            args: Vec::new(),
            params: Default::default(),
        };
        let mut with_aliases = files(&[("c", "http://c.test", None)]);
        with_aliases.aliases = vec![alias("bare", None), alias("mine", Some("g"))];
        assert!(apply(cx, with_aliases));
        registry.read_with(cx, |registry, _| {
            let names: Vec<_> = registry
                .aliases_for("c", "g")
                .into_iter()
                .map(|alias| alias.name)
                .collect();
            assert_eq!(names, ["mine"]);
            assert!(registry.aliases_for("c", "other").is_empty());
        });
    }
}
