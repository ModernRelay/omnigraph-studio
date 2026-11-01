//! The Omnigraph CLI's targets, as the CLI describes them (spec 003).
//!
//! Studio reads no operator configuration itself: `omnigraph target list --json`, run with the
//! CLI Studio chose ([`crate::cli_tokens`]), says what the targets are, without a server or a
//! secret. A `server` target lands on a server, an anonymous one sending no token and any other
//! taking its token from the CLI's `credentials export`, which gives one only on a Unix system:
//! elsewhere (Windows) a target that takes a token is refused, with why ([`Platform`]). A
//! `direct` target is refused: Studio never opens storage or starts a server. A `managed` cluster
//! with a data endpoint lands on that endpoint. Where the cluster offers a sign-in, Studio signs
//! in itself; elsewhere its token is the data credential the CLI exchanges from the person's
//! `omnigraph login` ([`CliCredential::Managed`]), an interim (spec 016; Omnigraph RFC 1017, A2).
//! An address is one server: of the targets on
//! one endpoint, Studio connects as the first by name and names the others, each an identity of
//! its own, as alternatives it does not connect as (`target list --json` does not say which
//! target is the default). The CLI's aliases are not read: the CLI has no command that lists
//! them. Studio lists the targets again when the operator file, a token file a target names, or
//! the CLI where Studio looks for it changes ([`stamp`]); it never writes or parses the file. A
//! listing is one turn at the CLI, and once the CLI is known `target list --json`, then for each
//! managed target's profile `credentials status --profile NAME --json`, which reads no secret
//! either and says the organization the profile is in.
//! A target's own certificate authority (`transport.tls.ca`) is read with the listing, as the CLI
//! reads it ([`CliRoots`]), and watched like a token file.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// How its requests reach it: the proxy and certificate authority its `transport` names.
    pub transport: CliTransport,
    /// The other targets on the address, by name, which Studio does not connect as.
    pub alternatives: Vec<String>,
    /// What decides its token: its entry as the CLI describes it, and the token file it names as
    /// last written. A server whose revision changes asks the CLI for its token again.
    pub revision: String,
}

/// How a CLI server's requests reach it, from its target's `transport`. The default is Studio's
/// own client, which follows the system's proxy settings and trusts the system's roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliTransport {
    /// The proxy its `transport.proxy` names (`mode: url`), else none.
    pub proxy: Option<String>,
    /// The certificate authority its `transport.tls.ca` names, as read when the targets were
    /// listed, else none.
    pub roots: Option<CliRoots>,
}

/// A target's own certificate authority (`transport.tls.ca`), read as the Omnigraph CLI reads it
/// (`operator_transport.rs`): a regular file of at most [`MAX_CA_BYTES`] holding PEM
/// certificates, at least one. Its bytes are kept, so a server's client is made from the bundle
/// the listing read; a changed file lists the targets again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliRoots {
    /// `mode: replace`: the bundle's certificates are the only roots trusted. `mode: append`:
    /// they are trusted with the system's.
    pub replace: bool,
    /// The bundle, as the CLI resolved its path.
    pub file: PathBuf,
    /// The bundle's bytes.
    pub bundle: Vec<u8>,
}

/// The most a certificate authority bundle may hold, as the CLI reads one: 1 MiB.
pub const MAX_CA_BYTES: u64 = 1024 * 1024;

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
    /// A managed target's cluster data token, which the CLI holds for `profile`: the one it has
    /// cached while that still holds, else a new one from the profile's credential (for a
    /// profile that signs in, the person's `omnigraph login`). Used only where the cluster
    /// offers no sign-in of its own (spec 016): with it Studio acts as the CLI's principal, not
    /// as a client of its own. `organization` is the one the profile names, as `credentials
    /// status --profile` reports it, which a sign-in to the cluster must be in; `None` when it
    /// names none or the CLI would not say, and the server checks it alone.
    Managed {
        profile: String,
        organization: Option<String>,
    },
}

/// Whether the system Studio runs on lets the Omnigraph CLI give it a token. Every command Studio
/// runs to find the CLI and list and check its targets (`version --json`, `target list --json`,
/// `target inspect`, `credentials status`) reads no secret and runs on every system; only
/// `credentials export`, the one way the CLI gives Studio a token, needs a Unix pipe. At Omnigraph
/// main `242ab02f`, its check of its standard output (`commands::targets::stdout_is_pipe`) asks
/// whether `/dev/stdout` is a FIFO on Unix and is `false` on every other system, with no named
/// pipe or other route, so on Windows the CLI refuses every export, whatever Studio connects to
/// its standard output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// A Unix system (macOS, Linux): the CLI exports a token to a pipe.
    Unix,
    /// Any other (Windows): the CLI exports no token, so a server target that takes one is
    /// refused, with why, as a direct or managed target is; an anonymous one is used as on Unix.
    Other,
}

impl Platform {
    /// The system Studio runs on.
    pub const CURRENT: Self = if cfg!(unix) { Self::Unix } else { Self::Other };

    /// Whether the CLI gives Studio a server target's token here (`credentials export`).
    pub fn exports_tokens(self) -> bool {
        self == Self::Unix
    }
}

/// Why Studio uses no server target that takes a token where the CLI exports none
/// ([`Platform::Other`]), and why Add server takes no token there.
pub const PIPE_ONLY: &str = "the Omnigraph CLI gives a token only through a Unix pipe";

/// A managed cluster target, as `target list --json` describes it, kept for Studio's own sign-in
/// (spec 016 builds that client from these fields, and reads the profile's control plane
/// itself). Until then one with a data endpoint also lands as a server whose token the CLI
/// exchanges from its login ([`CliCredential::Managed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedTarget {
    /// The target's name.
    pub name: String,
    /// The profile it is pinned to.
    pub profile: String,
    pub cluster_id: String,
    pub cluster_incarnation: String,
    /// The cluster's data endpoint; none for a registration that is control-only.
    pub endpoint: Option<String>,
    pub default_graph: Option<String>,
}

/// `$OMNIGRAPH_HOME`, else `~/.omnigraph`: the operator home the CLI reads from the environment
/// Studio passes on to it.
pub fn operator_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("OMNIGRAPH_HOME")
        && !home.is_empty()
    {
        return Some(expand_home(&home));
    }
    std::env::home_dir().map(|home| home.join(".omnigraph"))
}

/// `path` with a leading `~/` (or `~\` on Windows) as the person's home folder: how Studio reads
/// a path the person wrote, the operator home and the CLI its settings name alike.
pub fn expand_home(path: &str) -> PathBuf {
    let rest = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("~\\").filter(|_| cfg!(windows)));
    match (rest, std::env::home_dir()) {
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
    /// The files whose change can change a server or its token, the operator file and the
    /// token and certificate authority files the targets name, each with its stamp ([`stamp`])
    /// as the listing saw it: the operator file's taken just before the CLI read it, a token
    /// file's as its target's revision holds it. A later stamp that differs lists the targets
    /// again.
    pub watched: Vec<(PathBuf, Option<FileStamp>)>,
    /// The managed targets, as `target list` describes them, whether they landed or not.
    pub managed: Vec<ManagedTarget>,
}

/// Lists the targets of the operator home in `dir` with the first CLI of `programs` Studio can
/// drive, in one turn at the CLI, as they land on `platform` ([`servers_from`]). No directory is
/// no servers.
pub async fn read_targets(
    dir: Option<&Path>,
    programs: &[PathBuf],
    platform: Platform,
) -> CliTargets {
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
        watched: vec![(config.clone(), stamp_of(&config))],
        managed: Vec::new(),
    };
    let asked = "`omnigraph target list --json`";
    let turn = match cli_tokens::take_turn(&Arc::default(), cli_tokens::READ_TIMEOUT, asked).await {
        Ok(turn) => turn,
        // Not listed, so the file is kept unstamped: the next check lists the targets again.
        Err(busy) => {
            return CliTargets {
                watched: vec![(config.clone(), None)],
                ..refused(format!("Studio could not list the CLI's targets: {busy}"))
            };
        }
    };
    let cli = match cli_tokens::find_cli(programs, &turn).await {
        Ok(cli) => cli,
        Err(why) => {
            return refused(format!(
                "Studio lists the CLI's targets with the Omnigraph CLI, and {why}"
            ));
        }
    };
    // Taken before the CLI reads the file: a write while it lists changes the next stamp.
    let config_stamp = stamp_of(&config);
    match cli_tokens::list_targets(&cli, &turn).await {
        Ok(targets) => {
            let mut read = servers_from(&targets, platform);
            name_organizations(&mut read, &cli, &turn).await;
            read.watched.insert(0, (config, config_stamp));
            read
        }
        Err(refusal) => CliTargets {
            watched: vec![(config.clone(), config_stamp)],
            ..refused(format!(
                "The Omnigraph CLI ({}, {}) could not list its targets: {refusal}",
                cli.path.display(),
                cli.version
            ))
        },
    }
}

/// Names on each managed server of `read` the organization its profile names, as the CLI
/// reports it (`credentials status --profile NAME --json`, once per profile, in the listing's
/// turn): a sign-in to the cluster must be in it (spec 016). A profile whose organization the
/// CLI would not say is a notice, and a sign-in to its clusters is checked by the server alone.
async fn name_organizations(
    read: &mut CliTargets,
    cli: &cli_tokens::FoundCli,
    turn: &cli_tokens::Held,
) {
    let Some(servers) = read.servers.as_mut() else {
        return;
    };
    let mut named: Vec<(String, Option<String>)> = Vec::new();
    for server in servers.iter_mut() {
        let CliAuth::Token(CliCredential::Managed {
            profile,
            organization,
        }) = &mut server.auth
        else {
            continue;
        };
        if let Some((_, known)) = named.iter().find(|(read, _)| read == profile) {
            organization.clone_from(known);
            continue;
        }
        let found = match cli_tokens::profile_organization(cli, turn, profile).await {
            Ok(found) => found,
            Err(why) => {
                read.errors.push(format!(
                    "Studio could not read which organization CLI profile '{profile}' is in \
                     ({why}), so a sign-in to its clusters is checked by the server alone"
                ));
                None
            }
        };
        organization.clone_from(&found);
        named.push((profile.clone(), found));
    }
}

/// The stamp of the one file at `path` ([`stamp`]).
fn stamp_of(path: &Path) -> Option<FileStamp> {
    stamp(&[path.to_owned()]).pop().flatten()
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
    Managed {
        profile: String,
        cluster_id: String,
        cluster_incarnation: String,
        #[serde(default)]
        endpoint: Option<String>,
        #[serde(default)]
        default_graph: Option<String>,
    },
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

/// The servers `targets` (a `target list --json` answer) land on, on `platform`, with what cannot
/// be used, the token files to watch and the managed targets refused.
pub(crate) fn servers_from(
    targets: &serde_json::Map<String, Value>,
    platform: Platform,
) -> CliTargets {
    let mut errors: Vec<String> = Vec::new();
    let mut watched: Vec<(PathBuf, Option<FileStamp>)> = Vec::new();
    let mut landed: Vec<CliServer> = Vec::new();
    let mut managed: Vec<ManagedTarget> = Vec::new();
    let mut names: Vec<&String> = targets.keys().collect();
    names.sort();
    for name in names {
        let value = &targets[name];
        match land(name, value, platform, &mut watched, &mut managed) {
            Ok(server) => landed.push(server),
            Err(why) => errors.push(format!("CLI target '{name}' {why}")),
        }
    }
    CliTargets {
        servers: Some(on_one_address(landed, &mut errors)),
        errors,
        watched,
        managed,
    }
}

/// The server the target `name`, described as `value`, lands on, on `platform`, or why it lands
/// on none. A token or certificate authority file it names is watched, with its stamp; a managed
/// target is kept in `managed`.
fn land(
    name: &str,
    value: &Value,
    platform: Platform,
    watched: &mut Vec<(PathBuf, Option<FileStamp>)>,
    managed: &mut Vec<ManagedTarget>,
) -> Result<CliServer, String> {
    let entry: Entry = serde_json::from_value(value.clone())
        .map_err(|error| format!("is described in a form Studio does not read ({error})"))?;
    let (endpoint, auth, transport, default_graph) = match entry {
        Entry::Server {
            endpoint,
            auth,
            transport,
            default_graph,
        } => (endpoint, cli_auth(auth)?, transport, default_graph),
        Entry::Direct { scope } => {
            return Err(format!(
                "opens storage directly (`kind: direct`, scope {scope}); Studio never opens \
                 storage or starts a server, and reaches graphs through server targets only"
            ));
        }
        Entry::Managed {
            profile,
            cluster_id,
            cluster_incarnation,
            endpoint,
            default_graph,
        } => {
            managed.push(ManagedTarget {
                name: name.to_owned(),
                profile: profile.clone(),
                cluster_id,
                cluster_incarnation,
                endpoint: endpoint.clone(),
                default_graph: default_graph.clone(),
            });
            let Some(endpoint) = endpoint else {
                return Err(
                    "is a managed cluster registered without a data endpoint, so it names no \
                     server for Studio to reach"
                        .to_owned(),
                );
            };
            (
                endpoint,
                // Its organization is read with the listing ([`name_organizations`]).
                CliAuth::Token(CliCredential::Managed {
                    profile,
                    organization: None,
                }),
                None,
                default_graph,
            )
        }
        Entry::Other => return Err("is of a kind Studio does not know".to_owned()),
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
    let (proxy, ca) = match transport {
        Some(Transport { tls, proxy }) => (proxy, tls.and_then(|tls| tls.ca)),
        None => (None, None),
    };
    let proxy = match proxy {
        Some(Proxy::Url { url }) => Some(url),
        _ => None,
    };
    let roots = match ca {
        Some(ca) => {
            // As the CLI: one HTTPS proxy and a destination's own roots are not told apart.
            if let Some(url) = proxy.as_deref().filter(|url| {
                url.get(..8)
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
            }) {
                return Err(format!(
                    "trusts its own certificate authority and goes through the HTTPS proxy \
                     {url}, which the Omnigraph CLI refuses too: it does not keep the two trusts \
                     apart"
                ));
            }
            // Watched before it is read, refused or not: a write after the stamp lists the
            // targets again, and a bundle put right lands the target.
            watched.push((ca.file.clone(), stamp_of(&ca.file)));
            Some(read_roots(&ca)?)
        }
        None => None,
    };
    // Last, so a target refused on every system says that instead, and before its token file is
    // watched, since nothing here reads it.
    if matches!(auth, CliAuth::Token(_)) && !platform.exports_tokens() {
        return Err(format!(
            "takes a token, and {PIPE_ONLY}, so Studio on Windows cannot use this target yet"
        ));
    }
    // The token file's stamp is the one watched: a change after it asks for the token again.
    let file_stamp = match &auth {
        CliAuth::Token(CliCredential::File { path }) => {
            let at = stamp_of(path);
            watched.push((path.clone(), at));
            Some(at)
        }
        _ => None,
    };
    Ok(CliServer {
        name: name.to_owned(),
        url,
        endpoint,
        graphs: default_graph.into_iter().collect(),
        auth,
        transport: CliTransport { proxy, roots },
        alternatives: Vec::new(),
        // The name too: an entry renamed is listed alike, and its token is asked for by name.
        revision: format!("{name} {value} {file_stamp:?}"),
    })
}

/// How a server target's requests are authenticated, from its `auth`.
fn cli_auth(auth: Auth) -> Result<CliAuth, String> {
    Ok(match auth {
        Auth::Anonymous => CliAuth::Anonymous,
        Auth::Token {
            source: Source::Store(_),
        } => CliAuth::Token(CliCredential::Store),
        Auth::Token {
            source: Source::Env(variable),
        } => CliAuth::Token(CliCredential::Environment { variable }),
        Auth::Token {
            source: Source::File(path),
        } => CliAuth::Token(CliCredential::File { path }),
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
    })
}

/// The certificate authority `ca` names, read as the CLI reads it, or why it cannot be used.
fn read_roots(ca: &Ca) -> Result<CliRoots, String> {
    let replace = match ca.mode.as_str() {
        "append" => false,
        "replace" => true,
        other => {
            return Err(format!(
                "trusts its own certificate authority (`transport.tls.ca`) in mode {other}, \
                 which Studio does not know"
            ));
        }
    };
    let file = &ca.file;
    let refused = |why: String| {
        format!(
            "trusts its own certificate authority (`transport.tls.ca`, {}), which {why}",
            file.display()
        )
    };
    let metadata = std::fs::metadata(file)
        .map_err(|error| refused(format!("Studio could not read: {error}")))?;
    if !metadata.is_file() {
        return Err(refused("is not a regular file".into()));
    }
    let mut bundle = Vec::new();
    std::fs::File::open(file)
        .and_then(|opened| opened.take(MAX_CA_BYTES + 1).read_to_end(&mut bundle))
        .map_err(|error| refused(format!("Studio could not read: {error}")))?;
    if bundle.len() as u64 > MAX_CA_BYTES {
        return Err(refused("is over 1 MiB".into()));
    }
    match reqwest::Certificate::from_pem_bundle(&bundle) {
        Ok(certificates) if !certificates.is_empty() => Ok(CliRoots {
            replace,
            file: file.clone(),
            bundle,
        }),
        Ok(_) => Err(refused("holds no certificate".into())),
        Err(_) => Err(refused("is not PEM certificates".into())),
    }
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
            "{} {} the same address as '{}', so Studio connects as '{}' and not as {}",
            targets_named(&server.alternatives),
            if one { "uses" } else { "use" },
            server.name,
            server.name,
            if one { "it" } else { "them" },
        ));
    }
    servers
}
