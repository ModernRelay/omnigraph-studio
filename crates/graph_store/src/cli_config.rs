//! The OmniGraph CLI's targets, as the CLI describes them (spec 003).
//!
//! Studio reads no operator configuration itself: `omnigraph target list --json`, run with the
//! CLI Studio chose ([`crate::cli_tokens`]), says what the targets are, without a server or a
//! secret. A `server` target lands on a server, an anonymous one sending no token and any other
//! taking its token from the CLI's `credentials export`. A `direct` target is refused: Studio
//! never opens storage or starts a server. A `managed` cluster is refused until Studio has its own
//! sign-in: it does not borrow the CLI's managed token (OmniGraph RFC 1017, "One server model" and
//! A2). An address is one server: of the targets on one endpoint, Studio connects as the first by
//! name and names the others, each an identity of its own, as alternatives it does not connect
//! as (`target list --json` does not say which target is the default). The CLI's aliases are not
//! read: the CLI has no command that lists them. Studio lists the targets again when the
//! operator file, a token file a target names, or the CLI where Studio looks for it changes
//! ([`stamp`]); it never writes or parses the file.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;

use crate::cli_tokens;

/// A server a CLI target lands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliServer {
    /// The target Studio connects as: the first by name of the targets on this address.
    pub name: String,
    /// Its endpoint as a server root ([`graph_client::server_root`]), which every target on the
    /// address shares.
    pub url: String,
    /// Its endpoint as the CLI writes it, which a token exported for the target must still be
    /// for.
    pub endpoint: String,
    /// That target's `default_graph`.
    pub graphs: Vec<String>,
    pub auth: CliAuth,
    /// The proxy its `transport.proxy` names (`mode: url`), else none: Studio's own client,
    /// which follows the system's proxy settings.
    pub proxy: Option<String>,
    /// The other targets on the address, by name, which Studio does not connect as.
    pub alternatives: Vec<String>,
    /// What decides its token: its entry as the CLI describes it, and the token file it names as
    /// last written. A server whose revision changes asks the CLI for its token again.
    pub revision: String,
}

/// How a CLI server's requests are authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAuth {
    /// `provider: anonymous`: no token is sent.
    Anonymous,
    /// A token from `credentials export`, kept where this says.
    Token(CliCredential),
}

/// Where a server target's credential is, as `target list --json` describes it. Studio reads
/// none of these itself: it says where a missing one was looked for, keeps the variable an
/// environment source names in the CLI's environment, and watches a token file for a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCredential {
    /// `source: {store: {}}`: the CLI's secure store, which cannot be watched.
    Store,
    /// `source: {env: NAME}`.
    Environment { variable: String },
    /// `source: {file: PATH}`, the path as the CLI resolved it.
    File { path: PathBuf },
    /// `provider: exec`: a credential helper the CLI runs, given this long.
    Helper { timeout: Duration },
}

/// A managed cluster target, as `target inspect NAME --resolved --json` describes it with its
/// profile. Studio refuses it until it has its own sign-in (spec 016 builds that client from
/// these fields); it holds no token for it and never exports one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManagedTarget {
    /// The target's name.
    #[serde(skip)]
    pub name: String,
    /// The profile it is pinned to.
    pub profile: String,
    /// That profile's control-plane `api`.
    #[serde(skip)]
    pub api: String,
    /// That profile's `organization_id`.
    #[serde(skip)]
    pub organization_id: String,
    pub cluster_id: String,
    pub cluster_incarnation: String,
    /// The cluster's data endpoint; none for a registration that is control-only.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub default_graph: Option<String>,
}

/// The profile `target inspect --resolved` expands, with the fields a managed target needs.
#[derive(Debug, Deserialize)]
struct ResolvedProfile {
    api: String,
    organization_id: String,
}

/// A managed target from `target inspect NAME --resolved --json`.
pub(crate) fn managed_from(inspected: &Value) -> Result<ManagedTarget, String> {
    let read =
        |error: serde_json::Error| format!("is described in a form Studio does not read ({error})");
    let name = inspected
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "is described with no name".to_owned())?;
    let target = inspected
        .get("target")
        .filter(|target| target.get("kind").and_then(Value::as_str) == Some("managed"))
        .ok_or_else(|| "is not described as a managed target".to_owned())?;
    let mut managed: ManagedTarget = serde_json::from_value(target.clone()).map_err(read)?;
    let profile: ResolvedProfile = serde_json::from_value(
        inspected
            .get("profile")
            .cloned()
            .ok_or_else(|| "is described without its profile".to_owned())?,
    )
    .map_err(read)?;
    managed.name = name.to_owned();
    managed.api = profile.api;
    managed.organization_id = profile.organization_id;
    Ok(managed)
}

/// A personal alias in the CLI's config: a name bound to a stored query on a target and graph,
/// with default parameter values (spec 007). The CLI has no command that lists its aliases, so
/// Studio knows none ([`crate::ConnectionRegistry::aliases_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliAlias {
    pub name: String,
    pub server: String,
    pub graph: String,
    /// The stored query's name.
    pub query: String,
    /// Fixed parameter values.
    pub params: serde_json::Map<String, Value>,
}

/// `$OMNIGRAPH_HOME`, else `~/.omnigraph`: the operator home the CLI reads from the environment
/// Studio passes on to it.
pub fn operator_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("OMNIGRAPH_HOME")
        && !home.is_empty()
    {
        return Some(expand_tilde(&home));
    }
    std::env::home_dir().map(|home| home.join(".omnigraph"))
}

fn expand_tilde(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// The operator file in `dir`, whose stamp alone Studio reads: a change lists the targets again.
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.yaml")
}

/// What the CLI says of its targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliTargets {
    /// Each server, sorted by name. `None` when the targets could not be listed (no CLI Studio
    /// can drive, or one that refuses its configuration), so the servers read before stand.
    pub servers: Option<Vec<CliServer>>,
    /// What could not be listed or used, each said once.
    pub errors: Vec<String>,
    /// The files whose change can change a server or its token: the operator file, and the token
    /// files the targets name.
    pub watched: Vec<PathBuf>,
    /// The managed targets, refused (in `errors`) but kept as the CLI describes them.
    pub managed: Vec<ManagedTarget>,
}

/// Lists the targets of the operator home in `dir` with the first CLI of `programs` Studio can
/// drive. No directory is no servers.
pub async fn read_targets(dir: Option<&Path>, programs: &[PathBuf]) -> CliTargets {
    let Some(dir) = dir else {
        return CliTargets {
            servers: Some(Vec::new()),
            ..CliTargets::default()
        };
    };
    let config = config_path(dir);
    let refused = |error: String| CliTargets {
        servers: None,
        errors: vec![error],
        watched: vec![config.clone()],
        managed: Vec::new(),
    };
    let cli = match cli_tokens::find_cli(programs).await {
        Ok(cli) => cli,
        Err(why) => {
            return refused(format!(
                "Studio lists the CLI's targets with the OmniGraph CLI, and {why}"
            ));
        }
    };
    match cli_tokens::list_targets(&cli).await {
        Ok(targets) => {
            let mut read = servers_from(&targets);
            read.watched.insert(0, config);
            // A managed target's profile is in no listing: `target inspect --resolved` expands it.
            let mut managed: Vec<&String> = targets
                .iter()
                .filter(|(_, entry)| entry.get("kind").and_then(Value::as_str) == Some("managed"))
                .map(|(name, _)| name)
                .collect();
            managed.sort();
            for name in managed {
                let inspected = cli_tokens::inspect_target(&cli, name, true).await;
                match inspected.and_then(|inspected| managed_from(&inspected)) {
                    Ok(target) => read.managed.push(target),
                    Err(why) => read.errors.push(format!("CLI target '{name}' {why}")),
                }
            }
            read
        }
        Err(refusal) => refused(format!(
            "the OmniGraph CLI ({}, {}) could not list its targets: {refusal}",
            cli.path.display(),
            cli.version
        )),
    }
}

/// How long Studio waits at start for the CLI to list its targets, so the windows a start
/// reopens find the CLI's servers. Past it the targets are listed at the next check.
pub const START_TIMEOUT: Duration = Duration::from_secs(5);

/// [`read_targets`], given [`START_TIMEOUT`] at most.
pub async fn read_targets_at_start(dir: Option<&Path>, programs: &[PathBuf]) -> CliTargets {
    use futures::FutureExt as _;
    let read = read_targets(dir, programs).fuse();
    let timer = smol::Timer::after(START_TIMEOUT).fuse();
    futures::pin_mut!(read, timer);
    futures::select! {
        read = read => read,
        _ = timer => CliTargets {
            servers: None,
            errors: vec![format!(
                "the OmniGraph CLI did not list its targets within {} s of Studio's start; Studio \
                 lists them again",
                START_TIMEOUT.as_secs()
            )],
            watched: Vec::new(),
            managed: Vec::new(),
        },
    }
}

/// What a file's stamp holds: when it was last written, its size, and its permissions.
pub type FileStamp = (SystemTime, u64, u32);

/// When each of `paths` was last written, its size and its permissions (`None` for one that is
/// not there): a stamp that differs from the last one means the targets are listed again. A token
/// file rewritten or rotated (a symlink moved) changes it; so does a program installed where
/// Studio looks for the CLI.
pub fn stamp(paths: &[PathBuf]) -> Vec<Option<FileStamp>> {
    paths
        .iter()
        .map(|path| {
            let metadata = std::fs::metadata(path).ok()?;
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

/// A target as `target list --json` and `target inspect --json` describe it: the fields Studio
/// uses, by kind. Fields Studio does not use are ignored, but not in `transport`, where an
/// unknown one could change how the target is reached.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Entry {
    Server {
        endpoint: String,
        auth: Auth,
        #[serde(default)]
        transport: Option<Transport>,
        #[serde(default)]
        default_graph: Option<String>,
    },
    Managed {},
    Direct {
        scope: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
enum Auth {
    Anonymous,
    Token {
        source: Source,
    },
    Exec {
        #[serde(default)]
        timeout: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Store(serde::de::IgnoredAny),
    Env(String),
    File(PathBuf),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transport {
    #[serde(default)]
    tls: Option<Tls>,
    #[serde(default)]
    proxy: Option<Proxy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tls {
    #[serde(default)]
    ca: Option<Ca>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ca {
    mode: String,
    file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Proxy {
    Direct,
    Url { url: String },
    Environment,
}

/// The servers `targets` (a `target list --json` answer) land on, with what cannot be used and
/// the token files to watch.
pub(crate) fn servers_from(targets: &serde_json::Map<String, Value>) -> CliTargets {
    let mut errors: Vec<String> = Vec::new();
    let mut watched: Vec<PathBuf> = Vec::new();
    let mut landed: Vec<CliServer> = Vec::new();
    let mut names: Vec<&String> = targets.keys().collect();
    names.sort();
    for name in names {
        let value = &targets[name];
        match land(name, value, &mut watched) {
            Ok(server) => landed.push(server),
            Err(why) => errors.push(format!("CLI target '{name}' {why}")),
        }
    }
    CliTargets {
        servers: Some(on_one_address(landed, &mut errors)),
        errors,
        watched,
        managed: Vec::new(),
    }
}

/// The server the target `name`, described as `value`, lands on, or why it lands on none.
fn land(name: &str, value: &Value, watched: &mut Vec<PathBuf>) -> Result<CliServer, String> {
    let entry: Entry = serde_json::from_value(value.clone())
        .map_err(|error| format!("is described in a form Studio does not read ({error})"))?;
    let (endpoint, auth, transport, default_graph) = match entry {
        Entry::Server {
            endpoint,
            auth,
            transport,
            default_graph,
        } => (endpoint, auth, transport, default_graph),
        Entry::Direct { scope } => {
            return Err(format!(
                "opens storage directly (`kind: direct`, scope {scope}); Studio never opens \
                 storage or starts a server, and reaches graphs through server targets only"
            ));
        }
        Entry::Managed {} => {
            return Err(
                "is a managed cluster, which needs Studio's own sign-in; that is not \
                        built yet, and Studio does not borrow the CLI's managed token"
                    .to_owned(),
            );
        }
        Entry::Other => return Err("is of a kind Studio does not know".to_owned()),
    };
    let auth = match auth {
        Auth::Anonymous => CliAuth::Anonymous,
        Auth::Token {
            source: Source::Store(_),
        } => CliAuth::Token(CliCredential::Store),
        Auth::Token {
            source: Source::Env(variable),
        } => CliAuth::Token(CliCredential::Environment { variable }),
        Auth::Token {
            source: Source::File(path),
        } => {
            watched.push(path.clone());
            CliAuth::Token(CliCredential::File { path })
        }
        Auth::Exec { timeout } => {
            let seconds = timeout
                .as_deref()
                .and_then(|timeout| timeout.strip_suffix('s'))
                .and_then(|seconds| seconds.parse::<u64>().ok())
                .unwrap_or(30);
            CliAuth::Token(CliCredential::Helper {
                timeout: Duration::from_secs(seconds),
            })
        }
        Auth::Other => return Err("authenticates in a way Studio does not know".to_owned()),
    };
    if endpoint
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("unix:"))
    {
        return Err(format!(
            "is served on a Unix socket ({endpoint}); Studio's client reaches servers over TCP"
        ));
    }
    let url = graph_client::server_root(&endpoint)
        .map_err(|error| format!("cannot be reached: {error}"))?;
    let proxy = match transport {
        Some(Transport {
            tls: Some(Tls { ca: Some(ca) }),
            ..
        }) => {
            return Err(format!(
                "trusts its own certificate authority (`transport.tls.ca`, mode {}, {}), which \
                 Studio does not support yet; it is not connected rather than reached with other \
                 roots",
                ca.mode,
                ca.file.display()
            ));
        }
        Some(Transport {
            proxy: Some(Proxy::Url { url }),
            ..
        }) => Some(url),
        _ => None,
    };
    let file = match &auth {
        CliAuth::Token(CliCredential::File { path }) => vec![path.clone()],
        _ => Vec::new(),
    };
    Ok(CliServer {
        name: name.to_owned(),
        url,
        endpoint,
        graphs: default_graph.into_iter().collect(),
        auth,
        proxy,
        alternatives: Vec::new(),
        revision: format!("{value} {:?}", stamp(&file)),
    })
}

/// `'a'`, `'a' and 'b'`, or `'a', 'b' and 'c'`, after "CLI target" or "CLI targets".
fn targets_named(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("'{name}'")).collect();
    match quoted.as_slice() {
        [one] => format!("CLI target {one}"),
        [rest @ .., last] => format!("CLI targets {} and {last}", rest.join(", ")),
        [] => String::new(),
    }
}

/// One server per address. Of the targets on one endpoint (taken by name), the first is the one
/// Studio connects as; each other is an identity of its own, which a second connection to the
/// address would need, so it is named as an alternative and not connected.
fn on_one_address(landed: Vec<CliServer>, errors: &mut Vec<String>) -> Vec<CliServer> {
    let mut servers: Vec<CliServer> = Vec::new();
    for server in landed {
        match servers.iter_mut().find(|known| known.url == server.url) {
            Some(known) => known.alternatives.push(server.name),
            None => servers.push(server),
        }
    }
    for server in &servers {
        if server.alternatives.is_empty() {
            continue;
        }
        let one = server.alternatives.len() == 1;
        errors.push(format!(
            "{} {} at {} too, which Studio reaches as '{}', the first by name. Each target is \
             an identity of its own, and Studio keeps one connection per address, so it does not \
             connect as {}",
            targets_named(&server.alternatives),
            if one { "is" } else { "are" },
            server.url,
            server.name,
            if one { "it" } else { "them" },
        ));
    }
    servers
}
