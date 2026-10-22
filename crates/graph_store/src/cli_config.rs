//! Servers and tokens from the OmniGraph CLI's target files (spec 003).
//!
//! Studio reads `$OMNIGRAPH_HOME` (else `~/.omnigraph`) the way the CLI does
//! (`crates/omnigraph-cli/src/operator.rs` and `targets.rs`): `config.yaml` is
//! `target_config_version: 1` with `targets`. Each `server` target, and each `managed` cluster
//! target with an `endpoint`, lands on a server; an address is one server, which targets on it
//! with one credential share, since the CLI makes a target of every profile, and one with
//! another credential is said and left out. A server target's token is
//! `OMNIGRAPH_TOKEN_<OWNER>` for its credential owner, else `[<owner>] token = …` in
//! `credentials` when that section's `endpoint` covers the target's. A token is sent only to the
//! server it is keyed to: the untargeted `OMNIGRAPH_BEARER_TOKEN` goes to none, as the CLI
//! dropped it (OmniGraph #1030). Studio never writes these files; it reads them again when they
//! change ([`stamp`]), their permissions included.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliServer {
    /// The name of the target named like the server's credential owner, else of the target
    /// Studio connects as: the CLI's default target, else the first on the address in the file.
    pub name: String,
    pub url: String,
    /// Its targets' `default_graph`s.
    pub graphs: Vec<String>,
}

/// A personal alias in the CLI's config: a name bound to a stored query on a target and graph,
/// with default parameter values (spec 007). It carries no query text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliAlias {
    pub name: String,
    /// The server its target lands on (the target's own name when Studio lists no server for it).
    pub server: String,
    /// The graph the alias names, else its target's `default_graph`. Without either Studio shows
    /// it under no graph.
    pub graph: Option<String>,
    /// The stored query's name.
    pub query: String,
    /// The parameters the CLI fills from positional arguments, in order.
    pub args: Vec<String>,
    /// Fixed parameter values.
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// `config.yaml`, `target_config_version: 1`.
#[derive(Debug, Default, Deserialize)]
struct TargetsConfig {
    /// Read one by one, in the file's order: a target Studio cannot read is said and left out,
    /// not the whole file, and of targets on one address the first may be the one Studio
    /// connects as.
    #[serde(default)]
    targets: serde_norway::Mapping,
    /// Read for its `target` alone: the CLI's default, which Studio connects as among the
    /// targets on its address.
    #[serde(default)]
    defaults: Option<serde_norway::Value>,
    /// Read one by one too: an alias Studio cannot read is left out.
    #[serde(default)]
    aliases: BTreeMap<String, serde_norway::Value>,
}

/// The target fields Studio reads; the rest (storage, the managed control plane) are the CLI's.
#[derive(Debug, Deserialize)]
struct TargetConfig {
    kind: String,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    default_graph: Option<String>,
}

/// The only target file version this reader knows.
const TARGET_CONFIG_VERSION: u64 = 1;

#[derive(Debug, Deserialize)]
struct AliasConfig {
    target: String,
    #[serde(default)]
    graph: Option<String>,
    query: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    params: BTreeMap<String, serde_norway::Value>,
}

/// Where the CLI finds the token for one server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenSource {
    /// A server target's credential owner, whose saved token must be bound to an endpoint that
    /// covers the target's.
    Owner(String),
    /// `auth: anonymous`, or a managed target, whose token the CLI gets from the control plane.
    Anonymous,
}

/// What one `config.yaml` names.
#[derive(Debug, Default)]
struct Operator {
    servers: Vec<(CliServer, TokenSource)>,
    aliases: Vec<CliAlias>,
    /// Entries left out, with the reason.
    errors: Vec<String>,
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
    let operator = match dir {
        Some(dir) => load_operator_from(&dir.join("config.yaml")),
        None => Ok(Operator::default()),
    };
    let Operator {
        servers,
        aliases,
        mut errors,
    } = match operator {
        Ok(operator) => operator,
        Err(error) => {
            return OperatorFiles {
                servers: None,
                aliases: Vec::new(),
                errors: vec![error],
            };
        }
    };
    let mut servers: Vec<(CliServer, Option<String>)> = servers
        .into_iter()
        .map(|(server, source)| {
            let token = token_in(dir, &server, &source).unwrap_or_else(|error| {
                if !errors.contains(&error) {
                    errors.push(error);
                }
                None
            });
            (server, token)
        })
        .collect();
    servers.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
    OperatorFiles {
        servers: Some(servers),
        aliases,
        errors,
    }
}

/// What a file's stamp holds: when it was last written, its size, and its permissions.
pub type FileStamp = (SystemTime, u64, u32);

/// When `config.yaml` and `credentials` in `dir` were last written, their sizes and their
/// permissions: a stamp that differs from the last one means the files are read again. A
/// change of permissions writes nothing, so it moves neither time nor size, yet it decides
/// whether `credentials`' tokens are taken (a file others may read is refused): a
/// `chmod 600` that makes them usable is seen at the next check, not at the next start.
pub fn stamp(dir: Option<&Path>) -> Vec<Option<FileStamp>> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    ["config.yaml", "credentials"]
        .iter()
        .map(|file| {
            let metadata = std::fs::metadata(dir.join(file)).ok()?;
            Some((
                metadata.modified().ok()?,
                metadata.len(),
                permission_bits(&metadata.permissions()),
            ))
        })
        .collect()
}

/// A file's permissions as a number: its mode on Unix, else whether it is read-only.
fn permission_bits(permissions: &std::fs::Permissions) -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::fs::PermissionsExt::mode(permissions)
    }
    #[cfg(not(unix))]
    {
        u32::from(permissions.readonly())
    }
}

/// The servers and the aliases in a CLI target file.
fn load_operator_from(path: &Path) -> Result<Operator, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Operator::default());
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let unparsed =
        |error: serde_norway::Error| format!("could not parse {}: {error}", path.display());
    let value: serde_norway::Value = serde_norway::from_str(&text).map_err(unparsed)?;
    if value.is_null() {
        return Ok(Operator::default());
    }
    match value.get("target_config_version") {
        // Decoded from the text, so a parse error keeps its line and column.
        Some(version) if version.as_u64() == Some(TARGET_CONFIG_VERSION) => Ok(targets_operator(
            serde_norway::from_str(&text).map_err(unparsed)?,
        )),
        Some(version) => Err(format!(
            "{} is target_config_version {}, and Studio reads version {TARGET_CONFIG_VERSION}",
            path.display(),
            serde_norway::to_string(version)
                .map(|text| text.trim().to_owned())
                .unwrap_or_default()
        )),
        None => Err(format!(
            "{} has no target_config_version; Studio reads the CLI's target files. Convert it \
             with `omnigraph target import`",
            path.display()
        )),
    }
}

/// The servers the targets land on, and the aliases with their servers and graphs. A `direct`
/// target opens storage, which Studio does not, so it is not listed; a target Studio cannot
/// open is said.
///
/// An address is one server, as every graph's key is its server's address and its id: of the
/// targets on one endpoint, the CLI's default target is the one Studio connects as when it is
/// among them, else the first in the file. Those with its credential (one owner, or both
/// anonymous) land on that server too, as the targets `omnigraph target import` makes of one
/// server's profiles do, with their default graphs; one with another credential is said and
/// left out, since its token would go to the same address beside the first.
fn targets_operator(config: TargetsConfig) -> Operator {
    let default_target = config
        .defaults
        .as_ref()
        .and_then(|defaults| defaults.get("target"))
        .and_then(serde_norway::Value::as_str)
        .map(str::to_owned);
    let mut errors = Vec::new();
    // Each target Studio can open, in file order.
    let mut readable: Vec<Landing> = Vec::new();
    for (key, value) in config.targets {
        let Some(name) = key.as_str().map(str::to_owned) else {
            errors.push("a CLI target's name is not text".to_owned());
            continue;
        };
        let target: TargetConfig = match serde_norway::from_value(value) {
            Ok(target) => target,
            Err(error) => {
                errors.push(format!("CLI target '{name}': {error}"));
                continue;
            }
        };
        let source = match target.kind.as_str() {
            "server" => match (target.credential, target.auth.as_deref()) {
                (Some(owner), None) => TokenSource::Owner(owner),
                (None, Some("anonymous")) => TokenSource::Anonymous,
                _ => {
                    errors.push(format!(
                        "CLI target '{name}' needs one credential owner or `auth: anonymous`"
                    ));
                    continue;
                }
            },
            "managed" => TokenSource::Anonymous,
            _ => continue,
        };
        let Some(endpoint) = target.endpoint else {
            if target.kind == "server" {
                errors.push(format!("CLI target '{name}' has no endpoint"));
            }
            continue;
        };
        if endpoint
            .get(..5)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("unix:"))
        {
            errors.push(format!(
                "CLI target '{name}' is served on a Unix socket ({endpoint}); Studio reaches \
                 servers over HTTP(S)"
            ));
            continue;
        }
        let root = graph_client::server_root(&endpoint).unwrap_or_else(|_| endpoint.clone());
        readable.push(Landing {
            name,
            root,
            endpoint,
            source,
            default_graph: target.default_graph,
        });
    }

    let mut servers: Vec<(CliServer, TokenSource)> = Vec::new();
    // Each target listed: the server it lands on, and its default graph.
    let mut landed: BTreeMap<String, (usize, Option<String>)> = BTreeMap::new();
    let mut roots: Vec<&str> = Vec::new();
    for landing in &readable {
        if !roots.contains(&landing.root.as_str()) {
            roots.push(&landing.root);
        }
    }
    for root in roots {
        let on_root: Vec<&Landing> = readable.iter().filter(|t| t.root == root).collect();
        let chosen = on_root
            .iter()
            .find(|t| default_target.as_deref() == Some(t.name.as_str()));
        let why = if chosen.is_some() {
            "the CLI's default target"
        } else {
            "the first target on it in the file"
        };
        let first = chosen.copied().unwrap_or(on_root[0]);
        let (alike, other): (Vec<&Landing>, Vec<&Landing>) =
            on_root.into_iter().partition(|t| t.source == first.source);
        // Named like the credential owner, as an imported server's own target is, else as
        // the target Studio connects as.
        let named = alike
            .iter()
            .find(|t| t.source == TokenSource::Owner(t.name.clone()))
            .copied()
            .unwrap_or(first);
        let mut graphs: Vec<String> = Vec::new();
        for graph in alike.iter().filter_map(|t| t.default_graph.as_ref()) {
            if !graphs.contains(graph) {
                graphs.push(graph.clone());
            }
        }
        let index = servers.len();
        servers.push((
            CliServer {
                name: named.name.clone(),
                url: named.endpoint.clone(),
                graphs,
            },
            first.source.clone(),
        ));
        for target in alike {
            landed.insert(target.name.clone(), (index, target.default_graph.clone()));
        }
        for skipped in other {
            errors.push(format!(
                "CLI target '{}' is not listed: Studio keeps one connection per address, and \
                 reaches {root} as '{}' ({why}), whose credential differs",
                skipped.name, first.name
            ));
        }
    }
    let aliases = read_aliases(config.aliases)
        .into_iter()
        .map(|mut alias| {
            if let Some((index, default_graph)) = landed.get(&alias.server) {
                alias.server = servers[*index].0.name.clone();
                alias.graph = alias.graph.or_else(|| default_graph.clone());
            }
            alias
        })
        .collect();
    Operator {
        servers,
        aliases,
        errors,
    }
}

/// A target Studio can open, as `targets_operator` reads it.
struct Landing {
    name: String,
    /// Its endpoint as a server root ([`graph_client::server_root`]), which targets on one
    /// address share.
    root: String,
    endpoint: String,
    source: TokenSource,
    default_graph: Option<String>,
}

/// The aliases Studio can read, sorted by name, each naming its target.
fn read_aliases(aliases: BTreeMap<String, serde_norway::Value>) -> Vec<CliAlias> {
    aliases
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
                server: alias.target,
                graph: alias.graph,
                query: alias.query,
                args: alias.args,
                params,
            })
        })
        .collect()
}

/// `intel-dev` → `OMNIGRAPH_TOKEN_INTEL_DEV`.
fn token_env_name(owner: &str) -> String {
    let mut name = String::from("OMNIGRAPH_TOKEN_");
    for c in owner.chars() {
        name.push(match c {
            '-' => '_',
            other => other.to_ascii_uppercase(),
        });
    }
    name
}

/// The token for a CLI server, resolved as the CLI resolves it with its files in `dir`: the
/// owner's variable, else its saved token when that was saved for an endpoint covering the
/// server's.
fn token_in(
    dir: Option<&Path>,
    server: &CliServer,
    source: &TokenSource,
) -> Result<Option<String>, String> {
    let TokenSource::Owner(owner) = source else {
        return Ok(None);
    };
    if let Ok(token) = std::env::var(token_env_name(owner)) {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_owned()));
        }
    }
    let Some(dir) = dir else {
        return Ok(None);
    };
    let Some(saved) = read_credential_at(&dir.join("credentials"), owner)? else {
        return Ok(None);
    };
    let login = format!("run `omnigraph login --target {}`", server.name);
    let Some(scope) = saved.endpoint else {
        return Err(format!(
            "the token saved for '{owner}' names no endpoint, so the CLI does not send it; {login}"
        ));
    };
    if !within_scope(&server.url, &scope) {
        return Err(format!(
            "the token saved for '{owner}' is for {scope}, not {}; {login}",
            server.url
        ));
    }
    Ok(Some(saved.token))
}

/// Whether `endpoint` is `scope` or lies under it, as the CLI matches a saved credential: the
/// same scheme, host and port (the scheme's default filled in), and a path that starts with
/// every segment of `scope`'s.
fn within_scope(endpoint: &str, scope: &str) -> bool {
    fn segments(url: &url::Url) -> Vec<&str> {
        let mut segments: Vec<&str> = url
            .path_segments()
            .map(|segments| segments.collect())
            .unwrap_or_default();
        if segments.last() == Some(&"") {
            segments.pop();
        }
        segments
    }
    let (Ok(endpoint), Ok(scope)) = (url::Url::parse(endpoint), url::Url::parse(scope)) else {
        return false;
    };
    endpoint.scheme() == scope.scheme()
        && endpoint.host_str() == scope.host_str()
        && endpoint.port_or_known_default() == scope.port_or_known_default()
        && segments(&endpoint).starts_with(&segments(&scope))
}

/// A token saved in the credentials file, with the endpoint it was saved for.
struct SavedCredential {
    token: String,
    endpoint: Option<String>,
}

/// What `[owner]` holds in the credentials file at `path`, refused when other users can read
/// the file. The first section of that name is read.
fn read_credential_at(path: &Path, owner: &str) -> Result<Option<SavedCredential>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    refuse_over_permissive(path)?;
    let mut in_section = false;
    let mut token = None;
    let mut endpoint = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if in_section {
                break;
            }
            in_section = section.trim() == owner;
            continue;
        }
        if in_section && let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            let field = match key.trim() {
                "token" => &mut token,
                "endpoint" => &mut endpoint,
                _ => continue,
            };
            field.get_or_insert_with(|| value.to_owned());
        }
    }
    Ok(token
        .filter(|token: &String| !token.is_empty())
        .map(|token| SavedCredential { token, endpoint }))
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

// Every test here reads Unix file modes.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Both files are read together; a file that does not parse, or is not a target file,
    /// keeps the servers read before, and a stamp tells a rewritten file from an untouched one.
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
            "target_config_version: 1\ntargets:\n  studio-test-a: { kind: server, endpoint: http://a.test, credential: studio-test-a }\n  studio-test-b: { kind: server, endpoint: http://b.test, credential: studio-test-b }\n",
        )
        .unwrap();
        let credentials = dir.join("credentials");
        std::fs::write(
            &credentials,
            "[studio-test-a]\ntoken = one\nendpoint = http://a.test\n",
        )
        .unwrap();
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

        std::fs::write(
            &credentials,
            "[studio-test-a]\ntoken = two\nendpoint = http://a.test\n",
        )
        .unwrap();
        assert_ne!(stamp(Some(&dir)), before);
        assert_eq!(
            read_operator(Some(&dir)).servers.unwrap()[0].1.as_deref(),
            Some("two")
        );

        // Open permissions refuse the token, once, not the servers. A change of permissions
        // alone changes the stamp, so it is read again without a restart, either way.
        let written = stamp(Some(&dir));
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o644)).unwrap();
        let opened = stamp(Some(&dir));
        assert_ne!(opened, written);
        let files = read_operator(Some(&dir));
        assert_eq!(files.servers.unwrap()[0].1, None);
        assert_eq!(files.errors.len(), 1, "{:?}", files.errors);
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_ne!(stamp(Some(&dir)), opened, "a chmod 600 is seen");
        assert_eq!(
            read_operator(Some(&dir)).servers.unwrap()[0].1.as_deref(),
            Some("two")
        );
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o644)).unwrap();

        std::fs::write(dir.join("config.yaml"), "targets: [unclosed\n").unwrap();
        let files = read_operator(Some(&dir));
        assert_eq!(files.servers, None);
        assert!(
            files.errors[0].contains("could not parse"),
            "{:?}",
            files.errors
        );
        // A file without a version is not read; the reason names the CLI's conversion.
        std::fs::write(
            dir.join("config.yaml"),
            "servers:\n  studio-test-a: { url: http://a.test }\n",
        )
        .unwrap();
        let files = read_operator(Some(&dir));
        assert_eq!(files.servers, None);
        assert!(
            files.errors[0].contains("omnigraph target import"),
            "{:?}",
            files.errors
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// `omnigraph target import` makes a target of every server and of every profile, so one
    /// server's targets differ only in their default graph: they are one server, named like the
    /// credential owner, with every default graph, and an alias on any of them lands there. An
    /// address is one server: a target on it with another credential is said and left out,
    /// Studio connecting as the CLI's default target when it is there, else as the first on the
    /// address in the file.
    #[cfg(unix)]
    #[test]
    fn targets_on_one_endpoint_are_one_server() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("studio-cli-grouped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // As the CLI writes it, block style.
        std::fs::write(
            dir.join("config.yaml"),
            "target_config_version: 1\n\
             targets:\n\
             \x20 default:\n    kind: server\n    endpoint: http://127.0.0.1:8791\n    credential: studio-test-dev\n    default_graph: knowledge\n\
             \x20 studio-test-dev:\n    kind: server\n    endpoint: http://127.0.0.1:8791/\n    credential: studio-test-dev\n\
             \x20 studio-test-other:\n    kind: server\n    endpoint: https://other.test\n    credential: studio-test-other\n\
             \x20 work:\n    kind: server\n    endpoint: http://127.0.0.1:8791\n    credential: studio-test-dev\n    default_graph: notes\n\
             \x20 open:\n    kind: server\n    endpoint: http://127.0.0.1:8791\n    auth: anonymous\n\
             \x20 zz-public:\n    kind: server\n    endpoint: https://shared.test\n    auth: anonymous\n    default_graph: docs\n\
             \x20 aa-keyed:\n    kind: server\n    endpoint: https://shared.test/\n    credential: studio-test-other\n\
             defaults:\n  target: default\n\
             aliases:\n\
             \x20 people:\n    target: studio-test-dev\n    graph: knowledge\n    query: who_is\n\
             \x20 notes:\n    target: work\n    query: recent\n\
             \x20 public:\n    target: zz-public\n    query: find\n\
             \x20 hidden:\n    target: open\n    query: find\n\
             \x20 lost:\n    target: nowhere\n    query: q\n",
        )
        .unwrap();
        let credentials = dir.join("credentials");
        std::fs::write(
            &credentials,
            "[studio-test-dev]\ntoken = dev-token\nendpoint = http://127.0.0.1:8791\n",
        )
        .unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();

        let files = read_operator(Some(&dir));
        // The anonymous target beside the default one is not a second connection to its
        // address, nor the keyed one after the anonymous target first in the file.
        assert_eq!(files.errors.len(), 2, "{:?}", files.errors);
        assert!(
            files.errors[0].contains("'open' is not listed"),
            "{:?}",
            files.errors
        );
        assert!(
            files.errors[0].contains("as 'default' (the CLI's default target)"),
            "{:?}",
            files.errors
        );
        assert!(
            files.errors[1].contains("'aa-keyed' is not listed"),
            "{:?}",
            files.errors
        );
        assert!(
            files.errors[1].contains("as 'zz-public' (the first target on it in the file)"),
            "{:?}",
            files.errors
        );
        assert!(files.errors.iter().all(|e| !e.contains("dev-token")));
        let listed: Vec<_> = files
            .servers
            .unwrap()
            .into_iter()
            .map(|(server, token)| (server.name, server.url, server.graphs, token))
            .collect();
        assert_eq!(
            listed,
            vec![
                (
                    "studio-test-dev".into(),
                    "http://127.0.0.1:8791/".into(),
                    vec!["knowledge".to_owned(), "notes".to_owned()],
                    Some("dev-token".to_owned())
                ),
                (
                    "studio-test-other".into(),
                    "https://other.test".into(),
                    vec![],
                    None
                ),
                (
                    "zz-public".into(),
                    "https://shared.test".into(),
                    vec!["docs".to_owned()],
                    None
                ),
            ]
        );
        let aliases: Vec<_> = files
            .aliases
            .iter()
            .map(|alias| {
                (
                    alias.name.as_str(),
                    alias.server.as_str(),
                    alias.graph.as_deref(),
                )
            })
            .collect();
        // An alias on a target left out keeps its name and lands on no server.
        assert_eq!(
            aliases,
            vec![
                ("hidden", "open", None),
                ("lost", "nowhere", None),
                ("notes", "studio-test-dev", Some("notes")),
                ("people", "studio-test-dev", Some("knowledge")),
                ("public", "zz-public", Some("docs")),
            ]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// A target file lists its server targets and managed cluster endpoints; a token saved for
    /// a credential owner reaches a target only within the endpoint it was saved for, as the CLI
    /// sends it.
    #[cfg(unix)]
    #[test]
    fn reads_targets_with_their_endpoint_bound_tokens() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("studio-cli-targets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "target_config_version: 1\n\
             defaults: { target: studio-test-local, output: table }\n\
             targets:\n  \
               studio-test-local: { kind: server, endpoint: \"http://127.0.0.1:8787/og\", credential: studio-test-owner, default_graph: knowledge }\n  \
               studio-test-open: { kind: server, endpoint: \"https://open.test\", auth: anonymous }\n  \
               studio-test-sock: { kind: server, endpoint: \"unix:///tmp/og.sock\", credential: studio-test-sock }\n  \
               studio-test-half: { kind: server, endpoint: \"http://half.test\" }\n  \
               studio-test-cloud: { kind: managed, scope: cluster, api: \"https://api.test\", organization_id: org, cluster_id: c, cluster_incarnation: i, control_auth: human-device, endpoint: \"https://cluster.test\", default_graph: crm }\n  \
               studio-test-account: { kind: managed, scope: account, api: \"https://api.test\", control_auth: human-device }\n  \
               studio-test-disk: { kind: direct, scope: graph, uri: ./graph.omni }\n\
             aliases:\n  who: { target: studio-test-local, graph: knowledge, query: who_is, args: [slug] }\n",
        )
        .unwrap();
        let credentials = dir.join("credentials");
        let saved = |text: &str| {
            std::fs::write(&credentials, text).unwrap();
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
            read_operator(Some(&dir))
        };
        let token_of = |files: &OperatorFiles, name: &str| {
            let servers = files.servers.as_ref().unwrap();
            let (_, token) = servers.iter().find(|(s, _)| s.name == name).unwrap();
            token.clone()
        };

        let files = saved("[studio-test-owner]\ntoken = one\nendpoint = http://127.0.0.1:8787\n");
        let listed: Vec<_> = files
            .servers
            .as_ref()
            .unwrap()
            .iter()
            .map(|(server, _)| {
                (
                    server.name.as_str(),
                    server.url.as_str(),
                    server.graphs.clone(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                (
                    "studio-test-cloud",
                    "https://cluster.test",
                    vec!["crm".to_owned()]
                ),
                (
                    "studio-test-local",
                    "http://127.0.0.1:8787/og",
                    vec!["knowledge".to_owned()]
                ),
                ("studio-test-open", "https://open.test", vec![]),
            ]
        );
        assert_eq!(
            token_of(&files, "studio-test-local").as_deref(),
            Some("one")
        );
        assert_eq!(token_of(&files, "studio-test-open"), None);
        assert_eq!(token_of(&files, "studio-test-cloud"), None);
        // A Unix socket and a server target without an owner are said, not listed.
        assert_eq!(files.errors.len(), 2, "{:?}", files.errors);
        assert!(files.errors.iter().any(|e| e.contains("Unix socket")));
        assert!(files.errors.iter().any(|e| e.contains("studio-test-half")));
        assert_eq!(files.aliases[0].server, "studio-test-local");

        // The same scope, spelled with a trailing slash.
        let files =
            saved("[studio-test-owner]\ntoken = two\nendpoint = \"http://127.0.0.1:8787/og/\"\n");
        assert_eq!(
            token_of(&files, "studio-test-local").as_deref(),
            Some("two")
        );
        // A token with no endpoint, at another port, or under a deeper path is not sent.
        for (text, says) in [
            ("[studio-test-owner]\ntoken = three\n", "names no endpoint"),
            (
                "[studio-test-owner]\ntoken = three\nendpoint = http://127.0.0.1:9999\n",
                "is for",
            ),
            (
                "[studio-test-owner]\ntoken = three\nendpoint = http://127.0.0.1:8787/og/a\n",
                "is for",
            ),
            (
                "[studio-test-owner]\ntoken = three\nendpoint = https://127.0.0.1:8787\n",
                "is for",
            ),
        ] {
            let files = saved(text);
            assert_eq!(token_of(&files, "studio-test-local"), None, "{text}");
            assert!(
                files.errors.iter().any(|e| e.contains(says)),
                "{text}: {:?}",
                files.errors
            );
            assert!(
                files.errors.iter().all(|e| !e.contains("three")),
                "{:?}",
                files.errors
            );
        }

        // A version this reader does not know keeps the servers read before.
        std::fs::write(
            dir.join("config.yaml"),
            "target_config_version: 2\ntargets: {}\n",
        )
        .unwrap();
        let files = read_operator(Some(&dir));
        assert_eq!(files.servers, None);
        assert!(files.errors[0].contains("version 1"), "{:?}", files.errors);
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
        let saved = read_credential_at(&path, "local").unwrap().unwrap();
        assert_eq!((saved.token.as_str(), saved.endpoint), ("secret", None));
        assert!(read_credential_at(&path, "missing").unwrap().is_none());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_credential_at(&path, "local").is_err());
        std::fs::remove_dir_all(dir).ok();
        assert_eq!(token_env_name("intel-dev"), "OMNIGRAPH_TOKEN_INTEL_DEV");
    }
}
