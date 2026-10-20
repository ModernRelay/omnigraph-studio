//! Servers, profiles and tokens from the OmniGraph CLI's operator config (spec 003).
//!
//! Studio reads `$OMNIGRAPH_HOME` (else `~/.omnigraph`) the way the CLI does
//! (`crates/omnigraph-cli/src/operator.rs`): servers from `config.yaml`, and a server's token
//! from `OMNIGRAPH_TOKEN_<NAME>`, then `[<name>] token = …` in `credentials`. A token is sent
//! only to the server it is keyed to: the untargeted `OMNIGRAPH_BEARER_TOKEN` goes to none, as
//! the CLI dropped it (OmniGraph #1030). Studio never writes these files; it reads them again
//! when they change ([`stamp`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliServer {
    pub name: String,
    pub url: String,
    /// Graph ids named for this server by the CLI's profiles.
    pub graphs: Vec<String>,
}

/// A personal alias in the CLI's config: a name bound to a stored query on a named server and
/// graph, with default parameter values (spec 007). It carries no query text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliAlias {
    pub name: String,
    pub server: String,
    /// The graph the alias names. Without one the CLI posts to the server root, a route the
    /// server does not serve, so Studio shows such an alias under no graph.
    pub graph: Option<String>,
    /// The stored query's name.
    pub query: String,
    /// The parameters the CLI fills from positional arguments, in order.
    pub args: Vec<String>,
    /// Fixed parameter values.
    pub params: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct OperatorConfig {
    #[serde(default)]
    servers: BTreeMap<String, ServerConfig>,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileConfig>,
    /// Read one by one: an alias Studio cannot read is left out, not the whole file.
    #[serde(default)]
    aliases: BTreeMap<String, serde_norway::Value>,
}

#[derive(Debug, Deserialize)]
struct AliasConfig {
    server: String,
    #[serde(default)]
    graph: Option<String>,
    query: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    params: BTreeMap<String, serde_norway::Value>,
}

#[derive(Debug, Deserialize)]
struct ServerConfig {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ProfileConfig {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    default_graph: Option<String>,
}

/// `$OMNIGRAPH_HOME`, else `~/.omnigraph`.
pub fn operator_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("OMNIGRAPH_HOME")
        && !home.is_empty()
    {
        return Some(expand_tilde(&home));
    }
    home_dir().map(|home| home.join(".omnigraph"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

/// What the CLI's files say, read together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatorFiles {
    /// Each server, sorted by name, with the token the CLI would send it. `None` when
    /// `config.yaml` could not be read or parsed, so the servers read before stand.
    pub servers: Option<Vec<(CliServer, Option<String>)>>,
    /// The aliases, sorted by name; empty when `servers` is `None`.
    pub aliases: Vec<CliAlias>,
    /// What could not be read, each said once.
    pub errors: Vec<String>,
}

/// Reads the servers in `dir`'s `config.yaml` and each one's token. No directory, or no
/// config, is no servers.
pub fn read_operator(dir: Option<&Path>) -> OperatorFiles {
    let config = match dir {
        Some(dir) => load_operator_from(&dir.join("config.yaml")),
        None => Ok((Vec::new(), Vec::new())),
    };
    let mut errors = Vec::new();
    let mut aliases = Vec::new();
    let servers = match config {
        Ok((servers, read)) => {
            aliases = read;
            Some(
                servers
                    .into_iter()
                    .map(|server| {
                        let token = token_in(dir, &server.name).unwrap_or_else(|error| {
                            if !errors.contains(&error) {
                                errors.push(error);
                            }
                            None
                        });
                        (server, token)
                    })
                    .collect(),
            )
        }
        Err(error) => {
            errors.push(error);
            None
        }
    };
    OperatorFiles {
        servers,
        aliases,
        errors,
    }
}

/// When `config.yaml` and `credentials` in `dir` were last written, and their sizes: a stamp
/// that differs from the last one means the files are read again.
pub fn stamp(dir: Option<&Path>) -> Vec<Option<(SystemTime, u64)>> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    ["config.yaml", "credentials"]
        .iter()
        .map(|file| {
            let metadata = std::fs::metadata(dir.join(file)).ok()?;
            Some((metadata.modified().ok()?, metadata.len()))
        })
        .collect()
}

/// The servers and the aliases in a CLI config file.
fn load_operator_from(path: &Path) -> Result<(Vec<CliServer>, Vec<CliAlias>), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let config: OperatorConfig = serde_norway::from_str(&text)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let mut servers: Vec<CliServer> = config
        .servers
        .into_iter()
        .map(|(name, server)| CliServer {
            name,
            url: server.url,
            graphs: Vec::new(),
        })
        .collect();
    for profile in config.profiles.values() {
        if let (Some(server), Some(graph)) = (&profile.server, &profile.default_graph)
            && let Some(entry) = servers.iter_mut().find(|s| &s.name == server)
            && !entry.graphs.contains(graph)
        {
            entry.graphs.push(graph.clone());
        }
    }
    let aliases = config
        .aliases
        .into_iter()
        .filter_map(|(name, value)| {
            let alias: AliasConfig = serde_norway::from_value(value).ok()?;
            let params = alias
                .params
                .into_iter()
                .filter_map(|(key, value)| Some((key, serde_json::to_value(value).ok()?)))
                .collect();
            Some(CliAlias {
                name,
                server: alias.server,
                graph: alias.graph,
                query: alias.query,
                args: alias.args,
                params,
            })
        })
        .collect();
    Ok((servers, aliases))
}

/// `intel-dev` → `OMNIGRAPH_TOKEN_INTEL_DEV`.
fn token_env_name(server: &str) -> String {
    let mut name = String::from("OMNIGRAPH_TOKEN_");
    for c in server.chars() {
        name.push(match c {
            '-' => '_',
            other => other.to_ascii_uppercase(),
        });
    }
    name
}

/// The token for a named CLI server, resolved as the CLI resolves it with its files in `dir`.
fn token_in(dir: Option<&Path>, server: &str) -> Result<Option<String>, String> {
    if let Ok(token) = std::env::var(token_env_name(server)) {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_owned()));
        }
    }
    if let Some(dir) = dir
        && let Some(token) = read_credential_at(&dir.join("credentials"), server)?
    {
        return Ok(Some(token));
    }
    Ok(None)
}

/// The token `[server]` holds in the credentials file at `path`, refused when other users can
/// read the file.
fn read_credential_at(path: &Path, server: &str) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    refuse_over_permissive(path)?;
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = section.trim() == server;
            continue;
        }
        if in_section
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "token"
        {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            return Ok((!value.is_empty()).then(|| value.to_owned()));
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn refuse_over_permissive(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is readable by other users; run `chmod 600 {}`",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_over_permissive(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_servers_and_profile_graphs() {
        let dir = std::env::temp_dir().join(format!("studio-cli-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "servers:\n  local:\n    url: http://127.0.0.1:8787  # dev\n  prod: { url: https://example.com }\nprofiles:\n  dev: { server: local, default_graph: knowledge }\ndefaults:\n  output: table\naliases:\n  who: { server: local, graph: knowledge, query: who_is, args: [slug], params: { limit: 5, deep: true }, format: kv }\n  bare: { server: local, query: who_is }\n  broken: { graph: knowledge }\n",
        )
        .unwrap();
        let (servers, _) = load_operator_from(&path).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "local");
        assert_eq!(servers[0].graphs, vec!["knowledge"]);
        // Aliases come with their parameters; one without a server or query is left out.
        let (_, aliases) = load_operator_from(&path).unwrap();
        let names: Vec<_> = aliases.iter().map(|alias| alias.name.as_str()).collect();
        assert_eq!(names, ["bare", "who"]);
        let who = &aliases[1];
        assert_eq!(
            (
                who.server.as_str(),
                who.graph.as_deref(),
                who.query.as_str()
            ),
            ("local", Some("knowledge"), "who_is")
        );
        assert_eq!(who.args, ["slug"]);
        assert_eq!(who.params["limit"], serde_json::json!(5));
        assert_eq!(who.params["deep"], serde_json::json!(true));
        assert_eq!(aliases[0].graph, None);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Both files are read together; a config that does not parse keeps the servers read
    /// before, and a stamp tells a rewritten file from an untouched one.
    #[cfg(unix)]
    #[test]
    fn reads_servers_with_their_tokens_and_stamps_the_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("studio-cli-operator-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(stamp(Some(&dir)), vec![None, None]);
        assert_eq!(read_operator(Some(&dir)).servers, Some(Vec::new()));

        std::fs::write(
            dir.join("config.yaml"),
            "servers:\n  studio-test-a: { url: http://a.test }\n  studio-test-b: { url: http://b.test }\n",
        )
        .unwrap();
        let credentials = dir.join("credentials");
        std::fs::write(&credentials, "[studio-test-a]\ntoken = one\n").unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = stamp(Some(&dir));
        assert!(before.iter().all(Option::is_some));
        // A server the credentials do not name has no token: an untargeted one in the
        // environment is sent to no server.
        let files = read_operator(Some(&dir));
        let servers = files.servers.unwrap();
        assert_eq!(
            servers
                .iter()
                .map(|(server, token)| (server.name.as_str(), token.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("studio-test-a", Some("one".to_owned())),
                ("studio-test-b", None)
            ]
        );
        assert!(files.errors.is_empty());

        std::fs::write(&credentials, "[studio-test-a]\ntoken = two\n").unwrap();
        assert_ne!(stamp(Some(&dir)), before);
        assert_eq!(
            read_operator(Some(&dir)).servers.unwrap()[0].1.as_deref(),
            Some("two")
        );

        // Open permissions refuse the token, once, not the servers.
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o644)).unwrap();
        let files = read_operator(Some(&dir));
        assert_eq!(files.servers.unwrap()[0].1, None);
        assert_eq!(files.errors.len(), 1, "{:?}", files.errors);

        std::fs::write(dir.join("config.yaml"), "servers: [unclosed\n").unwrap();
        let files = read_operator(Some(&dir));
        assert_eq!(files.servers, None);
        assert!(
            files.errors[0].contains("could not parse"),
            "{:?}",
            files.errors
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn reads_a_token_section_and_refuses_open_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("studio-cli-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials");
        std::fs::write(&path, "[other]\ntoken = a\n[local]\ntoken = \"secret\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_credential_at(&path, "local").unwrap().as_deref(),
            Some("secret")
        );
        assert_eq!(read_credential_at(&path, "missing").unwrap(), None);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_credential_at(&path, "local").is_err());
        std::fs::remove_dir_all(dir).ok();
        assert_eq!(token_env_name("intel-dev"), "OMNIGRAPH_TOKEN_INTEL_DEV");
    }
}
