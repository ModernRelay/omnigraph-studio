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
    pub load_errors: Vec<String>,
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
            this.add_connection(
                ServerEntry {
                    name: "environment".into(),
                    url,
                    source: ServerSource::Environment,
                    known_graphs: graphs,
                },
                token,
                cx,
            );
        }

        this.operator_stamp = cli_config::stamp(operator_dir.as_deref());
        this.apply_operator_files(
            cli_config::read_operator(operator_dir.as_deref()),
            &saved,
            cx,
        );
        this.watch_operator_files(cx);

        let mut any_moved = false;
        for saved in saved.servers.into_iter().filter(|s| s.source == "studio") {
            // An address saved without a scheme (`localhost:8080`) is read with the one it
            // needs; its token, kept under the old address, moves with it.
            let url = graph_client::server_root(&saved.url).unwrap_or_else(|_| saved.url.clone());
            let moved = url != saved.url;
            let entry = ServerEntry {
                name: saved.name,
                url: url.clone(),
                source: ServerSource::Studio,
                known_graphs: saved.graphs,
            };
            // Listed at once, so the windows a start reopens find it and a save made while the
            // keychain is read keeps it; it connects once its token is in.
            let Some(connection) = this.add_connection(entry, None, cx) else {
                continue;
            };
            connection.update(cx, |connection, _| connection.await_token());
            any_moved |= moved;
            let read = cx.read_credentials(&saved.url);
            cx.spawn(async move |_, cx| {
                let token = read
                    .await
                    .ok()
                    .flatten()
                    .and_then(|(_, bytes)| String::from_utf8(bytes).ok());
                if moved && let Some(token) = &token {
                    cx.update(|cx| cx.write_credentials(&url, KEYCHAIN_USER, token.as_bytes()))
                        .detach();
                }
                connection.update(cx, |connection, cx| connection.set_token(token, cx));
            })
            .detach();
        }
        // Once, with every address read again: the file then names each with its scheme.
        if any_moved {
            this.save(cx);
        }
        this
    }

    fn empty(http: Arc<dyn HttpClient>, operator_dir: Option<PathBuf>) -> Self {
        Self {
            connections: Vec::new(),
            load_errors: Vec::new(),
            http,
            operator_dir,
            operator_stamp: Vec::new(),
            cli_tokens: HashMap::new(),
            cli_errors: Vec::new(),
            aliases: Vec::new(),
            _watch: None,
        }
    }

    /// What the CLI's files could not give when last read: a config that does not parse, or
    /// credentials other users can read.
    pub fn cli_errors(&self) -> &[String] {
        &self.cli_errors
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
    /// profile's graph is added. Files that could not be read change no server.
    fn apply_operator_files(
        &mut self,
        files: OperatorFiles,
        saved: &SavedServers,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut changed = files.errors != self.cli_errors;
        self.cli_errors = files.errors;
        let Some(servers) = files.servers else {
            return changed;
        };
        if files.aliases != self.aliases {
            self.aliases = files.aliases;
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
                Err(error) => self.cli_errors.push(error.to_string()),
            }
        }
        changed
    }

    fn add_connection(
        &mut self,
        entry: ServerEntry,
        token: Option<String>,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Connection>> {
        match Connection::new(entry, token, self.http.clone()) {
            Ok(connection) => {
                let connection = cx.new(|_| connection);
                self.connections.push(connection.clone());
                Some(connection)
            }
            Err(error) => {
                self.load_errors.push(error.to_string());
                None
            }
        }
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
        if let Some(token) = &token {
            cx.write_credentials(&url, KEYCHAIN_USER, token.as_bytes())
                .detach();
        }
        let entry = ServerEntry {
            name,
            url,
            source: ServerSource::Studio,
            known_graphs: graphs,
        };
        let connection = self
            .add_connection(entry, token, cx)
            .ok_or_else(|| anyhow::anyhow!("invalid server address"))?;
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
            cx.write_credentials(&url, KEYCHAIN_USER, token.as_bytes())
                .detach();
        }
    }

    /// Writes Studio-added servers and remembered graph ids to the data directory.
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
    /// every server and say why.
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
                    .header(graph_client::CONTRACT_HEADER, "0.13")
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
