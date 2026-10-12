//! Servers, profiles and tokens from the OmniGraph CLI's operator config (spec 003).
//!
//! Studio reads `$OMNIGRAPH_HOME` (else `~/.omnigraph`) the way the CLI does at OmniGraph
//! v0.13.0 (`crates/omnigraph-cli/src/operator.rs`): servers from `config.yaml`, a server's token
//! from `OMNIGRAPH_TOKEN_<NAME>`, then `[<name>] token = …` in `credentials`, then
//! `OMNIGRAPH_BEARER_TOKEN`. Studio never writes these files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliServer {
    pub name: String,
    pub url: String,
    /// Graph ids named for this server by the CLI's profiles.
    pub graphs: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OperatorConfig {
    #[serde(default)]
    servers: BTreeMap<String, ServerConfig>,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileConfig>,
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
        && !home.is_empty() {
            return Some(expand_tilde(&home));
        }
    home_dir().map(|home| home.join(".omnigraph"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

/// The servers in the CLI's config, sorted by name. A missing or unreadable config is no
/// servers; a config Studio cannot parse is reported.
pub fn load_servers() -> Result<Vec<CliServer>, String> {
    let Some(dir) = operator_dir() else {
        return Ok(Vec::new());
    };
    load_servers_from(&dir.join("config.yaml"))
}

pub fn load_servers_from(path: &Path) -> Result<Vec<CliServer>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let config: OperatorConfig = serde_yaml::from_str(&text)
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
                && !entry.graphs.contains(graph) {
                    entry.graphs.push(graph.clone());
                }
    }
    Ok(servers)
}

/// `intel-dev` → `OMNIGRAPH_TOKEN_INTEL_DEV`.
pub fn token_env_name(server: &str) -> String {
    let mut name = String::from("OMNIGRAPH_TOKEN_");
    for c in server.chars() {
        name.push(match c {
            '-' => '_',
            other => other.to_ascii_uppercase(),
        });
    }
    name
}

/// The token for a named CLI server, resolved as the CLI resolves it.
pub fn token_for(server: &str) -> Result<Option<String>, String> {
    if let Ok(token) = std::env::var(token_env_name(server)) {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_owned()));
        }
    }
    if let Some(dir) = operator_dir()
        && let Some(token) = read_credential_at(&dir.join("credentials"), server)? {
            return Ok(Some(token));
        }
    if let Ok(token) = std::env::var("OMNIGRAPH_BEARER_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_owned()));
        }
    }
    Ok(None)
}

pub fn read_credential_at(path: &Path, server: &str) -> Result<Option<String>, String> {
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
                && key.trim() == "token" {
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
            "servers:\n  local:\n    url: http://127.0.0.1:8787  # dev\n  prod: { url: https://example.com }\nprofiles:\n  dev: { server: local, default_graph: knowledge }\ndefaults:\n  output: table\n",
        )
        .unwrap();
        let servers = load_servers_from(&path).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "local");
        assert_eq!(servers[0].graphs, vec!["knowledge"]);
        std::fs::remove_dir_all(dir).ok();
    }

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
