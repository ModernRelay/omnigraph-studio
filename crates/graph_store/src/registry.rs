//! Every server Studio knows: from the CLI config, added in Studio, or from the environment.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};

use crate::cli_config;
use crate::connection::{Connection, ServerEntry, ServerSource};

/// The keychain user name Studio stores server tokens under.
const KEYCHAIN_USER: &str = "omnigraph-studio";

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

    pub(crate) fn init(http: Arc<dyn HttpClient>, cx: &mut App) {
        let registry = cx.new(|cx| Self::load(http, cx));
        cx.set_global(GlobalRegistry(registry));
    }

    fn load(http: Arc<dyn HttpClient>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            connections: Vec::new(),
            load_errors: Vec::new(),
            http,
        };
        let saved = load_saved();

        if let Ok(url) = std::env::var("OMNIGRAPH_STUDIO_SERVER") {
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

        match cli_config::load_servers() {
            Ok(servers) => {
                for server in servers {
                    let token = match cli_config::token_for(&server.name) {
                        Ok(token) => token,
                        Err(error) => {
                            this.load_errors.push(error);
                            None
                        }
                    };
                    let mut graphs = server.graphs.clone();
                    if let Some(saved) = saved
                        .servers
                        .iter()
                        .find(|s| s.source == "cli" && s.name == server.name)
                    {
                        for graph in &saved.graphs {
                            if !graphs.contains(graph) {
                                graphs.push(graph.clone());
                            }
                        }
                    }
                    this.add_connection(
                        ServerEntry {
                            name: server.name,
                            url: server.url,
                            source: ServerSource::Cli,
                            known_graphs: graphs,
                        },
                        token,
                        cx,
                    );
                }
            }
            Err(error) => this.load_errors.push(error),
        }

        for saved in saved.servers.into_iter().filter(|s| s.source == "studio") {
            let entry = ServerEntry {
                name: saved.name,
                url: saved.url.clone(),
                source: ServerSource::Studio,
                known_graphs: saved.graphs,
            };
            let read = cx.read_credentials(&saved.url);
            cx.spawn(async move |this, cx| {
                let token = read
                    .await
                    .ok()
                    .flatten()
                    .and_then(|(_, bytes)| String::from_utf8(bytes).ok());
                this.update(cx, |this, cx| {
                    this.add_connection(entry, token, cx);
                    cx.emit(RegistryEvent::ServersChanged);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        this
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
        let url = url.trim().trim_end_matches('/').to_owned();
        url::Url::parse(&url)?;
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

    /// Removes a server Studio added, with its keychain token.
    pub fn remove_server(&mut self, connection: &Entity<Connection>, cx: &mut Context<Self>) {
        let url = connection.read(cx).server.url.clone();
        if connection.read(cx).server.source == ServerSource::Studio {
            cx.delete_credentials(&url).detach();
        }
        self.connections.retain(|c| c != connection);
        self.save(cx);
        cx.emit(RegistryEvent::ServersChanged);
        cx.notify();
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
        let dir = data_dir();
        if std::fs::create_dir_all(&dir).is_ok() {
            if let Ok(text) = serde_json::to_string_pretty(&saved) {
                let _ = std::fs::write(saved_path(), text);
            }
        }
    }

    pub fn find(&self, name: &str, cx: &App) -> Option<Entity<Connection>> {
        self.connections
            .iter()
            .find(|c| c.read(cx).server.name == name)
            .cloned()
    }
}
