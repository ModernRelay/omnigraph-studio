//! The Omnigraph CLI, as Studio finds, checks and runs it: for the CLI's targets and their tokens
//! (spec 003), and for the agent, whose CLI the agent panel chooses with the same finder, probe
//! and runner, checking only the home it writes for the agent on top (spec 013).
//!
//! Studio parses no operator configuration and no credential source: what the person's CLI
//! targets are comes from `omnigraph target list --json` and `target inspect NAME --json`, which
//! read the configuration without a server or a secret; whether a target has a credential comes
//! from `credentials status --target NAME --json`, which reads no secret either; and a server
//! target's token comes from `credentials export --target NAME`, which writes the token and a
//! newline to a pipe and refuses a terminal or a file. So an environment variable, a token file,
//! the CLI's secure store and a credential helper are each resolved by Omnigraph's own code
//! (Omnigraph RFC 1017, "Static tokens for clients"). A managed target's token is the cluster
//! data token the CLI holds for its profile: the one it has cached while that still holds,
//! else a new one it asks for (3600 s, which the issuer may shorten; one made with
//! `omnigraph cluster token --ttl` can last a day). `credentials status` takes no managed target,
//! so its export is asked for directly. That is an interim until Studio's own sign-in (spec 016). Studio keeps a token in memory only, never
//! logs or writes it, and sends it only to the endpoint of the target it was exported for. Only
//! the export needs a Unix pipe: on Windows the CLI refuses every export, so Studio there finds
//! the CLI and lists its targets as on Unix, and uses only those that take no token
//! ([`crate::cli_config::Platform`]).
//!
//! Studio looks for the CLI where [`candidates`] says: first the one Studio's settings name
//! (`omnigraph_cli`, [`configured`]), then on `PATH` and in the install folders. A program is the
//! CLI Studio runs when what `version --json` reports is Studio's HTTP contract and
//! [`PROTOCOLS`] at [`PROTOCOL`]; what a program was found
//! to be is remembered across launches until its file changes ([`Probes`], [`probe`]). It runs
//! off the main thread with a timeout, one sequence of commands at a time ([`TURN`]: the CLI
//! refuses a second command on a home whose lock another holds; [`run`]), its standard input
//! empty, and none of Omnigraph's variables ([`omnigraph_variables`]) but `OMNIGRAPH_HOME` and
//! the one a target's own token source names ([`scrubbed`]). A token is asked for when its
//! server connects, once more after a 401 that refused it, on Refresh, and when the target or the
//! CLI changes (`Connection`, `ConnectionRegistry`), not on a timer: the secure store cannot be
//! watched.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::{AsyncReadExt as _, FutureExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli_config::{CliCredential, CliServer, FileStamp};

/// The program's name.
const CLI_NAME: &str = if cfg!(windows) {
    "omnigraph.exe"
} else {
    "omnigraph"
};

/// What the name of every Omnigraph variable starts with, in any ASCII case
/// ([`is_omnigraph_variable`]).
pub const VARIABLE_PREFIX: &str = "OMNIGRAPH_";

/// The protocols `version --json` reports, each of which Studio drives at [`PROTOCOL`].
const PROTOCOLS: &[&str] = &["target", "credential", "access"];

/// The number of each of [`PROTOCOLS`] Studio drives: Omnigraph 0.14.0's, the first that names
/// the version-1 operator configuration, its commands and their answers exactly
/// (ModernRelay/omnigraph#1135, #1133). Earlier builds are not supported; a CLI that reports
/// another number, or none, speaks commands Studio does not know.
const PROTOCOL: u64 = 2;

/// The first Omnigraph release whose CLI Studio drives.
pub const FIRST_CLI_RELEASE: &str = "0.14.0";

/// The CLI's own bound on a token (`credential_sources::MAX_CREDENTIAL_BYTES`, 64 KiB).
const MAX_TOKEN_BYTES: usize = 64 * 1024;

/// How much of a JSON answer, or of what the CLI writes to standard error, is read.
pub const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

/// How long a CLI may take to answer a command that reads only its configuration, and how long a
/// listing waits for its turn at the CLI. In tests (this crate's, and those of a crate built with
/// `test-support`, as the agent panel's are), an hour: a stand-in is a shell script written by
/// the test, whose first run macOS scans before it starts (seconds while builds run, spec 013)
/// and which waits for its turn behind every other test's, so no bound is reached by the machine
/// being busy. Only a test that sets a short timeout of its own, on a stand-in that never
/// answers, reaches one.
pub const READ_TIMEOUT: Duration = if cfg!(any(test, feature = "test-support")) {
    Duration::from_secs(3600)
} else {
    Duration::from_secs(10)
};

/// How long an export may take when no credential helper runs.
pub(crate) const EXPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// How much longer than its helper's own timeout an export may take: the helper is the CLI's to
/// stop, and this is the CLI's start and its own work.
pub(crate) const HELPER_MARGIN: Duration = Duration::from_secs(15);

/// A token, kept in memory only: its `Debug` never shows it.
#[derive(Clone)]
pub struct Token {
    secret: String,
    lease: Option<Lease>,
}

/// How long a short-lived token stands, and whose it is: an OAuth access token's (spec 016). The
/// connection asks for the next before it ends, and the next one of the same holder, from the
/// same source, replaces it quietly, unless a 401 stands or it answers a Refresh, as nothing read
/// with it needs reading again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub expires_at: std::time::SystemTime,
    /// When the next is asked for: a minute before it ends, or halfway through a token that
    /// lives two minutes or less.
    pub renew_at: std::time::SystemTime,
    /// The account the token was issued to.
    pub holder: String,
}

impl Token {
    /// A token the person typed into Studio, handed to the CLI to keep (`credentials set`), or
    /// one the CLI gave: it stands until the server refuses it.
    pub(crate) fn typed(token: String) -> Self {
        Self {
            secret: token,
            lease: None,
        }
    }

    /// A token that stands until `lease` ends.
    pub(crate) fn leased(token: String, lease: Lease) -> Self {
        Self {
            secret: token,
            lease: Some(lease),
        }
    }

    pub(crate) fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    pub(crate) fn into_string(self) -> String {
        self.secret
    }

    #[cfg(test)]
    pub(crate) fn new(token: &str) -> Self {
        Self::typed(token.to_owned())
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(…)")
    }
}

/// Why a token source gave no token, in words the person reads ([`NoToken::why`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoToken {
    pub why: String,
    /// The source could not answer now: the CLI was busy, did not answer in time or could not
    /// be run, or a sign-in's service did not answer. A token it gave before still stands
    /// (`Connection` keeps it while it lives). Otherwise the source answered that it has no
    /// token to give: none is configured, the target changed, the sign-in ended.
    pub passing: bool,
}

impl NoToken {
    /// The source answered that it has no token.
    pub fn lasting(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            passing: false,
        }
    }

    /// The source could not answer now.
    pub fn passing(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            passing: true,
        }
    }
}

impl From<NoToken> for String {
    fn from(no: NoToken) -> Self {
        no.why
    }
}

/// Where a connection's token comes from each time it is asked (`Connection`): on each connect,
/// once more after a 401 that refused its token, on Refresh and when the CLI changes, and before
/// a token with a lease ends ([`Lease`]). A failure says why, and whether it may pass.
/// `hurry` is raised while a window waits for the token: its ask goes before the others
/// waiting for the CLI ([`TURN`]), however long it has waited already.
pub trait TokenSource: Send + Sync + 'static {
    fn token(&self, hurry: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>>;

    /// The CLI profile whose managed token this is, when it is the CLI's own managed data token
    /// rather than a token for the server: said where the connection is shown.
    fn login_profile(&self) -> Option<String> {
        None
    }

    /// The organization a sign-in to the server must be in, when this is a managed target's
    /// login: the one its profile names.
    fn organization(&self) -> Option<String> {
        None
    }

    /// Whether the CLI would not say this login's organization this time: the one its profile
    /// named before stands.
    fn organization_unread(&self) -> bool {
        false
    }

    /// The server refused the last token this source gave (a 401 that refused it, not one that
    /// found none),
    /// said before the source is asked again: a source that keeps a token it can renew renews it
    /// rather than giving the same one back. The CLI's sources keep nothing.
    fn note_refused(&self) {}
}

/// The CLI Studio runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundCli {
    pub path: PathBuf,
    /// Its release, as `version --json` reports it.
    pub version: String,
}

/// The CLI Studio's settings name (`omnigraph_cli`), kept in step with them by the registry
/// (`ConnectionRegistry`): where Studio looks first, for its servers and for the agent alike.
static CONFIGURED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The CLI Studio's settings name (`omnigraph_cli`), its `~` taken as the home folder; `None`
/// when the setting is empty.
pub fn configured() -> Option<PathBuf> {
    CONFIGURED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Takes `setting`, the value of `omnigraph_cli`, as the CLI Studio looks for first.
pub(crate) fn set_configured(setting: Option<&str>) {
    let path = setting
        .filter(|path| !path.is_empty())
        .map(crate::cli_config::expand_home);
    *CONFIGURED.lock().unwrap_or_else(PoisonError::into_inner) = path;
}

/// Where Studio looks for the CLI, in order: the one its settings name ([`configured`]), each
/// directory on `PATH`, then `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin`, where its
/// installers put it. Every place is listed whether or not a program is there, so a stamp of
/// them sees one installed later; [`installed`] takes the programs there.
pub fn candidates() -> Vec<PathBuf> {
    let mut places: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !places.contains(&path) {
            places.push(path);
        }
    };
    if let Some(path) = configured() {
        push(path);
    }
    if let Some(path) = std::env::var_os("PATH") {
        // An empty or relative entry (`.`) names Studio's own working directory, which no one
        // chose to hold the CLI, and a program found there runs again by its relative path:
        // only absolute directories are looked in, as the Windows launcher does.
        for dir in std::env::split_paths(&path).filter(|dir| dir.is_absolute()) {
            push(dir.join(CLI_NAME));
        }
    }
    if let Some(home) = std::env::home_dir() {
        push(home.join(".local/bin").join(CLI_NAME));
    }
    if cfg!(unix) {
        for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
            push(Path::new(dir).join(CLI_NAME));
        }
    }
    places
}

/// The programs among `places` that can be run, in their order, each file once: a place that
/// links to a program listed before it is that program.
pub fn installed(places: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut programs = Vec::new();
    for program in places {
        if !is_executable(program) {
            continue;
        }
        let canonical = std::fs::canonicalize(program).unwrap_or_else(|_| program.clone());
        if seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical);
        programs.push(program.clone());
    }
    programs
}

/// What a program Studio looked at was found to be, its file's stamp then, and what Studio
/// asked of a CLI then ([`requirements`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Probed {
    path: PathBuf,
    stamp: FileStamp,
    /// [`requirements`] when it was probed; an entry kept under other ones, or none (an earlier
    /// Studio's), is probed again.
    #[serde(default)]
    requires: String,
    found: Result<FoundCli, String>,
}

/// What Studio asks of a CLI, in words: a program judged under other ones is judged again, so
/// a Studio that asks more or less than the one that probed it does not take its verdict.
fn requirements() -> String {
    format!(
        "HTTP contract {}, {} protocol {PROTOCOL}",
        graph_client::CONTRACT,
        PROTOCOLS.join(", ")
    )
}

/// What each program Studio looked at was found to be, kept in Studio's data directory
/// (`cli_probes.json`) as well as in memory, so a launch asks a program nothing it asked before:
/// a program is probed again only when its file changes (its modification time, size or
/// permissions), so a start and a connect run only the commands they need. A probe is one
/// command, `version --json`.
struct Probes {
    /// The file was read: at the first look of a run.
    read: bool,
    held: Vec<Probed>,
}

/// How many programs are remembered: the places Studio looks for the CLI, a few times over.
const PROBES_KEPT: usize = 32;

static PROBED: Mutex<Probes> = Mutex::new(Probes {
    read: false,
    held: Vec::new(),
});

fn probes_path() -> PathBuf {
    studio_settings::data_dir().join("cli_probes.json")
}

impl Probes {
    /// The programs remembered, read from the data directory at the first look; a file that
    /// is not there or cannot be read remembers none, and the next probe replaces it.
    fn lock() -> MutexGuard<'static, Probes> {
        let mut probes = PROBED.lock().unwrap_or_else(PoisonError::into_inner);
        if !std::mem::replace(&mut probes.read, true) {
            probes.held = std::fs::read(probes_path())
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
        }
        probes
    }

    /// What `program` was found to be while its file had `stamp`.
    fn find(&self, program: &Path, stamp: FileStamp) -> Option<Result<FoundCli, String>> {
        self.held
            .iter()
            .find(|probed| {
                probed.path == program && probed.stamp == stamp && probed.requires == requirements()
            })
            .map(|probed| probed.found.clone())
    }

    /// Remembers what `program` was found to be while its file had `stamp`, in place of what
    /// it was before, and writes the list on the write thread.
    fn keep(&mut self, program: &Path, stamp: FileStamp, found: Result<FoundCli, String>) {
        self.held.retain(|probed| probed.path != program);
        self.held.push(Probed {
            path: program.to_owned(),
            stamp,
            requires: requirements(),
            found,
        });
        let excess = self.held.len().saturating_sub(PROBES_KEPT);
        self.held.drain(..excess);
        if let Ok(text) = serde_json::to_vec_pretty(&self.held) {
            studio_settings::write_atomic_in_turn(
                probes_path(),
                text,
                studio_settings::Access::Private,
            );
        }
    }
}

/// Why a program is not the CLI Studio runs: its own answer, which stands while its file does
/// not change (`lasting`), or a run that did not end (a timeout, a program that could not
/// start), which is asked again next time.
struct Unfit {
    why: String,
    lasting: bool,
}

impl Unfit {
    fn lasting(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            lasting: true,
        }
    }
}

/// The first of `programs` that is an Omnigraph CLI Studio can drive for its servers
/// ([`probe`]), on every system: only `credentials export` needs a Unix pipe, so where there is
/// none the CLI still lists its targets, and a target that takes a token is refused as it is
/// listed ([`crate::cli_config::Platform`]). Otherwise why none is, naming each program found.
pub(crate) async fn find_cli(programs: &[PathBuf], turn: &Held) -> Result<FoundCli, String> {
    Ok(found_cli(programs, turn).await?)
}

/// [`find_cli`], whose failure may pass when a program's probe did not end: a CLI just
/// replaced can be slow to start its first run.
pub(crate) async fn found_cli(programs: &[PathBuf], turn: &Held) -> Result<FoundCli, NoToken> {
    let mut unfit: Vec<String> = Vec::new();
    let mut passing = false;
    for program in installed(programs) {
        match probed(&program, turn).await {
            Ok(found) => return Ok(found),
            Err(why) => {
                passing |= !why.lasting;
                unfit.push(format!("{} {}", shown(&program), why.why));
            }
        }
    }
    let looked = if unfit.is_empty() {
        "no `omnigraph` is on PATH or in ~/.local/bin, /opt/homebrew/bin or /usr/local/bin"
            .to_owned()
    } else {
        unfit.join("; ")
    };
    Err(NoToken {
        why: format!(
            "Studio found no Omnigraph CLI it can drive (Omnigraph {FIRST_CLI_RELEASE} or \
             later, speaking HTTP contract {} and reporting target, credential and access \
             protocol {PROTOCOL}): {looked}",
            graph_client::CONTRACT
        ),
        passing,
    })
}

/// `program` as the CLI Studio runs: one whose `version --json` reports Studio's HTTP contract
/// and [`PROTOCOLS`] at [`PROTOCOL`]. Otherwise why not, in words that follow its path ("is
/// 0.15.0, whose target protocol 3 Studio does not know"). What it was found to be is
/// remembered while its file is unchanged ([`Probes`]); otherwise it is probed in the turn held.
pub async fn probe(program: &Path, turn: &Held) -> Result<FoundCli, String> {
    probed(program, turn).await.map_err(|unfit| unfit.why)
}

/// [`probe`], saying whether a failure stands: only those that do are remembered.
async fn probed(program: &Path, turn: &Held) -> Result<FoundCli, Unfit> {
    let stamp = crate::cli_config::stamp(&[program.to_owned()])
        .pop()
        .flatten();
    if let Some(found) = stamp.and_then(|stamp| Probes::lock().find(program, stamp)) {
        return found.map_err(Unfit::lasting);
    }
    let probed = probe_now(program, turn).await;
    let lasting = probed.as_ref().err().is_none_or(|unfit| unfit.lasting);
    if lasting && let Some(stamp) = stamp {
        let kept = match &probed {
            Ok(found) => Ok(found.clone()),
            Err(unfit) => Err(unfit.why.clone()),
        };
        Probes::lock().keep(program, stamp, kept);
    }
    probed
}

/// [`probe`], run now: its commands run in the turn held.
async fn probe_now(program: &Path, turn: &Held) -> Result<FoundCli, Unfit> {
    let transient = |unrun: Unrun| Unfit {
        why: match unrun {
            Unrun::Timeout => {
                "did not answer `version --json` in time, and is asked again".to_owned()
            }
            Unrun::Failed(error) => format!("could not be run ({error}), and is asked again"),
        },
        lasting: false,
    };
    let ran = run(
        turn,
        || command(program, &[], &["version", "--json"]),
        READ_TIMEOUT,
        MAX_ANSWER_BYTES,
    )
    .await
    .map_err(transient)?;
    let json = Some(ran)
        .filter(|ran| ran.status.success())
        .and_then(|ran| serde_json::from_slice::<Value>(&ran.stdout).ok())
        .ok_or_else(|| {
            Unfit::lasting(
                "answers no `version --json` (older than Omnigraph 0.14, or not its CLI)",
            )
        })?;
    let version = json
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("a build that names no version")
        .to_owned();
    match json.get("http_contract").and_then(Value::as_str) {
        Some(contract) if contract == graph_client::CONTRACT => {}
        Some(contract) => {
            return Err(Unfit::lasting(format!(
                "is {version}, which speaks HTTP contract {contract}, not Studio's {}",
                graph_client::CONTRACT
            )));
        }
        None => {
            return Err(Unfit::lasting(format!(
                "is {version}, which reports no HTTP contract"
            )));
        }
    }
    for protocol in PROTOCOLS {
        let reported = json.pointer(&format!("/protocols/{protocol}"));
        match reported.and_then(Value::as_u64) {
            Some(PROTOCOL) => {}
            Some(number) => {
                return Err(Unfit::lasting(format!(
                    "is {version}, whose {protocol} protocol {number} Studio does not know"
                )));
            }
            None => {
                return Err(Unfit::lasting(format!(
                    "is {version}, which reports no {protocol} protocol"
                )));
            }
        }
    }
    Ok(FoundCli {
        path: program.to_owned(),
        version,
    })
}

/// The targets `target list --json` names: each name with its entry as the CLI describes it.
/// A failure passes when the CLI did not answer in time or could not be run.
pub(crate) async fn list_targets(
    cli: &FoundCli,
    turn: &Held,
) -> Result<serde_json::Map<String, Value>, NoToken> {
    match json_answer(cli, turn, &[], &["target", "list", "--json"]).await? {
        Value::Object(targets) => Ok(targets),
        _ => Err(NoToken::lasting(
            "`omnigraph target list --json` did not answer an object of targets".to_owned(),
        )),
    }
}

/// The organization the managed profile `profile` names, as `credentials status --profile NAME
/// --json` reports it (`owner.organization_id`), without a service or a secret: `None` when it
/// names none.
pub(crate) async fn profile_organization(
    cli: &FoundCli,
    turn: &Held,
    profile: &str,
) -> Result<Option<String>, String> {
    let args = ["credentials", "status", "--profile", profile, "--json"];
    let answer = json_answer(cli, turn, &[], &args)
        .await
        .map_err(|no| no.why)?;
    match answer.pointer("/owner/organization_id") {
        Some(Value::String(id)) if !id.is_empty() => Ok(Some(id.clone())),
        Some(Value::Null) => Ok(None),
        _ => Err(format!(
            "`omnigraph {}` did not say the profile's organization",
            args.join(" ")
        )),
    }
}

/// What `target inspect NAME --json` says of one target, without a server or a secret.
async fn inspect_target(cli: &FoundCli, turn: &Held, name: &str) -> Result<Value, NoToken> {
    json_answer(cli, turn, &[], &["target", "inspect", name, "--json"]).await
}

/// The digests `target inspect NAME --json` reports for a server target (`protocols.studio`
/// 1): `binding`, of what it resolves to, and `home`, of the operator file; and its default
/// graph. What an `omnigraph-studio://open` link is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDigests {
    pub binding: String,
    pub home: String,
    pub default_graph: Option<String>,
}

/// Reads [`TargetDigests`] of the server target `name` with the first CLI of `programs`
/// Studio can drive, in one turn at the CLI. A target of another kind, or one the CLI does
/// not know, is a refusal in words.
pub async fn target_digests(programs: &[PathBuf], name: &str) -> Result<TargetDigests, String> {
    let asked = format!("`omnigraph target inspect {name} --json`");
    let turn = take_turn(&Arc::default(), READ_TIMEOUT, &asked).await?;
    let cli = find_cli(programs, &turn).await?;
    let inspected = inspect_target(&cli, &turn, name)
        .await
        .map_err(|no| no.why)?;
    let text = |pointer: &str| {
        inspected
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    if text("/target/kind").as_deref() != Some("server") {
        return Err(format!(
            "Target {name} is not a server target, which is all a link opens."
        ));
    }
    match (text("/binding"), text("/home")) {
        (Some(binding), Some(home)) => Ok(TargetDigests {
            binding,
            home,
            default_graph: text("/target/default_graph"),
        }),
        _ => Err(format!(
            "{asked} reported no digests for the target: a CLI of 0.14.1 or later reports them."
        )),
    }
}

/// Adds the server target `name` at `endpoint` to the person's operator configuration through
/// the CLI (`target add NAME`, the target's entry on its standard input): with `token`, kept in
/// the CLI's secure store (`auth: {provider: token, source: {store: {}}}`, then `credentials set
/// --target NAME --stdin --replace` with the token on its standard input); without one,
/// anonymous. Studio writes neither the file nor the token itself. A refusal is the CLI's own
/// words; a target whose token the CLI would not keep is removed again (`target remove`). The
/// commands run in one turn at the CLI.
pub(crate) async fn add_server_target(
    programs: &[PathBuf],
    name: &str,
    endpoint: &str,
    token: Option<Token>,
) -> Result<(), String> {
    let asked = format!("`omnigraph target add {name}`");
    let turn = take_turn(&Arc::default(), READ_TIMEOUT, &asked).await?;
    let cli = find_cli(programs, &turn).await?;
    let auth = match token {
        Some(_) => serde_json::json!({"provider": "token", "source": {"store": {}}}),
        None => serde_json::json!({"provider": "anonymous"}),
    };
    let entry = serde_json::json!({"kind": "server", "endpoint": endpoint, "auth": auth});
    let entry = entry.to_string();
    say(
        &cli,
        &turn,
        &["target", "add", name, "--json"],
        Some(entry.as_bytes()),
    )
    .await?;
    let Some(token) = token else {
        return Ok(());
    };
    let mut input = token.into_string().into_bytes();
    input.push(b'\n');
    let set = [
        "credentials",
        "set",
        "--target",
        name,
        "--stdin",
        "--replace",
    ];
    if let Err(refused) = say(&cli, &turn, &set, Some(&input)).await {
        let removed = say(&cli, &turn, &["target", "remove", name, "--json"], None).await;
        return Err(match removed {
            Ok(()) => format!("{refused}; the target was removed again"),
            Err(left) => format!("{refused}; the target stays, without a token ({left})"),
        });
    }
    Ok(())
}

/// Runs `args` with `cli`, `input` on its standard input, for its success alone: what it printed
/// is not read, and a failure is what it said.
async fn say(
    cli: &FoundCli,
    turn: &Held,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<(), String> {
    let asked = format!("`omnigraph {}`", args.join(" "));
    match run_with(
        turn,
        || command(&cli.path, &[], args),
        input,
        READ_TIMEOUT,
        MAX_ANSWER_BYTES,
    )
    .await
    {
        Ok(ran) if ran.status.success() => Ok(()),
        Ok(ran) => Err(format!(
            "{} ({asked}, {})",
            said(&ran.stderr),
            exit(&ran.status)
        )),
        Err(unrun) => Err(unrun.said(&cli.path, &asked, READ_TIMEOUT)),
    }
}

/// What `cli` prints as JSON for `args`, or what it says when it refuses: a refusal asked for as
/// JSON comes on standard output as `{"error": …, "code": …}`, any other failure on standard
/// error. A run that did not end may pass; an answer does not.
async fn json_answer(
    cli: &FoundCli,
    turn: &Held,
    kept: &[String],
    args: &[&str],
) -> Result<Value, NoToken> {
    let asked = format!("`omnigraph {}`", args.join(" "));
    let ran = match run(
        turn,
        || command(&cli.path, kept, args),
        READ_TIMEOUT,
        MAX_ANSWER_BYTES,
    )
    .await
    {
        Ok(ran) => ran,
        Err(unrun) => {
            return Err(NoToken::passing(unrun.said(
                &cli.path,
                &asked,
                READ_TIMEOUT,
            )));
        }
    };
    let json = serde_json::from_slice::<Value>(&ran.stdout).ok();
    if ran.status.success() {
        return json.ok_or_else(|| {
            NoToken::lasting(format!("the Omnigraph CLI's answer to {asked} is not JSON"))
        });
    }
    let refused = json
        .as_ref()
        .and_then(|json| json.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| said(&ran.stderr));
    Err(NoToken::lasting(format!(
        "{refused} ({asked}, {})",
        exit(&ran.status)
    )))
}

/// A CLI server's token: `credentials status`, then `credentials export`, then `target inspect`
/// for its target, run with the CLI Studio finds, in one turn at the CLI.
#[derive(Debug, Clone)]
pub struct CliTokenSource {
    /// The target's name.
    pub target: String,
    /// Its endpoint as the CLI writes it: the token is kept only when the CLI still names this
    /// endpoint for the target after the export, so it reaches no other server.
    pub endpoint: String,
    /// Where its credential is, as the CLI described the target.
    pub credential: CliCredential,
    /// How long the export may take.
    pub timeout: Duration,
    /// Where to look for the CLI; `None` for where Studio looks ([`candidates`]).
    pub programs: Option<Vec<PathBuf>>,
}

impl CliTokenSource {
    pub(crate) fn for_server(server: &CliServer, credential: &CliCredential) -> Self {
        let timeout = match credential {
            CliCredential::Helper { timeout } => EXPORT_TIMEOUT.max(*timeout + HELPER_MARGIN),
            // An exchange with the control plane, renewing the login when it must.
            CliCredential::Managed { .. } => EXPORT_TIMEOUT + HELPER_MARGIN,
            _ => EXPORT_TIMEOUT,
        };
        Self {
            target: server.name.clone(),
            endpoint: server.endpoint.clone(),
            credential: credential.clone(),
            timeout,
            programs: None,
        }
    }

    /// The Omnigraph variables the CLI keeps for this target: the one its token source names.
    fn kept(&self) -> Vec<String> {
        match &self.credential {
            CliCredential::Environment { variable } => vec![variable.clone()],
            _ => Vec::new(),
        }
    }

    /// The token, its commands run in one turn at the CLI, which waits for the turn as long as
    /// the export may take: the turn goes to this ask before those nobody hurries while
    /// `hurry` is raised.
    async fn resolve(&self, hurry: Arc<AtomicBool>) -> Result<Token, NoToken> {
        let programs = self.programs.clone().unwrap_or_else(candidates);
        let asked = format!("`omnigraph credentials export --target {}`", self.target);
        let turn = take_turn(&hurry, self.timeout, &asked)
            .await
            .map_err(NoToken::passing)?;
        let cli = found_cli(&programs, &turn).await?;
        let target = self.target.as_str();
        let kept = self.kept();

        // Whether a credential is there at all, read without the secret. The CLI takes no
        // managed target here (its status is its profile's, which says nothing of the cluster's
        // data credential), so a managed export is asked for directly.
        let managed = matches!(self.credential, CliCredential::Managed { .. });
        if !managed {
            let status = json_answer(
                &cli,
                &turn,
                &kept,
                &["credentials", "status", "--target", target, "--json"],
            )
            .await?;
            let availability = status.get("availability").and_then(Value::as_str);
            let route = status.pointer("/owner/route").and_then(Value::as_str);
            match availability {
                Some("absent" | "unavailable") => {
                    return Err(NoToken::lasting(self.no_credential()));
                }
                Some("anonymous") => return Err(NoToken::lasting(self.changed())),
                _ if route != Some(self.endpoint.as_str()) => {
                    return Err(NoToken::lasting(self.changed()));
                }
                _ => {}
            }
        }

        let token = self.export(&cli, &turn, &kept).await?;

        // The export read the configuration again: a token for another endpoint, or another
        // kind of target, is dropped.
        let inspected = inspect_target(&cli, &turn, target).await?;
        let kind = if managed { "managed" } else { "server" };
        let still = inspected.pointer("/target/kind").and_then(Value::as_str) == Some(kind)
            && inspected
                .pointer("/target/endpoint")
                .and_then(Value::as_str)
                == Some(self.endpoint.as_str());
        if !still {
            return Err(NoToken::lasting(self.changed()));
        }
        Ok(token)
    }

    /// Runs the export with `cli` and takes the token it prints.
    async fn export(&self, cli: &FoundCli, turn: &Held, kept: &[String]) -> Result<Token, NoToken> {
        let args = ["credentials", "export", "--target", self.target.as_str()];
        let asked = format!("`omnigraph credentials export --target {}`", self.target);
        let ran = match run(
            turn,
            || command(&cli.path, kept, &args),
            self.timeout,
            MAX_TOKEN_BYTES + 1,
        )
        .await
        {
            Ok(ran) => ran,
            Err(Unrun::Timeout) => {
                return Err(NoToken::passing(format!(
                    "the Omnigraph CLI gave no token within {} s ({asked}), and was stopped",
                    self.timeout.as_secs()
                )));
            }
            Err(unrun) => {
                return Err(NoToken::passing(unrun.said(
                    &cli.path,
                    &asked,
                    self.timeout,
                )));
            }
        };
        // Read to its bound (a token's most and its newline) and closed there, an answer longer
        // than a token fills it without the newline, the CLI stopped by its next write: said as
        // too long, whichever way it ended.
        if ran.stdout.len() > MAX_TOKEN_BYTES && ran.stdout.last() != Some(&b'\n') {
            return Err(NoToken::lasting(format!(
                "the Omnigraph CLI's answer to {asked} is longer than a token"
            )));
        }
        if !ran.status.success() {
            // What it printed is never shown: it may be part of a token.
            return Err(NoToken::lasting(format!(
                "{} ({asked}, {})",
                said(&ran.stderr),
                exit(&ran.status)
            )));
        }
        exported_token(ran.stdout).ok_or_else(|| {
            NoToken::lasting(format!(
                "the Omnigraph CLI's answer to {asked} was not one token and a newline"
            ))
        })
    }

    /// Why there is no credential, as `credentials status` found it.
    fn no_credential(&self) -> String {
        let target = &self.target;
        match &self.credential {
            CliCredential::Environment { variable } => format!(
                "No credential is configured for '{target}': its token comes from the variable \
                 {variable}, which is not set in Studio's environment"
            ),
            CliCredential::File { path } => format!(
                "No credential is configured for '{target}': its token file {} is not there",
                path.display()
            ),
            _ => format!(
                "No credential is configured for '{target}' (`omnigraph credentials status \
                 --target {target}`)"
            ),
        }
    }

    fn changed(&self) -> String {
        format!(
            "The CLI target '{}' changed while Studio read its token, which was not kept; Studio \
             reads the CLI's targets again",
            self.target
        )
    }
}

impl TokenSource for CliTokenSource {
    fn token(&self, hurry: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
        let source = self.clone();
        async move { source.resolve(hurry).await }.boxed()
    }

    fn login_profile(&self) -> Option<String> {
        match &self.credential {
            CliCredential::Managed { profile, .. } => Some(profile.clone()),
            _ => None,
        }
    }

    fn organization(&self) -> Option<String> {
        match &self.credential {
            CliCredential::Managed { organization, .. } => organization.clone(),
            _ => None,
        }
    }

    fn organization_unread(&self) -> bool {
        matches!(self.credential, CliCredential::Managed { unread: true, .. })
    }
}

/// The token in what `credentials export` printed: the token and one newline, the token a
/// bearer token (visible ASCII, 64 KiB at most).
fn exported_token(stdout: Vec<u8>) -> Option<Token> {
    let text = String::from_utf8(stdout).ok()?;
    let token = text.strip_suffix('\n')?;
    let valid = !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token.bytes().all(|byte| (0x21..=0x7e).contains(&byte));
    valid.then(|| Token::typed(token.to_owned()))
}

/// `program` with `args`, as Studio starts it ([`scrubbed`]).
fn command(program: &Path, kept: &[String], args: &[&str]) -> std::process::Command {
    let mut command = scrubbed(program, kept, std::env::vars_os().map(|(name, _)| name));
    command.args(args);
    command
}

/// `program` as Studio starts it: with no Omnigraph variable from Studio's environment (whose
/// names are `names`) but `OMNIGRAPH_HOME`, which names the operator home, and those `kept`,
/// which a target's token source reads; and with no colours in what it says.
fn scrubbed(
    program: &Path,
    kept: &[String],
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    let mut keep = vec!["OMNIGRAPH_HOME"];
    keep.extend(kept.iter().map(String::as_str));
    for name in omnigraph_variables(names, &keep) {
        command.env_remove(name);
    }
    command.env("NO_COLOR", "1");
    command
}

/// Whether `name` is one of Omnigraph's variables: it starts with [`VARIABLE_PREFIX`], in any
/// ASCII case. That takes in the CLI's tokens and addressing, tokens kept for other servers
/// (`OMNIGRAPH_MR_BEARER_TOKEN`) and a local server's token list alike, and a variable Omnigraph
/// adds later without a list to keep. Windows reads a name in any case (there
/// `omnigraph_profile` is the profile); elsewhere such a name is taken in too, which removes
/// nothing a program needs.
fn is_omnigraph_variable(name: &str) -> bool {
    name.get(..VARIABLE_PREFIX.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(VARIABLE_PREFIX))
}

/// The Omnigraph variables among `names` that a process Studio starts must not inherit, sorted
/// and each once ([`is_omnigraph_variable`]): every one but those `kept`, matched in any ASCII
/// case too, as Windows reads names. Studio's own CLI keeps its operator home and the variable a
/// target's token source names; the agent keeps those its settings set. A name that is not
/// Unicode is left: it cannot be named on a command line here, and no program reads it by an
/// Omnigraph name, which is.
pub fn omnigraph_variables(
    names: impl IntoIterator<Item = std::ffi::OsString>,
    kept: &[&str],
) -> Vec<String> {
    let mut found: Vec<String> = names
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| is_omnigraph_variable(name))
        .filter(|name| !kept.iter().any(|kept| kept.eq_ignore_ascii_case(name)))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// What a finished run printed ([`run`]).
pub struct Ran {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why a run gave no output ([`run`]).
pub enum Unrun {
    /// It did not end within its timeout, and was stopped.
    Timeout,
    /// It could not be started, or its output read.
    Failed(String),
}

impl Unrun {
    /// What the person reads of a run asked as `asked` with `cli`, past `timeout`.
    fn said(self, cli: &Path, asked: &str, timeout: Duration) -> String {
        match self {
            Unrun::Timeout => format!(
                "the Omnigraph CLI did not answer {asked} within {} s, and was stopped",
                timeout.as_secs()
            ),
            Unrun::Failed(error) => format!(
                "Studio could not run the Omnigraph CLI at {} ({asked}): {error}",
                shown(cli)
            ),
        }
    }
}

/// Studio's turn at the CLI: one sequence of its commands at a time, held for the whole
/// sequence (a token's `credentials status`, `credentials export` and `target inspect`; a
/// listing; a target added). The CLI takes its operator home's locks without waiting
/// (`ConfigEditLock` takes `try_lock`), so two commands at once on one home refuse each other
/// ("could not lock operator configuration"), as every server's token asked together did when
/// the Open dialog connected them all. A turn per command let those sequences interleave, so
/// every token waited for nearly every other's commands. The sequences waiting take the turn in
/// the order they asked, but one a window waits on (its `hurry` raised) goes first.
static TURN: Turn = Turn::new();

/// The queue for [`TURN`].
struct Turn {
    queue: Mutex<Queue>,
}

struct Queue {
    /// A sequence holds the turn.
    held: bool,
    /// Numbers the sequences that waited.
    asked: u64,
    /// The sequences waiting, in the order they asked.
    waiting: Vec<Waiter>,
}

/// A sequence waiting for the turn: given it through `give`.
struct Waiter {
    id: u64,
    hurry: Arc<AtomicBool>,
    give: oneshot::Sender<()>,
}

/// Studio's turn at the CLI, held by one sequence of commands: [`run`] runs a command only in
/// it, and dropped it passes to the next sequence waiting.
pub struct Held(&'static Turn);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.pass();
    }
}

/// A sequence waiting for the turn. Dropped before it took the turn (its caller stopped
/// waiting), it leaves the queue, or, given the turn meanwhile, passes it on: a turn given to a
/// caller gone would otherwise be held by nobody, and every later command wait for it.
struct Asking {
    turn: &'static Turn,
    id: u64,
    /// Dropped after `drop` has run, so a turn given while it runs reaches this receiver.
    given: oneshot::Receiver<()>,
    taken: bool,
}

impl Drop for Asking {
    fn drop(&mut self) {
        if self.taken {
            return;
        }
        let mut queue = self.turn.queue();
        match queue.waiting.iter().position(|waiter| waiter.id == self.id) {
            Some(at) => {
                queue.waiting.remove(at);
            }
            // Out of the queue unasked: given the turn.
            None => {
                drop(queue);
                self.turn.pass();
            }
        }
    }
}

impl Turn {
    const fn new() -> Self {
        Self {
            queue: Mutex::new(Queue {
                held: false,
                asked: 0,
                waiting: Vec::new(),
            }),
        }
    }

    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The turn, once the sequences before this one are done, or `None` past `bound`.
    async fn take(&'static self, hurry: &Arc<AtomicBool>, bound: Duration) -> Option<Held> {
        let mut asking = {
            let mut queue = self.queue();
            if !queue.held {
                queue.held = true;
                return Some(Held(self));
            }
            queue.asked += 1;
            let id = queue.asked;
            let (give, given) = oneshot::channel();
            queue.waiting.push(Waiter {
                id,
                hurry: hurry.clone(),
                give,
            });
            Asking {
                turn: self,
                id,
                given,
                taken: false,
            }
        };
        let given = {
            let given = (&mut asking.given).fuse();
            let waited = smol::Timer::after(bound).fuse();
            futures::pin_mut!(given, waited);
            futures::select! {
                _ = given => true,
                // Given the turn as the bound passed, it is taken all the same.
                _ = waited => !self.queue().waiting.iter().any(|waiter| waiter.id == asking.id),
            }
        };
        if !given {
            return None;
        }
        asking.taken = true;
        Some(Held(self))
    }

    /// Gives the turn to the next sequence waiting, a hurried one first; with none, frees it.
    fn pass(&self) {
        let mut queue = self.queue();
        loop {
            let next = queue
                .waiting
                .iter()
                .position(|waiter| waiter.hurry.load(Ordering::SeqCst))
                .or((!queue.waiting.is_empty()).then_some(0));
            let Some(next) = next else {
                queue.held = false;
                return;
            };
            let waiter = queue.waiting.remove(next);
            if waiter.give.send(()).is_ok() {
                return;
            }
        }
    }

    /// Whether the sequence asked with `hurry` waits for the turn.
    #[cfg(all(test, unix))]
    fn waits_for(&self, hurry: &Arc<AtomicBool>) -> bool {
        self.queue()
            .waiting
            .iter()
            .any(|waiter| Arc::ptr_eq(&waiter.hurry, hurry))
    }
}

/// Forgets the programs probed, as a new launch starts: the next look reads the data
/// directory's file again.
#[cfg(all(test, unix))]
fn forget_probes() {
    let mut probes = PROBED.lock().unwrap_or_else(PoisonError::into_inner);
    probes.read = false;
    probes.held.clear();
}

/// Studio's turn at the CLI for one sequence of commands, waited for `bound` at most: one held
/// back past it by another of Studio's (a credential helper that takes long) is not run, and
/// says so, naming what it would have run (`asked`). While `hurry` is raised the sequence goes
/// before those waiting that nobody hurries.
pub async fn take_turn(
    hurry: &Arc<AtomicBool>,
    bound: Duration,
    asked: &str,
) -> Result<Held, String> {
    TURN.take(hurry, bound).await.ok_or_else(|| {
        format!(
            "the Omnigraph CLI was busy with another of Studio's commands for {} s, so {asked} \
             was not run",
            bound.as_secs()
        )
    })
}

/// What the CLI says when another program holds its operator home's lock
/// (`targets::ConfigEditLock::acquire`).
const LOCK_REFUSED: &str = "could not lock operator configuration";

/// How many times a run the CLI refused for another program's lock is made again, and how long
/// apart: the person's own CLI holds the lock for one command.
const LOCK_RETRIES: u32 = 4;
const LOCK_PAUSE: Duration = Duration::from_millis(250);

/// Runs the command `make` builds, in the turn its sequence holds at the CLI ([`TURN`]), with
/// its standard input empty and its output read from pipes, at most `limit` bytes of standard
/// output and [`MAX_ANSWER_BYTES`] of standard error. Past `timeout` the process is stopped. A
/// run refused for another program's lock is made again, [`LOCK_RETRIES`] times at most. Every
/// command Studio runs to find, check or ask the CLI runs here; the agent's own commands
/// (`omnigraph_run`) do not.
pub async fn run(
    turn: &Held,
    make: impl Fn() -> std::process::Command,
    timeout: Duration,
    limit: usize,
) -> Result<Ran, Unrun> {
    run_with(turn, make, None, timeout, limit).await
}

/// [`run`], with `input` as the command's standard input when there is one: a target's entry
/// for `target add`, a token for `credentials set`, never on its command line.
async fn run_with(
    _turn: &Held,
    make: impl Fn() -> std::process::Command,
    input: Option<&[u8]>,
    timeout: Duration,
    limit: usize,
) -> Result<Ran, Unrun> {
    let mut retries = 0;
    loop {
        let ran = run_once(make(), input, timeout, limit).await?;
        let locked =
            !ran.status.success() && String::from_utf8_lossy(&ran.stderr).contains(LOCK_REFUSED);
        if !locked || retries == LOCK_RETRIES {
            return Ok(ran);
        }
        retries += 1;
        smol::Timer::after(LOCK_PAUSE).await;
    }
}

/// Reads `pipe` into `into` up to `bound` bytes, then closes it with the read: a program that
/// writes more then stops (its next write fails) rather than waiting on a full pipe that stays
/// open. What was read stays in `into` should the read be given up first.
async fn read_into(
    pipe: impl futures::AsyncRead + Unpin,
    bound: usize,
    into: &mut Vec<u8>,
) -> std::io::Result<()> {
    pipe.take(bound as u64).read_to_end(into).await.map(|_| ())
}

/// How long a command's pipes are read after it ends: what it wrote is in them by then, and a
/// program it started that holds them open is not waited for.
const PIPES_AFTER_EXIT: Duration = Duration::from_millis(250);

/// [`run_with`], once.
async fn run_once(
    command: std::process::Command,
    input: Option<&[u8]>,
    timeout: Duration,
    limit: usize,
) -> Result<Ran, Unrun> {
    let mut child = smol::process::Command::from(command)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| Unrun::Failed(error.to_string()))?;
    // Written while the output is read and within the timeout: a token may be larger than a
    // pipe holds, and a command that reads none of it must not hold the CLI's turn for good.
    // The pipe is closed after it, which ends the command's input.
    let stdin = child.stdin.take();
    let write = async move {
        if let (Some(input), Some(mut stdin)) = (input, stdin) {
            use futures::AsyncWriteExt as _;
            stdin.write_all(input).await?;
            drop(stdin);
        }
        Ok::<_, std::io::Error>(())
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(Unrun::Failed("its output could not be read".to_owned()));
    };
    let work = async move {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (written, exited) = {
            let reads = futures::future::join3(
                write,
                read_into(stdout, limit, &mut out),
                read_into(stderr, MAX_ANSWER_BYTES, &mut err),
            )
            .fuse();
            let exit = child.status().fuse();
            futures::pin_mut!(reads, exit);
            futures::select! {
                (written, read_out, read_err) = reads => {
                    read_out.map_err(|error| error.to_string())?;
                    read_err.map_err(|error| error.to_string())?;
                    (written, None)
                }
                // Ended: what it wrote is in its pipes, which a program it started may hold
                // open after it (a credential helper's background process). They are read a
                // moment more, not until that program ends, which would hold the CLI's turn to
                // the timeout.
                status = exit => {
                    let grace = smol::Timer::after(PIPES_AFTER_EXIT).fuse();
                    futures::pin_mut!(grace);
                    let written = futures::select! {
                        (written, read_out, read_err) = reads => {
                            read_out.map_err(|error| error.to_string())?;
                            read_err.map_err(|error| error.to_string())?;
                            written
                        }
                        _ = grace => Ok(()),
                    };
                    (written, Some(status))
                }
            }
        };
        // A command that stopped reading its input (it exited, refused a lock) says why in its
        // output and how it ended: those decide, not the pipe it closed.
        written
            .or_else(|error| match error.kind() {
                std::io::ErrorKind::BrokenPipe => Ok(()),
                _ => Err(error),
            })
            .map_err(|error| error.to_string())?;
        let status = match exited {
            Some(status) => status,
            None => child.status().await,
        }
        .map_err(|error| error.to_string())?;
        Ok::<_, String>(Ran {
            status,
            stdout: out,
            stderr: err,
        })
    }
    .fuse();
    let timer = smol::Timer::after(timeout).fuse();
    futures::pin_mut!(work, timer);
    futures::select! {
        ran = work => ran.map_err(Unrun::Failed),
        _ = timer => Err(Unrun::Timeout),
    }
}

/// `exit 1`, or that the process was stopped.
fn exit(status: &std::process::ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "stopped".to_owned(), |code| format!("exit {code}"))
}

/// What the CLI said when it failed, as one line: its refusal, or the causes of its error
/// report (`Error:`, then numbered lines), each problem document as its title and detail, with
/// its colours taken out. Its note of the target it chose is left out.
fn said(stderr: &[u8]) -> String {
    let text = without_escapes(&String::from_utf8_lossy(stderr));
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let causes: Vec<&str> = match lines.iter().position(|line| line.starts_with("Error:")) {
        Some(at) => std::iter::once(lines[at].trim_start_matches("Error:").trim())
            .chain(lines[at + 1..].iter().map(|line| unnumbered(line)))
            .filter(|line| !line.is_empty())
            .collect(),
        None => lines.last().copied().into_iter().collect(),
    };
    let causes: Vec<String> = causes.into_iter().map(problem).collect();
    if causes.is_empty() {
        return "the Omnigraph CLI failed and said nothing".to_owned();
    }
    let mut line = causes.join("; ");
    const MAX: usize = 400;
    if line.len() > MAX {
        line.truncate(line.floor_char_boundary(MAX));
        line.push('…');
    }
    line
}

/// A cause of an error report without its number (`   0: …`).
fn unnumbered(line: &str) -> &str {
    match line.split_once(':') {
        Some((number, rest))
            if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) =>
        {
            rest.trim()
        }
        _ => line,
    }
}

/// A problem document (`{"title": …, "detail": …}`) as its title and detail; any other line as
/// it is.
fn problem(line: &str) -> String {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(line) else {
        return line.to_owned();
    };
    let field = |name: &str| fields.get(name).and_then(Value::as_str);
    match (field("title"), field("detail")) {
        (Some(title), Some(detail)) => format!("{title}: {detail}"),
        (Some(said), None) | (None, Some(said)) => said.to_owned(),
        (None, None) => line.to_owned(),
    }
}

/// `text` without terminal escape sequences (`ESC [ … letter`), which colour the CLI's errors.
fn without_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// `path` with the home folder written `~`.
fn shown(path: &Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

    use gpui::{AppContext as _, TestAppContext};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    use super::*;
    use crate::cli_config::{CliAuth, Platform, read_targets};
    use crate::connection::{Connection, ConnectionState, ServerEntry, ServerSource};

    /// What `version --json` reports in the Omnigraph 0.14.0 release.
    const VERSION: &str = r#"{"version":"0.14.0","channel":"stable","commit":"60bfd19db780fdff3a5178f959c1204143420114","http_contract":"0.14","protocols":{"target":2,"credential":2,"access":2},"internal_schema":{"writes":16,"serves":[14,16]}}"#;

    /// A stand-in for the Omnigraph CLI: a shell script that answers the commands Studio runs
    /// from files in its directory, and records each call and the Omnigraph variables of each
    /// export. Its export refuses a standard output that is not a pipe, as the CLI's does.
    struct StandIn {
        dir: PathBuf,
        program: PathBuf,
    }

    impl StandIn {
        /// A stand-in named `name`, made under the test's hold on the probe cache: its probes
        /// write the cache's file.
        fn new(_held: &CacheHold, name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("studio-cli-stand-in-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            let program = dir.join("omnigraph");
            let script = format!(
                r#"#!/bin/sh
dir='{dir}'
echo "$*" >> "$dir/calls"
# The CLI's lock of its operator home, taken without waiting, as `ConfigEditLock` takes it.
lock() {{
  [ -f "$dir/locking" ] || return 0
  # Held by another program, which lets go once it has refused one command when asked to.
  mkdir "$dir/lock" 2>/dev/null || {{ [ -f "$dir/release-once" ] && rm "$dir/release-once" && rmdir "$dir/lock"; echo "Error: could not lock operator configuration: lock acquisition failed because the operation would block" >&2; exit 1; }}
  sleep 0.2
  rmdir "$dir/lock"
}}
case "$*" in
  "version --json") cat "$dir/version.json"; exit 0 ;;
  "target add "*" --json")
    [ -f "$dir/refuse-add" ] && {{ printf 'Error: \n   0: \033[91mtarget already exists\033[0m\n' >&2; exit 1; }}
    cat > "$dir/added-$3.json"; echo '{{"registered":"'"$3"'"}}'; exit 0 ;;
  "credentials set --target "*" --stdin --replace")
    [ -f "$dir/refuse-set" ] && {{ printf 'Error: \n   0: set requires provider: token with source: store\n' >&2; exit 1; }}
    cat > "$dir/token-$4.input"; tr -d '\n' < "$dir/token-$4.input" > "$dir/token-$4"; rm "$dir/token-$4.input"
    echo "credential installed; server validity unverified"; exit 0 ;;
  "target remove "*" --json")
    rm -f "$dir/added-$3.json"; echo '{{"removed":"'"$3"'","credentials_preserved":true}}'; exit 0 ;;
  "target list --json")
    if [ -f "$dir/list-refused.json" ]; then cat "$dir/list-refused.json"; exit 2; fi
    cat "$dir/targets.json"; exit 0 ;;
  "target inspect "*)
    file="$dir/inspect-$3.json"
    [ "$5" = "--resolved" ] && file="$dir/inspect-$3-resolved.json"
    cat "$file"; exit 0 ;;
  "credentials status --target "*)
    lock
    echo "target $4 (explicit): server; config=$dir/config.yaml" >&2
    cat "$dir/status-$4.json"; exit 0 ;;
  "credentials status --profile "*)
    lock
    [ -f "$dir/profile-$4.json" ] || {{ echo "Error: profile '$4' is not configured" >&2; exit 1; }}
    echo "profile $4 (explicit): api https://api.example.com; config=$dir/config.yaml" >&2
    cat "$dir/profile-$4.json"; exit 0 ;;
  "credentials export --target "*)
    lock
    env | sed 's/=.*//' | grep '^OMNIGRAPH_' > "$dir/export-env"
    [ -p /dev/stdout ] || {{ echo "credentials export requires stdout to be a pipe; terminals and regular files are refused" >&2; exit 1; }}
    if [ -f "$dir/fail-$4" ]; then cat "$dir/fail-$4"; printf 'Error: \n   0: \033[91m{{"type":"credential_missing","title":"no credential","detail":"no token is stored for %s"}}\033[0m\n' "$4" >&2; exit 1; fi
    [ -f "$dir/move-$4" ] && cp "$dir/move-$4" "$dir/inspect-$4.json"
    [ -f "$dir/never-$4" ] && exec sleep 86400
    cat "$dir/token-$4"; echo; exit 0 ;;
esac
echo "error: unrecognized subcommand '$1'" >&2
exit 64
"#,
                dir = dir.display()
            );
            std::fs::write(&program, script).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            let stand_in = Self { dir, program };
            stand_in.write("version.json", VERSION);
            stand_in
        }

        fn write(&self, file: &str, contents: &str) {
            std::fs::write(self.dir.join(file), contents).unwrap();
        }

        fn calls(&self) -> Vec<String> {
            let calls = std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default();
            std::fs::remove_file(self.dir.join("calls")).ok();
            calls.lines().map(str::to_owned).collect()
        }

        fn programs(&self) -> Vec<PathBuf> {
            vec![self.program.clone()]
        }

        /// A server target `name` at `endpoint` whose token is in the CLI's store, as `target
        /// inspect` and `credentials status` describe it.
        fn server(&self, name: &str, endpoint: &str) {
            self.write(
                &format!("inspect-{name}.json"),
                &format!(
                    r#"{{"name":"{name}","target":{{"kind":"server","endpoint":"{endpoint}","auth":{{"provider":"token","source":{{"store":{{}}}}}}}}}}"#
                ),
            );
            self.status(name, endpoint, "unknown");
        }

        /// The managed profile `name`, in `organization` when it names one, as `credentials
        /// status --profile` describes it.
        fn profile(&self, name: &str, organization: Option<&str>) {
            let organization = organization.map_or("null".to_owned(), |id| format!(r#""{id}""#));
            self.write(
                &format!("profile-{name}.json"),
                &format!(
                    r#"{{"owner":{{"kind":"profile","owner_id":"o","profile_id":"p","route":"https://api.example.com","organization_id":{organization}}},"profile":"{name}","availability":"unknown","identity":"unknown","server_validity":"unverified","observed_at":"2026-10-30T20:19:39Z","verified_at":null,"session":{{"availability":"unknown","identity":"unknown","reason":"noninteractive secure-store inspection is unavailable"}}}}"#
                ),
            );
        }

        fn status(&self, name: &str, endpoint: &str, availability: &str) {
            self.write(
                &format!("status-{name}.json"),
                &format!(
                    r#"{{"owner":{{"kind":"server","owner_id":"o","profile_id":"p","route":"{endpoint}","organization_id":null}},"profile":"{name}","availability":"{availability}","identity":"unknown","server_validity":"unverified","observed_at":"2026-10-22T06:07:34Z","verified_at":null}}"#
                ),
            );
        }

        /// The calls made since the last look, but the probe's (`version --json`).
        fn commands(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|call| call != "version --json")
                .collect()
        }

        fn source(&self, name: &str, endpoint: &str, credential: CliCredential) -> CliTokenSource {
            CliTokenSource {
                target: name.into(),
                endpoint: endpoint.into(),
                credential,
                timeout: READ_TIMEOUT,
                programs: Some(self.programs()),
            }
        }

        /// Every file in its directory that holds `text`, but the stand-in's own store.
        fn files_holding(&self, text: &str) -> Vec<PathBuf> {
            std::fs::read_dir(&self.dir)
                .unwrap()
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    !path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("token-")
                })
                .filter(|path| std::fs::read_to_string(path).is_ok_and(|read| read.contains(text)))
                .collect()
        }
    }

    impl Drop for StandIn {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// The probe cache is the process's, and the tests run at once. Every probe rewrites the
    /// cache's one file with each program the process remembers, so a probe in one test could
    /// replace what another wrote there, or bring back what it forgot ([`forget_probes`]). A
    /// stand-in is therefore made under its test's hold on the cache: shared ([`probing`]), or
    /// whole ([`probe_cache`]) for a test whose assertions need the cache to itself.
    static PROBE_CACHE: RwLock<()> = RwLock::new(());

    /// A test's hold on the probe cache, taken once at its start: a thread that asked for a
    /// second share would wait behind a test waiting for the whole, which waits on the first.
    enum CacheHold {
        Shared(#[expect(dead_code)] RwLockReadGuard<'static, ()>),
        Whole(#[expect(dead_code)] RwLockWriteGuard<'static, ()>),
    }

    fn probing() -> CacheHold {
        CacheHold::Shared(PROBE_CACHE.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn probe_cache() -> CacheHold {
        CacheHold::Whole(PROBE_CACHE.write().unwrap_or_else(PoisonError::into_inner))
    }

    /// The CLI Studio would run of `programs`, found in a turn at the CLI of its own.
    fn found(programs: &[PathBuf]) -> Result<FoundCli, String> {
        smol::block_on(async {
            let turn = take_turn(&Arc::default(), READ_TIMEOUT, "a look for the CLI").await?;
            find_cli(programs, &turn).await
        })
    }

    /// A token from `source`, asked as a connection nobody waits on asks.
    fn token_of(source: &CliTokenSource) -> Result<Token, NoToken> {
        smol::block_on(source.token(Arc::default()))
    }

    /// A CLI is chosen only when it speaks Studio's HTTP contract and reports target,
    /// credential and access protocol 2, as Omnigraph 0.14.0 does; an earlier build is passed
    /// over, and each one passed over is named with why.
    #[test]
    fn a_cli_is_chosen_by_its_contract_and_protocols() {
        let cache = probe_cache();
        let old = StandIn::new(&cache, "old");
        old.write("version.json", "not json");
        let other_contract = StandIn::new(&cache, "other-contract");
        other_contract.write(
            "version.json",
            r#"{"version":"0.15.0","http_contract":"0.15","protocols":{"target":2,"credential":2,"access":1}}"#,
        );
        let no_contract = StandIn::new(&cache, "no-contract");
        no_contract.write(
            "version.json",
            r#"{"version":"0.14.0-dev","protocols":{"target":1,"credential":1,"access":1}}"#,
        );
        let pre_access = StandIn::new(&cache, "pre-access");
        pre_access.write(
            "version.json",
            r#"{"version":"0.14.0-edge+50283ce","http_contract":"0.14","protocols":{"target":2,"credential":2}}"#,
        );
        let unknown = StandIn::new(&cache, "unknown");
        unknown.write(
            "version.json",
            r#"{"version":"0.15.0","http_contract":"0.14","protocols":{"target":3,"credential":2,"access":1}}"#,
        );
        let edge = StandIn::new(&cache, "edge");
        edge.write(
            "version.json",
            r#"{"version":"0.14.0-edge+7fff9df","channel":"edge","http_contract":"0.14","protocols":{"target":1,"credential":1,"access":1}}"#,
        );
        let current = StandIn::new(&cache, "current");
        let missing = old.dir.join("not-there");
        let not_executable = old.dir.join("plain");
        std::fs::write(&not_executable, "#!/bin/sh\n").unwrap();

        let unfit = [
            &old,
            &other_contract,
            &no_contract,
            &pre_access,
            &unknown,
            &edge,
        ];
        let mut programs: Vec<PathBuf> = vec![missing, not_executable];
        programs.extend(unfit.iter().map(|stand_in| stand_in.program.clone()));
        // A place that links to a program listed before it is that program, probed once.
        let linked = old.dir.join("linked");
        std::os::unix::fs::symlink(&edge.program, &linked).unwrap();
        programs.push(linked);
        let why = found(&programs).unwrap_err();
        for said in [
            "answers no `version --json`",
            "is 0.15.0, which speaks HTTP contract 0.15, not Studio's 0.14",
            "is 0.14.0-dev, which reports no HTTP contract",
            "is 0.14.0-edge+50283ce, which reports no access protocol",
            "is 0.15.0, whose target protocol 3 Studio does not know",
            "is 0.14.0-edge+7fff9df, whose target protocol 1 Studio does not know",
            "Omnigraph 0.14.0 or later",
        ] {
            assert!(why.contains(said), "{said}: {why}");
        }
        assert!(
            !why.contains("not-there") && !why.contains("plain") && !why.contains("linked"),
            "{why}"
        );
        assert_eq!(why.matches("whose target protocol 1").count(), 1);

        programs.push(current.program.clone());
        let chosen = found(&programs).unwrap();
        assert_eq!(chosen.path, current.program);
        assert_eq!(chosen.version, "0.14.0");
        // Found again with no probe while the program is unchanged.
        let again = StandIn::new(&cache, "again");
        assert_eq!(found(&again.programs()).unwrap().version, "0.14.0");
        again.calls();
        found(&again.programs()).unwrap();
        assert!(again.calls().is_empty());

        let none = found(&[]).unwrap_err();
        assert!(none.contains("no `omnigraph` is on PATH"), "{none}");
    }

    /// The targets are what `target list --json` says, in the shape the CLI at Omnigraph main
    /// `7fff9dfd` prints: server targets land on servers (an anonymous one with no token, the
    /// others with where their credential is), a managed target with a data endpoint lands on it
    /// with its CLI login as its credential, direct targets and a managed one without an endpoint
    /// are refused, each with why, a managed target is kept as the listing describes it with no
    /// other command, a
    /// target's own certificate authority is read with it and its bundle watched, and a second
    /// target on an address is named as an alternative. Where the CLI exports no token (Windows), the CLI is found and the targets
    /// listed alike, and each server target that takes a token is refused with why. A
    /// configuration the CLI refuses lists nothing, with the CLI's own words.
    #[test]
    fn the_targets_are_listed_as_the_cli_describes_them() {
        let cache = probing();
        let cli = StandIn::new(&cache, "targets");
        let token_file = cli.dir.join("tokens").join("filed");
        let authority = crate::test_tls::Authority::new("strategy");
        let ca_file = cli.dir.join("ca.pem");
        std::fs::write(&ca_file, &authority.pem).unwrap();
        cli.write(
            "targets.json",
            &format!(
                r#"{{
  "cloud": {{"kind": "managed", "profile": "work", "cluster_id": "c1", "cluster_incarnation": "i1", "endpoint": "https://cluster.example.com/", "default_graph": "crm"}},
  "control": {{"kind": "managed", "profile": "work", "cluster_id": "c2", "cluster_incarnation": "i2"}},
  "disk": {{"kind": "direct", "scope": "graph", "uri": "file:///tmp/graph.omni"}},
  "envy": {{"kind": "server", "endpoint": "https://env.example.com/og/", "auth": {{"provider": "token", "source": {{"env": "OMNIGRAPH_STUDIO_TEST_TOKEN"}}}}, "default_graph": "knowledge"}},
  "filed": {{"kind": "server", "endpoint": "https://file.example.com/", "auth": {{"provider": "token", "source": {{"file": "{file}"}}}}}},
  "helped": {{"kind": "server", "endpoint": "https://exec.example.com/", "auth": {{"provider": "exec", "command": ["/usr/bin/false"], "timeout": "5s"}}, "transport": {{"proxy": {{"mode": "url", "url": "http://proxy.example.com:8080/"}}}}}},
  "open": {{"kind": "server", "endpoint": "http://127.0.0.1:8787/", "auth": {{"provider": "anonymous"}}}},
  "rooted": {{"kind": "direct", "scope": "root", "uri": "file:///tmp/graphs", "default_graph": "knowledge"}},
  "strategy": {{"kind": "server", "endpoint": "https://127.0.0.1:8443/", "auth": {{"provider": "token", "source": {{"store": {{}}}}}}, "transport": {{"tls": {{"ca": {{"mode": "append", "file": "{ca}"}}}}, "proxy": {{"mode": "direct"}}}}, "default_graph": "strategy"}},
  "stored": {{"kind": "server", "endpoint": "https://store.example.com/", "auth": {{"provider": "token", "source": {{"store": {{}}}}}}}},
  "stored-too": {{"kind": "server", "endpoint": "https://store.example.com", "auth": {{"provider": "anonymous"}}}}
}}"#,
                file = token_file.display(),
                ca = ca_file.display()
            ),
        );
        cli.profile("work", Some("org_work"));
        let home = cli.dir.join("home");
        let listed = smol::block_on(read_targets(Some(&home), &cli.programs(), Platform::Unix));
        let servers = listed.servers.clone().unwrap();
        let shown: Vec<_> = servers
            .iter()
            .map(|server| {
                (
                    server.name.as_str(),
                    server.url.as_str(),
                    server.graphs.clone(),
                    server.auth.clone(),
                    server.transport.proxy.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            vec![
                (
                    "cloud",
                    "https://cluster.example.com",
                    vec!["crm".to_owned()],
                    CliAuth::Token(CliCredential::Managed {
                        profile: "work".into(),
                        organization: Some("org_work".into()),
                        unread: false,
                    }),
                    None
                ),
                (
                    "envy",
                    "https://env.example.com/og",
                    vec!["knowledge".to_owned()],
                    CliAuth::Token(CliCredential::Environment {
                        variable: "OMNIGRAPH_STUDIO_TEST_TOKEN".into()
                    }),
                    None
                ),
                (
                    "filed",
                    "https://file.example.com",
                    vec![],
                    CliAuth::Token(CliCredential::File {
                        path: token_file.clone()
                    }),
                    None
                ),
                (
                    "helped",
                    "https://exec.example.com",
                    vec![],
                    CliAuth::Token(CliCredential::Helper {
                        timeout: Duration::from_secs(5)
                    }),
                    Some("http://proxy.example.com:8080/")
                ),
                (
                    "open",
                    "http://127.0.0.1:8787",
                    vec![],
                    CliAuth::Anonymous,
                    None
                ),
                (
                    "stored",
                    "https://store.example.com",
                    vec![],
                    CliAuth::Token(CliCredential::Store),
                    None
                ),
                (
                    "strategy",
                    "https://127.0.0.1:8443",
                    vec!["strategy".to_owned()],
                    CliAuth::Token(CliCredential::Store),
                    None
                ),
            ]
        );
        assert_eq!(servers[5].alternatives, ["stored-too"]);
        assert_eq!(servers[1].endpoint, "https://env.example.com/og/");
        assert_eq!(
            servers[6].transport.roots,
            Some(crate::cli_config::CliRoots {
                replace: false,
                file: ca_file.clone(),
                bundle: authority.pem.clone().into_bytes(),
            })
        );
        let watched: Vec<&PathBuf> = listed.watched.iter().map(|(path, _)| path).collect();
        assert_eq!(watched, [&home.join("config.yaml"), &token_file, &ca_file]);
        let said = |name: &str| {
            listed
                .errors
                .iter()
                .find(|error| error.contains(&format!("'{name}'")))
                .cloned()
                .unwrap_or_else(|| panic!("{name}: {:?}", listed.errors))
        };
        assert!(said("disk").contains("opens storage directly"));
        assert!(said("rooted").contains("scope root"));
        assert!(said("control").contains("managed cluster registered without a data endpoint"));
        assert!(said("stored-too").contains("so Studio connects as 'stored'"));
        assert_eq!(listed.errors.len(), 4, "{:?}", listed.errors);
        let managed: Vec<_> = listed
            .managed
            .iter()
            .map(|target| {
                (
                    target.name.as_str(),
                    target.profile.as_str(),
                    target.cluster_id.as_str(),
                    target.cluster_incarnation.as_str(),
                    target.endpoint.as_deref(),
                    target.default_graph.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            managed,
            vec![
                (
                    "cloud",
                    "work",
                    "c1",
                    "i1",
                    Some("https://cluster.example.com/"),
                    Some("crm")
                ),
                ("control", "work", "c2", "i2", None, None),
            ]
        );
        // The managed profile's organization is read once, though two targets name it; neither
        // command reads a credential.
        assert_eq!(
            cli.commands(),
            [
                "target list --json",
                "credentials status --profile work --json"
            ],
        );

        // Where the CLI exports no token (Windows), the same CLI is found and the same targets
        // listed with the same one command: the anonymous ones land, `stored-too` now the one
        // target on its address, and each that takes a token is refused with why, after any
        // refusal that holds on every system, its token file not watched. A bundle is read, and
        // watched, before that refusal, as one that cannot be used is refused on every system.
        let windows = smol::block_on(read_targets(Some(&home), &cli.programs(), Platform::Other));
        let landed: Vec<_> = windows
            .servers
            .clone()
            .unwrap()
            .into_iter()
            .map(|server| (server.name, server.auth, server.alternatives))
            .collect();
        assert_eq!(
            landed,
            [
                ("open".to_owned(), CliAuth::Anonymous, vec![]),
                ("stored-too".to_owned(), CliAuth::Anonymous, vec![]),
            ]
        );
        for name in ["cloud", "envy", "filed", "helped", "strategy", "stored"] {
            let refused = format!(
                "CLI target '{name}' takes a token, and the Omnigraph CLI gives a token only \
                 through a Unix pipe, so Studio on Windows cannot use this target yet"
            );
            assert!(
                windows.errors.contains(&refused),
                "{name}: {:?}",
                windows.errors
            );
        }
        for (name, said) in [
            ("disk", "opens storage directly"),
            ("rooted", "scope root"),
            ("control", "managed cluster"),
        ] {
            assert!(
                windows
                    .errors
                    .iter()
                    .any(|error| error.starts_with(&format!("CLI target '{name}'"))
                        && error.contains(said)),
                "{name}: {:?}",
                windows.errors
            );
        }
        assert_eq!(windows.errors.len(), 9, "{:?}", windows.errors);
        assert_eq!(windows.managed, listed.managed);
        let watched: Vec<&PathBuf> = windows.watched.iter().map(|(path, _)| path).collect();
        assert_eq!(watched, [&home.join("config.yaml"), &ca_file]);
        assert_eq!(cli.commands(), ["target list --json"]);

        // The CLI's own refusal of an old-format file, as it answers `--json`.
        cli.write(
            "list-refused.json",
            r#"{"error":"target_config_invalid: operator config '/h/config.yaml': legacy target_config_version is unsupported; write a version: 1 operator document using docs/user/cli/targets.md#upgrade-from-the-old-format; there is no automatic importer","code":"target_config_invalid"}"#,
        );
        let refused = smol::block_on(read_targets(Some(&home), &cli.programs(), Platform::Unix));
        assert_eq!(refused.servers, None);
        assert_eq!(refused.errors.len(), 1);
        assert!(
            refused.errors[0].contains("legacy target_config_version is unsupported")
                && refused.errors[0].contains("upgrade-from-the-old-format"),
            "{:?}",
            refused.errors
        );
        // A refusal lasts: the targets are listed again when the operator file changes.
        let config = home.join("config.yaml");
        let stamped = crate::cli_config::stamp(std::slice::from_ref(&config))
            .pop()
            .flatten();
        assert_eq!(refused.watched, [(config.clone(), stamped)]);

        // A CLI that could not be run may run at the next check, which lists the targets again.
        let unrunnable = cli.dir.join("unrunnable");
        std::fs::write(&unrunnable, "#!/nonexistent/interpreter\n").unwrap();
        std::fs::set_permissions(&unrunnable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let passing = smol::block_on(read_targets(Some(&home), &[unrunnable], Platform::Unix));
        assert_eq!(passing.servers, None);
        assert!(
            passing.errors[0].contains("could not be run"),
            "{:?}",
            passing.errors
        );
        assert_eq!(
            passing.watched,
            crate::cli_config::unlisted(&config, String::new()).watched
        );
    }

    /// A target's own certificate authority is read as the CLI reads it: a regular file of at
    /// most 1 MiB holding PEM certificates, at least one, trusted with the system's roots
    /// (`append`) or alone (`replace`), and never with an HTTPS proxy. Every bundle a target
    /// names is watched, one that cannot be used too, so putting it right lands its target.
    /// A managed profile the CLI cannot describe still lands its clusters, on the CLI's login
    /// alone, marked unread, and says how its sign-ins are checked; one in no organization names
    /// none, and is read.
    #[test]
    fn a_managed_profile_the_cli_cannot_describe_is_said() {
        let cache = probing();
        let cli = StandIn::new(&cache, "profiles");
        cli.write(
            "targets.json",
            r#"{
  "lost": {"kind": "managed", "profile": "gone", "cluster_id": "c1", "cluster_incarnation": "i1", "endpoint": "https://lost.example.com/"},
  "solo": {"kind": "managed", "profile": "own", "cluster_id": "c2", "cluster_incarnation": "i2", "endpoint": "https://solo.example.com/"}
}"#,
        );
        cli.profile("own", None);
        let home = cli.dir.join("home");
        let listed = smol::block_on(read_targets(Some(&home), &cli.programs(), Platform::Unix));
        let landed: Vec<_> = listed
            .servers
            .unwrap()
            .into_iter()
            .map(|server| (server.name, server.auth))
            .collect();
        assert_eq!(
            landed,
            [
                (
                    "lost".to_owned(),
                    CliAuth::Token(CliCredential::Managed {
                        profile: "gone".into(),
                        organization: None,
                        unread: true,
                    })
                ),
                (
                    "solo".to_owned(),
                    CliAuth::Token(CliCredential::Managed {
                        profile: "own".into(),
                        organization: None,
                        unread: false,
                    })
                ),
            ]
        );
        assert_eq!(listed.errors.len(), 1, "{:?}", listed.errors);
        assert!(
            listed.errors[0]
                .starts_with("Studio could not read which organization CLI profile 'gone' is in (")
                && listed.errors[0].contains("profile 'gone' is not configured")
                && listed.errors[0]
                    .ends_with("checked against the one read before, else by the server alone"),
            "{}",
            listed.errors[0]
        );
    }

    #[test]
    fn a_targets_certificate_authority_is_read_as_the_cli_reads_it() {
        let dir = std::env::temp_dir().join(format!("studio-ca-bundles-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        let pem = crate::test_tls::Authority::new("bundle").pem;
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let good = file("good.pem", pem.as_bytes());
        let large = file(
            "large.pem",
            &vec![b'a'; crate::cli_config::MAX_CA_BYTES as usize + 1],
        );
        let broken = file(
            "broken.pem",
            b"-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n",
        );
        let key = file(
            "key.pem",
            rcgen::KeyPair::generate()
                .unwrap()
                .serialize_pem()
                .as_bytes(),
        );
        let missing = dir.join("missing.pem");
        let folder = dir.join("folder");
        let target = |name: &str, mode: &str, path: &Path, proxy: Option<&str>| {
            let mut transport =
                serde_json::json!({ "tls": { "ca": { "mode": mode, "file": path } } });
            if let Some(url) = proxy {
                transport["proxy"] = serde_json::json!({ "mode": "url", "url": url });
            }
            (
                name.to_owned(),
                serde_json::json!({
                    "kind": "server",
                    "endpoint": format!("https://{name}.example.com/"),
                    "auth": { "provider": "anonymous" },
                    "transport": transport,
                }),
            )
        };
        let targets: serde_json::Map<String, serde_json::Value> = [
            target("append", "append", &good, None),
            target("replace", "replace", &good, None),
            target(
                "proxied",
                "append",
                &good,
                Some("http://proxy.example.com:8080/"),
            ),
            target("missing", "append", &missing, None),
            target("folder", "append", &folder, None),
            target("large", "append", &large, None),
            target("broken", "append", &broken, None),
            target("keyonly", "replace", &key, None),
            target("merged", "merge", &good, None),
            target(
                "secured",
                "append",
                &good,
                Some("https://proxy.example.com/"),
            ),
        ]
        .into_iter()
        .collect();
        let listed = crate::cli_config::servers_from(&targets, Platform::Unix);
        let landed: Vec<_> = listed
            .servers
            .unwrap()
            .into_iter()
            .map(|server| {
                let roots = server.transport.roots.unwrap();
                assert_eq!(
                    (&roots.file, &roots.bundle),
                    (&good, &pem.clone().into_bytes())
                );
                (server.name, roots.replace, server.transport.proxy)
            })
            .collect();
        assert_eq!(
            landed,
            [
                ("append".to_owned(), false, None),
                (
                    "proxied".to_owned(),
                    false,
                    Some("http://proxy.example.com:8080/".to_owned())
                ),
                ("replace".to_owned(), true, None),
            ]
        );
        let said = |name: &str| {
            listed
                .errors
                .iter()
                .find(|error| error.starts_with(&format!("CLI target '{name}' ")))
                .cloned()
                .unwrap_or_else(|| panic!("{name}: {:?}", listed.errors))
        };
        for (name, why) in [
            ("missing", "which Studio could not read"),
            ("folder", "which is not a regular file"),
            ("large", "which is over 1 MiB"),
            ("broken", "which is not PEM certificates"),
            ("keyonly", "which holds no certificate"),
            ("merged", "in mode merge, which Studio does not know"),
            (
                "secured",
                "goes through the HTTPS proxy https://proxy.example.com/",
            ),
        ] {
            assert!(said(name).contains(why), "{name}: {}", said(name));
        }
        assert_eq!(listed.errors.len(), 7, "{:?}", listed.errors);
        // By name, each bundle as its target names it; the HTTPS proxy is refused before its
        // bundle is looked at.
        let watched: Vec<&PathBuf> = listed.watched.iter().map(|(path, _)| path).collect();
        assert_eq!(
            watched,
            [
                &good, &broken, &folder, &key, &large, &good, &missing, &good, &good
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A token comes from `credentials export` through a pipe, after `credentials status` found
    /// one there, and stands only while `target inspect` still names the endpoint it was asked
    /// for. The CLI runs with no Omnigraph variable but `OMNIGRAPH_HOME` and the one its target
    /// names, and the token reaches no file, error or debug output.
    #[test]
    fn a_token_is_exported_through_a_pipe() {
        let cache = probing();
        let cli = StandIn::new(&cache, "export");
        cli.server("stored", "https://store.example.com/");
        cli.write("token-stored", "secret-stored-token");
        let source = cli.source("stored", "https://store.example.com/", CliCredential::Store);
        let token = token_of(&source).unwrap();
        assert_eq!(format!("{token:?}"), "Token(…)");
        assert_eq!(token.into_string(), "secret-stored-token");
        assert_eq!(
            cli.commands(),
            [
                "credentials status --target stored --json",
                "credentials export --target stored",
                "target inspect stored --json",
            ]
        );
        let exported = std::fs::read_to_string(cli.dir.join("export-env")).unwrap();
        assert!(
            exported.lines().all(|name| name == "OMNIGRAPH_HOME"),
            "{exported}"
        );
        assert!(cli.files_holding("secret-stored-token").is_empty());

        // The target moved to another endpoint while its token was read: the token is dropped.
        cli.server("moving", "https://store.example.com/");
        cli.write("token-moving", "secret-moving-token");
        cli.write(
            "move-moving",
            r#"{"name":"moving","target":{"kind":"server","endpoint":"https://elsewhere.example.com/","auth":{"provider":"token","source":{"store":{}}}}}"#,
        );
        let moved =
            token_of(&cli.source("moving", "https://store.example.com/", CliCredential::Store))
                .unwrap_err();
        assert!(!moved.passing, "a moved target is an answer");
        let moved = moved.why;
        assert!(
            moved.contains("changed while Studio read its token"),
            "{moved}"
        );
        assert!(!moved.contains("secret-moving-token"));

        // An answer longer than a token is read to its bound and said as too long, not as
        // something else the closed pipe made of it.
        cli.server("huge", "https://store.example.com/");
        cli.write("token-huge", &"a".repeat(4 * MAX_TOKEN_BYTES));
        let huge =
            token_of(&cli.source("huge", "https://store.example.com/", CliCredential::Store))
                .unwrap_err();
        assert!(!huge.passing, "a token too long is an answer");
        assert!(huge.why.contains("is longer than a token"), "{}", huge.why);
    }

    /// A managed target's token is the data credential the CLI exchanges from its login, asked
    /// for with `credentials export` alone, since `credentials status` takes no managed target,
    /// and kept only while `target inspect` still names that managed target at that endpoint.
    /// The source says whose login it carries, its organization, and whether the CLI would not
    /// say it this time; a server target's says none.
    #[test]
    fn a_managed_targets_token_is_exchanged_from_the_clis_login() {
        let cache = probing();
        let cli = StandIn::new(&cache, "managed");
        let endpoint = "https://c1-i1.graphs.example.com/";
        cli.write(
            "inspect-cloud.json",
            &format!(
                r#"{{"name":"cloud","target":{{"kind":"managed","profile":"work","cluster_id":"c1","cluster_incarnation":"i1","endpoint":"{endpoint}"}}}}"#
            ),
        );
        cli.write("token-cloud", "secret-data-token");
        let source = cli.source(
            "cloud",
            endpoint,
            CliCredential::Managed {
                profile: "work".into(),
                organization: Some("org_work".into()),
                unread: false,
            },
        );
        assert_eq!(source.login_profile().as_deref(), Some("work"));
        assert_eq!(source.organization().as_deref(), Some("org_work"));
        assert!(!source.organization_unread());
        let missed = cli.source(
            "cloud",
            endpoint,
            CliCredential::Managed {
                profile: "work".into(),
                organization: None,
                unread: true,
            },
        );
        assert!(missed.organization_unread(), "the CLI would not say");
        assert_eq!(
            token_of(&source).unwrap().into_string(),
            "secret-data-token"
        );
        assert_eq!(
            cli.commands(),
            [
                "credentials export --target cloud",
                "target inspect cloud --json",
            ]
        );
        assert!(cli.files_holding("secret-data-token").is_empty());

        // Turned into a server target at the same address while its token was read: dropped.
        cli.write(
            "move-cloud",
            &format!(
                r#"{{"name":"cloud","target":{{"kind":"server","endpoint":"{endpoint}","auth":{{"provider":"anonymous"}}}}}}"#
            ),
        );
        let turned = token_of(&source).unwrap_err();
        assert!(!turned.passing, "a changed target is an answer");
        let turned = turned.why;
        assert!(
            turned.contains("changed while Studio read its token"),
            "{turned}"
        );
        assert!(!turned.contains("secret-data-token"));
        let stored = cli.source("stored", "https://store.example.com/", CliCredential::Store);
        assert_eq!(stored.login_profile(), None);
    }

    /// A refused export says the CLI's reason, without its colours or its note of the target,
    /// and never what it printed; a credential `credentials status` finds missing is said
    /// without an export; an export that never answers is stopped past its time.
    #[test]
    fn a_refused_export_says_why_and_never_the_token() {
        let cache = probing();
        let cli = StandIn::new(&cache, "refused");
        cli.server("stored", "https://store.example.com/");
        cli.write("fail-stored", "secret-half-printed");
        let why =
            token_of(&cli.source("stored", "https://store.example.com/", CliCredential::Store))
                .unwrap_err();
        assert!(!why.passing, "a refusal is an answer");
        let why = why.why;
        assert_eq!(
            why,
            "no credential: no token is stored for stored (`omnigraph credentials export \
             --target stored`, exit 1)"
        );
        assert!(!why.contains("secret-half-printed"));

        cli.server("filed", "https://file.example.com/");
        cli.status("filed", "https://file.example.com/", "unavailable");
        cli.calls();
        let missing = token_of(&cli.source(
            "filed",
            "https://file.example.com/",
            CliCredential::File {
                path: "/tokens/filed".into(),
            },
        ))
        .unwrap_err();
        assert_eq!(
            missing,
            NoToken::lasting(
                "No credential is configured for 'filed': its token file /tokens/filed is not \
                 there"
            )
        );
        assert!(
            cli.calls()
                .iter()
                .all(|call| !call.starts_with("credentials export --target"))
        );

        // Run in a turn the test holds, waited for as long as other tests' commands take, so
        // only the export's own timeout can end it: the stand-in never answers.
        cli.server("slow", "https://slow.example.com/");
        cli.write("token-slow", "secret-slow");
        cli.write("never-slow", "");
        let mut slow = cli.source("slow", "https://slow.example.com/", CliCredential::Store);
        slow.timeout = Duration::from_millis(300);
        let stopped = smol::block_on(async {
            let turn = take_turn(&Arc::default(), READ_TIMEOUT, "a test's export")
                .await
                .map_err(NoToken::passing)?;
            let found = found_cli(&cli.programs(), &turn).await?;
            slow.export(&found, &turn, &[])
                .await
                .map(Token::into_string)
        })
        .unwrap_err();
        assert_eq!(
            stopped,
            NoToken::passing(
                "the Omnigraph CLI gave no token within 0 s (`omnigraph credentials export \
                 --target slow`), and was stopped"
            )
        );
    }

    /// The CLI takes its operator home's lock without waiting, so two of its commands at once
    /// refuse each other. Every server's token asked together, as the Open dialog asks them,
    /// each from a thread of its own, takes its turn and is given, its three commands run
    /// together; a lock another program holds is waited out a bounded number of times, then
    /// said.
    #[test]
    fn tokens_asked_together_take_turns_at_the_cli() {
        let cache = probing();
        let cli = StandIn::new(&cache, "turns");
        cli.write("locking", "");
        let names = ["a", "b", "c", "d"];
        for name in names {
            let endpoint = format!("https://{name}.example.com/");
            cli.server(name, &endpoint);
            cli.write(&format!("token-{name}"), &format!("secret-{name}"));
        }
        let threads: Vec<_> = names
            .iter()
            .map(|name| {
                let source = cli.source(
                    name,
                    &format!("https://{name}.example.com/"),
                    CliCredential::Store,
                );
                std::thread::spawn(move || {
                    futures::executor::block_on(source.token(Arc::default()))
                        .map(Token::into_string)
                })
            })
            .collect();
        let given: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            given,
            names.map(|name| Ok(format!("secret-{name}"))).to_vec()
        );
        // One target's status, export and inspect, then the next's: none waits for the
        // others' commands between its own.
        let commands = cli.commands();
        let mut asked: Vec<String> = Vec::new();
        for run in commands.chunks(3) {
            let target = run[1].rsplit(' ').next().unwrap_or_default().to_owned();
            assert_eq!(
                run,
                [
                    format!("credentials status --target {target} --json"),
                    format!("credentials export --target {target}"),
                    format!("target inspect {target} --json"),
                ],
                "{commands:?}"
            );
            asked.push(target);
        }
        asked.sort();
        assert_eq!(asked, names, "{commands:?}");

        // Another program holds the lock, and lets go once it has refused one command, which is
        // made again; then it holds it for good, and every try is refused.
        let held = cli.dir.join("lock");
        std::fs::create_dir(&held).unwrap();
        cli.write("release-once", "");
        let source = cli.source("a", "https://a.example.com/", CliCredential::Store);
        assert_eq!(
            token_of(&source).map(Token::into_string),
            Ok("secret-a".to_owned())
        );
        let status = "credentials status --target a --json";
        assert_eq!(
            cli.commands(),
            [
                status,
                status,
                "credentials export --target a",
                "target inspect a --json"
            ]
        );
        std::fs::create_dir(&held).unwrap();
        let refused = token_of(&source).unwrap_err();
        assert!(!refused.passing, "the CLI answered");
        assert!(refused.why.contains(LOCK_REFUSED), "{refused:?}");
        assert_eq!(cli.commands().len(), 1 + LOCK_RETRIES as usize);
    }

    /// A command's input is written within its timeout: one that reads none of an input larger
    /// than a pipe holds is stopped when its time is up, not waited on for good, and one that
    /// ends without reading it is judged by its own answer.
    #[test]
    fn input_a_command_does_not_read_ends_at_its_timeout() {
        let dir =
            std::env::temp_dir().join(format!("studio-cli-unread-input-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("deaf");
        std::fs::write(&program, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let input = vec![b'x'; 1 << 20];
        let ran = smol::block_on(run_once(
            std::process::Command::new(&program),
            Some(&input),
            Duration::from_millis(500),
            1024,
        ));
        assert!(matches!(ran, Err(Unrun::Timeout)));

        // One that refuses and ends without reading it is judged by what it said. The timeout
        // leaves room for macOS to scan the new script on its first run, which can take
        // seconds while builds run: at 10 s a loaded machine ran it out.
        let refusing = dir.join("refusing");
        std::fs::write(&refusing, "#!/bin/sh\necho 'lock refused' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&refusing, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ran = smol::block_on(run_once(
            std::process::Command::new(&refusing),
            Some(&input),
            Duration::from_secs(90),
            1024,
        ));
        let ran = match ran {
            Ok(ran) => ran,
            Err(Unrun::Timeout) => panic!("the command's own answer, not a timeout"),
            Err(Unrun::Failed(error)) => panic!("the command's own answer, not: {error}"),
        };
        assert!(!ran.status.success());
        assert!(String::from_utf8_lossy(&ran.stderr).contains("lock refused"));

        // One that leaves a program holding its output open (a credential helper's background
        // process) is done when it ends, with what it wrote. Before, the run waited for that
        // program, holding the CLI's turn up to the timeout.
        let leaving = dir.join("leaving");
        std::fs::write(
            &leaving,
            "#!/bin/sh
(sleep 60) &
echo token
",
        )
        .unwrap();
        std::fs::set_permissions(&leaving, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let ran = smol::block_on(run_once(
            std::process::Command::new(&leaving),
            None,
            Duration::from_secs(90),
            1024,
        ))
        .unwrap_or_else(|_| panic!("the command's own answer"));
        // Far under the background program's minute, which a run waiting for it would last
        // within its timeout, with room for a first run's scan.
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        assert!(ran.status.success());
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "token\n");

        // One that writes past its bound is stopped by the pipe closing at it, not left
        // blocked on a full pipe until the timeout. Before, the pipe stayed open past the
        // bound, and such a command ran out the timeout.
        let flooding = dir.join("flooding");
        std::fs::write(
            &flooding,
            "#!/bin/sh\nhead -c 4000000 /dev/zero\necho done >&2\n",
        )
        .unwrap();
        std::fs::set_permissions(&flooding, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let ran = smol::block_on(run_once(
            std::process::Command::new(&flooding),
            None,
            Duration::from_secs(60),
            1024,
        ));
        let ran = match ran {
            Ok(ran) => ran,
            Err(Unrun::Timeout) => panic!("stopped at its bound, not at the timeout"),
            Err(Unrun::Failed(error)) => panic!("its own answer, not: {error}"),
        };
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(ran.stdout.len(), 1024, "read to its bound");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A program that could not be run, or did not answer in time, is asked again at the next
    /// look; only its own answer is kept while its file does not change.
    #[test]
    fn a_program_that_did_not_answer_is_asked_again() {
        let cache = probing();
        let cli = StandIn::new(&cache, "transient");
        // Runs through an interpreter that is not there yet, so it cannot start.
        let shell = cli.dir.join("shell");
        let program = cli.dir.join("late");
        let script = std::fs::read_to_string(&cli.program).unwrap().replacen(
            "#!/bin/sh",
            &format!("#!{}", shell.display()),
            1,
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let programs = vec![program.clone()];
        let first = found(&programs).unwrap_err();
        assert!(first.contains("could not be run"), "{first}");
        std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
        assert_eq!(found(&programs).unwrap().path, program);
    }

    /// What a program was found to be is remembered in Studio's data directory, so the next
    /// launch asks it nothing while its file is unchanged; a program rewritten (upgraded) is
    /// asked again.
    #[test]
    fn a_probe_is_remembered_across_launches() {
        let cache = probe_cache();
        let cli = StandIn::new(&cache, "remembered");
        assert!(found(&cli.programs()).is_ok());
        assert_eq!(cli.calls(), ["version --json"]);
        // The next launch: nothing in memory, the data directory's file read again.
        studio_settings::writes_done();
        let kept = std::fs::read_to_string(probes_path()).unwrap();
        assert!(kept.contains(r#""0.14.0""#), "{kept}");
        forget_probes();
        assert!(found(&cli.programs()).is_ok());
        assert!(cli.calls().is_empty(), "asked nothing");

        // Upgraded in place: its stamp changed, so it is asked again.
        let script = std::fs::read_to_string(&cli.program).unwrap();
        std::fs::write(&cli.program, format!("{script}\n# upgraded\n")).unwrap();
        assert!(found(&cli.programs()).is_ok());
        assert_eq!(cli.calls(), ["version --json"]);

        // Kept by a Studio that asked something else of a CLI: probed again, under these.
        studio_settings::writes_done();
        let upgraded = std::fs::read_to_string(probes_path()).unwrap();
        let earlier = upgraded.replace(&requirements(), "HTTP contract 0.14, target protocol 1");
        assert_ne!(earlier, upgraded, "the requirements are in the file");
        std::fs::write(probes_path(), &earlier).unwrap();
        forget_probes();
        assert!(found(&cli.programs()).is_ok());
        assert_eq!(cli.calls(), ["version --json"], "judged again");
        studio_settings::writes_done();
        assert!(
            std::fs::read_to_string(probes_path())
                .unwrap()
                .contains(&requirements())
        );
    }

    /// The sequences waiting for the CLI take it in the order they asked, but one a window
    /// waits on (`hurry` raised) goes before them, even raised after it started waiting.
    #[test]
    fn the_token_a_window_waits_on_goes_first() {
        let cache = probe_cache();
        let cli = StandIn::new(&cache, "hurry");
        for name in ["early", "late", "shown"] {
            cli.server(name, &format!("https://{name}.example.com/"));
            cli.write(&format!("token-{name}"), &format!("secret-{name}"));
        }
        // Probed beforehand, so each ask is its own three commands.
        assert!(found(&cli.programs()).is_ok());
        cli.calls();
        let held = smol::block_on(TURN.take(&Arc::default(), READ_TIMEOUT)).unwrap();
        let shown = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for (name, hurry) in [
            ("early", Arc::<AtomicBool>::default()),
            ("late", Arc::default()),
            ("shown", shown.clone()),
        ] {
            let source = cli.source(
                name,
                &format!("https://{name}.example.com/"),
                CliCredential::Store,
            );
            let asking = hurry.clone();
            threads.push(std::thread::spawn(move || {
                futures::executor::block_on(source.token(hurry)).map(Token::into_string)
            }));
            // Queued before the next asks, so the order they asked in is known.
            while !TURN.waits_for(&asking) {
                std::thread::yield_now();
            }
        }
        shown.store(true, Ordering::SeqCst);
        drop(held);
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let exported: Vec<String> = cli
            .commands()
            .into_iter()
            .filter(|call| call.starts_with("credentials export"))
            .collect();
        assert_eq!(
            exported,
            [
                "credentials export --target shown",
                "credentials export --target early",
                "credentials export --target late",
            ]
        );
    }

    /// A token's ask waits for its turn at the CLI as long as its export may take: one another
    /// of Studio's holds back past it is not run, and says so.
    #[test]
    fn a_command_held_back_past_its_timeout_is_not_run() {
        let cache = probing();
        let cli = StandIn::new(&cache, "busy");
        cli.server("stored", "https://store.example.com/");
        let held = smol::block_on(TURN.take(&Arc::default(), READ_TIMEOUT)).unwrap();
        let mut source = cli.source("stored", "https://store.example.com/", CliCredential::Store);
        source.timeout = Duration::from_millis(200);
        let waiting = std::thread::spawn(move || token_of(&source));
        let answer = waiting.join().unwrap();
        drop(held);
        let said = answer.unwrap_err();
        assert!(said.passing, "a busy CLI may answer later");
        assert!(
            said.why.contains("busy with another of Studio's commands"),
            "{said:?}"
        );
        assert!(cli.calls().is_empty(), "nothing was run");
    }

    /// The CLI runs with none of Studio's Omnigraph variables but `OMNIGRAPH_HOME` and the one
    /// its target's source names: checked in a process of its own, started with such variables
    /// set, so no other test's environment changes.
    #[test]
    fn the_export_runs_without_studios_omnigraph_variables() {
        let checked = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "cli_tokens::tests::export_environment_in_its_own_process",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("OMNIGRAPH_STUDIO_TEST_SECRET", "studios-own-secret")
            .env("OMNIGRAPH_STUDIO_TEST_KEPT", "kept-for-its-target")
            .env("OMNIGRAPH_HOME", std::env::temp_dir())
            .output()
            .unwrap();
        let printed = String::from_utf8_lossy(&checked.stdout);
        assert!(checked.status.success(), "{printed}");
        assert!(printed.contains("environment checked"), "{printed}");
    }

    /// Run by [`the_export_runs_without_studios_omnigraph_variables`] in a process whose
    /// environment holds Omnigraph variables; run alone it checks nothing.
    #[test]
    #[ignore = "child: run by the_export_runs_without_studios_omnigraph_variables"]
    fn export_environment_in_its_own_process() {
        if std::env::var_os("OMNIGRAPH_STUDIO_TEST_SECRET").is_none() {
            return;
        }
        let cache = probing();
        let cli = StandIn::new(&cache, "environment");
        cli.server("stored", "https://store.example.com/");
        cli.write("token-stored", "secret-stored-token");
        let source = cli.source(
            "stored",
            "https://store.example.com/",
            CliCredential::Environment {
                variable: "OMNIGRAPH_STUDIO_TEST_KEPT".into(),
            },
        );
        token_of(&source).unwrap();
        let exported = std::fs::read_to_string(cli.dir.join("export-env")).unwrap();
        let mut names: Vec<&str> = exported.lines().collect();
        names.sort();
        assert_eq!(names, ["OMNIGRAPH_HOME", "OMNIGRAPH_STUDIO_TEST_KEPT"]);
        println!("environment checked");
    }

    /// A server is added through the CLI: `target add` takes the target's entry on its standard
    /// input, and `credentials set` the token, which reaches no command line; one with no token
    /// is anonymous. A refusal is the CLI's words; a token the CLI would not keep removes the
    /// target again.
    #[test]
    fn a_server_is_added_through_the_cli() {
        let cache = probing();
        let cli = StandIn::new(&cache, "add");
        let added = |name: &str| -> serde_json::Value {
            serde_json::from_str(
                &std::fs::read_to_string(cli.dir.join(format!("added-{name}.json"))).unwrap(),
            )
            .unwrap()
        };
        smol::block_on(add_server_target(
            &cli.programs(),
            "work",
            "https://graph.example.com",
            Some(Token::typed("secret-typed-token".into())),
        ))
        .unwrap();
        assert_eq!(
            added("work"),
            serde_json::json!({"kind": "server", "endpoint": "https://graph.example.com",
                "auth": {"provider": "token", "source": {"store": {}}}})
        );
        assert_eq!(
            std::fs::read_to_string(cli.dir.join("token-work")).unwrap(),
            "secret-typed-token"
        );
        assert_eq!(
            cli.commands(),
            [
                "target add work --json",
                "credentials set --target work --stdin --replace"
            ]
        );
        assert!(cli.files_holding("secret-typed-token").is_empty());

        smol::block_on(add_server_target(
            &cli.programs(),
            "open",
            "http://127.0.0.1:8787",
            None,
        ))
        .unwrap();
        assert_eq!(
            added("open")["auth"],
            serde_json::json!({"provider": "anonymous"})
        );
        assert!(
            cli.calls()
                .iter()
                .all(|call| !call.starts_with("credentials set"))
        );

        cli.write("refuse-set", "");
        let refused = smol::block_on(add_server_target(
            &cli.programs(),
            "plain",
            "https://plain.example.com",
            Some(Token::typed("secret-plain".into())),
        ))
        .unwrap_err();
        assert!(
            refused.starts_with("set requires provider: token with source: store")
                && refused.ends_with("the target was removed again"),
            "{refused}"
        );
        assert!(!cli.dir.join("added-plain.json").exists());
        assert!(!refused.contains("secret-plain"));

        cli.write("refuse-add", "");
        let refused = smol::block_on(add_server_target(
            &cli.programs(),
            "work",
            "https://graph.example.com",
            None,
        ))
        .unwrap_err();
        assert_eq!(
            refused,
            "target already exists (`omnigraph target add work --json`, exit 1)"
        );
    }

    /// The CLI's export prints the token and a newline; anything else is refused.
    #[test]
    fn only_one_token_and_a_newline_is_a_token() {
        assert_eq!(
            exported_token(b"abc.DEF-123\n".to_vec()).map(Token::into_string),
            Some("abc.DEF-123".to_owned())
        );
        for refused in [&b"abc"[..], b"\n", b"a b\n", b"abc\n\n", b"\xff\n"] {
            assert!(exported_token(refused.to_vec()).is_none(), "{refused:?}");
        }
        let long: Vec<u8> = std::iter::repeat_n(b'a', MAX_TOKEN_BYTES + 1)
            .chain(*b"\n")
            .collect();
        assert!(exported_token(long).is_none());
    }

    /// What the CLI says on failure is one line: an error report's causes, a problem
    /// document's title and detail, or a refusal's line, without colours or its target note.
    #[test]
    fn what_the_cli_said_is_one_line() {
        assert_eq!(
            said(
                "target cloud (explicit): managed …\nError: \n   0: \u{1b}[91m{\"type\":\"login_required\",\"title\":\"login required\",\"detail\":\"sign in with login --profile NAME before continuing\"}\u{1b}[0m\n"
                    .as_bytes()
            ),
            "login required: sign in with login --profile NAME before continuing"
        );
        assert_eq!(
            said(b"Error: \n   0: first\n   1: second\n"),
            "first; second"
        );
        assert_eq!(
            said(b"target_unknown: unknown target 'x'; register it\n"),
            "target_unknown: unknown target 'x'; register it"
        );
        assert_eq!(said(b""), "the Omnigraph CLI failed and said nothing");
    }

    /// The CLI keeps no Omnigraph variable from Studio's environment but `OMNIGRAPH_HOME` and
    /// the one its target's token source names: Studio's own tokens and addressing stay out.
    #[test]
    fn the_cli_keeps_only_the_operator_home_and_its_own_variable() {
        let names = [
            "PATH",
            "OMNIGRAPH_HOME",
            "OMNIGRAPH_BEARER_TOKEN",
            "OMNIGRAPH_MR_BEARER_TOKEN",
            "OMNIGRAPH_STUDIO_TOKEN",
            "OMNIGRAPH_PERSONAL_TOKEN",
            "omnigraph_profile",
        ]
        .map(std::ffi::OsString::from);
        let command = scrubbed(
            Path::new("/bin/omnigraph"),
            &["OMNIGRAPH_PERSONAL_TOKEN".to_owned()],
            names,
        );
        let mut removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        removed.sort();
        assert_eq!(
            removed,
            [
                "OMNIGRAPH_BEARER_TOKEN",
                "OMNIGRAPH_MR_BEARER_TOKEN",
                "OMNIGRAPH_STUDIO_TOKEN",
                "omnigraph_profile"
            ]
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "NO_COLOR" && value.is_some())
        );
    }

    /// A token source standing in for the CLI: the answers it gives in turn, the last one
    /// again and again, and how many times it was asked.
    struct Answers {
        answers: std::sync::Mutex<Vec<Result<&'static str, NoToken>>>,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl TokenSource for Answers {
        fn token(&self, _: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut answers = self.answers.lock().unwrap();
            let answer = if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            };
            async move { answer.map(Token::new) }.boxed()
        }
    }

    /// A server answering `/healthz`, and the graph list only to the token `good`: another
    /// token is refused with the challenge `error="invalid_token"`, none with a bare `Bearer`.
    /// It counts the graph lists asked.
    fn guarded() -> (
        Arc<dyn http_client::HttpClient>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = lists.clone();
        let http = FakeHttpClient::create(move |request| {
            let health = request.uri().path().ends_with("/healthz");
            let token = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if !health {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            async move {
                let response = Response::builder()
                    .header(graph_client::CONTRACT_HEADER, graph_client::CONTRACT);
                let (response, body) = if health {
                    (
                        response.status(200),
                        r#"{"status":"ok","version":"0.14.0","internal_schema_version":16}"#,
                    )
                } else if token.as_deref() == Some("Bearer good") {
                    (response.status(200), r#"{"graphs":[]}"#)
                } else {
                    let challenge = if token.is_some() {
                        r#"Bearer error="invalid_token""#
                    } else {
                        "Bearer"
                    };
                    (
                        response.status(401).header("www-authenticate", challenge),
                        r#"{"error":"unauthorized","code":"unauthorized"}"#,
                    )
                };
                Ok(response.body(AsyncBody::from(body)).unwrap())
            }
        });
        (http, lists)
    }

    /// A CLI server whose token comes from `answers`.
    fn connection_with(
        answers: Vec<Result<&'static str, NoToken>>,
        cx: &mut TestAppContext,
    ) -> (
        gpui::Entity<Connection>,
        Arc<Answers>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let (http, lists) = guarded();
        let source = Arc::new(Answers {
            answers: std::sync::Mutex::new(answers),
            asked: Default::default(),
        });
        let server = ServerEntry {
            name: "stored".into(),
            url: "https://store.example.com".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let given: Arc<dyn TokenSource> = source.clone();
        let connection = cx.new(|_| {
            Connection::new(server, None, http)
                .unwrap()
                .with_token_source(Some(given))
        });
        (connection, source, lists)
    }

    fn asked(source: &Answers) -> usize {
        source.asked.swap(0, std::sync::atomic::Ordering::SeqCst)
    }

    /// A token the server refuses is exported again once: a rotated one connects, the same one
    /// stays refused and is asked for no more, through health checks too, until Refresh. The
    /// challenge says the token was refused.
    #[gpui::test]
    async fn a_refused_token_is_exported_again_once(cx: &mut TestAppContext) {
        let (rotated, source, _) = connection_with(vec![Ok("stale"), Ok("good")], cx);
        rotated.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 2);
        rotated.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready)
        });

        let (stale, source, lists) = connection_with(vec![Ok("stale")], cx);
        let _shown = stale.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        assert_eq!(asked(&source), 2, "once at connect, once after the 401");
        let listed = lists.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..3 {
            cx.executor().advance_clock(Duration::from_secs(120));
            cx.run_until_parked();
        }
        assert_eq!(asked(&source), 0);
        assert_eq!(lists.load(std::sync::atomic::Ordering::SeqCst), listed);
        stale.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized);
            assert_eq!(
                connection.refusal,
                Some(graph_client::Unauthorized::TokenRefused)
            );
            assert_eq!(connection.label(), "Token refused");
        });
        stale.update(cx, |connection, cx| connection.refresh_token(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1, "Refresh asks again");
    }

    /// Opening a graph asks the CLI for the token once, at the connect: its first reads do not
    /// ask again, nor does a reload, and a token typed in stands through them; only the
    /// person's Refresh asks again.
    #[gpui::test]
    async fn opening_a_graph_asks_the_cli_once(cx: &mut TestAppContext) {
        let (connection, source, _) = connection_with(vec![Ok("good")], cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        let session = cx.update(|cx| {
            crate::open_session(connection.clone(), "knowledge".into(), "main".into(), cx)
        });
        cx.run_until_parked();
        assert_eq!(asked(&source), 1, "the connect's ask alone");
        session.update(cx, |session, cx| session.reload(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 0, "a reload asks nothing");

        connection.update(cx, |connection, cx| {
            connection.set_token(Some("typed".into()), cx)
        });
        session.update(cx, |session, cx| session.reload(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 0, "the token typed stands");
        session.update(cx, |session, cx| session.refresh(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1, "Refresh asks again");
    }

    /// A source that gives no token sends the server nothing: the connection says why, and a
    /// token typed in then connects and stands until Refresh, which asks the source again.
    #[gpui::test]
    async fn no_credential_sends_nothing(cx: &mut TestAppContext) {
        let reason = "No credential is configured for 'stored': its token file /t is not there";
        let (connection, source, lists) = connection_with(vec![Err(NoToken::lasting(reason))], cx);
        let _shown = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        assert_eq!(asked(&source), 1);
        assert_eq!(lists.load(std::sync::atomic::Ordering::SeqCst), 0);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized);
            assert_eq!(connection.refusal, None, "no 401 stands: nothing was sent");
            assert!(!connection.token_unanswered);
            assert_eq!(connection.token_unread.as_deref(), Some(reason));
            assert_eq!(connection.label(), "No token from the CLI");
        });
        // The CLI answered: health checks do not ask it again.
        cx.executor().advance_clock(Duration::from_secs(240));
        cx.run_until_parked();
        assert_eq!(asked(&source), 0);
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("good".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(asked(&source), 0);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready);
            assert_eq!(connection.token_unread, None);
        });
        connection.update(cx, |connection, cx| connection.refresh_token(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized)
        });
    }

    /// A CLI that cannot answer now (busy, slow, not started) leaves the token it gave in use:
    /// Refresh and a connect go on with it, and nothing is said. Before, the working token was
    /// dropped and the server shown refused until the person acted.
    #[gpui::test]
    async fn a_cli_that_cannot_answer_leaves_its_token_in_use(cx: &mut TestAppContext) {
        let busy = NoToken::passing("the Omnigraph CLI was busy");
        let (connection, source, lists) = connection_with(vec![Ok("good"), Err(busy)], cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1);
        let changes = connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready);
            connection.client().credential_changes()
        });

        connection.update(cx, |connection, cx| connection.refresh_token(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1, "Refresh asks");
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready);
            assert_eq!(connection.token_unread, None);
            assert_eq!(connection.client().credential_changes(), changes);
        });

        // A connect sends the token held: the server lists the graphs only for it.
        let listed = lists.load(std::sync::atomic::Ordering::SeqCst);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1);
        assert_eq!(lists.load(std::sync::atomic::Ordering::SeqCst), listed + 1);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready)
        });
    }

    /// A CLI that cannot answer the ask after the server refused its token leaves no token:
    /// the refused one is not kept. The server says why, and the next health check asks again,
    /// which a CLI that answered it has none is not.
    #[gpui::test]
    async fn a_cli_that_cannot_answer_after_a_refusal_is_asked_again(cx: &mut TestAppContext) {
        let busy = "the Omnigraph CLI was busy";
        let (connection, source, _) = connection_with(
            vec![Ok("stale"), Err(NoToken::passing(busy)), Ok("good")],
            cx,
        );
        let _shown = connection.update(cx, |connection, cx| {
            connection.connect(cx);
            connection.watch(cx)
        });
        cx.run_until_parked();
        assert_eq!(asked(&source), 2, "once at connect, once after the 401");
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized);
            assert!(connection.token_unanswered);
            assert_eq!(connection.token_unread.as_deref(), Some(busy));
            assert_eq!(connection.label(), "No token from the CLI");
        });
        cx.executor().advance_clock(Duration::from_secs(120));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready);
            assert!(!connection.token_unanswered);
            assert_eq!(connection.token_unread, None);
        });
    }

    /// A 401 to a request with no token is a server asking for one, as its bare challenge says.
    #[gpui::test]
    async fn a_bare_challenge_asks_for_a_token(cx: &mut TestAppContext) {
        let (http, _) = guarded();
        let server = ServerEntry {
            name: "open".into(),
            url: "https://store.example.com".into(),
            source: ServerSource::Cli,
            known_graphs: Vec::new(),
        };
        let connection = cx.new(|_| Connection::new(server, None, http).unwrap());
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized);
            assert_eq!(
                connection.refusal,
                Some(graph_client::Unauthorized::TokenNeeded)
            );
            assert_eq!(connection.label(), "Asks for a token");
        });
    }
}

/// The one rule for Omnigraph's variables, which Studio's CLI, the agent, the agent's
/// `omnigraph_run` and its wrapper follow alike: on every system, as Windows reads names.
#[cfg(test)]
mod variable_tests {
    use super::*;

    /// The prefix alone decides, whole and at the start of the name, in any ASCII case.
    #[test]
    fn every_name_with_omnigraphs_prefix_is_omnigraphs() {
        for name in [
            "OMNIGRAPH_TOKEN_PERSONAL",
            "OMNIGRAPH_BEARER_TOKEN",
            "OMNIGRAPH_CONTROL_TOKEN",
            "OMNIGRAPH_A_VARIABLE_ADDED_LATER",
            "OMNIGRAPH_",
            "omnigraph_profile",
            "Omnigraph_Token_Work",
        ] {
            assert!(is_omnigraph_variable(name), "{name}");
        }
        for name in [
            "OMNIGRAPH",
            "OMNIGRAPHX",
            "XOMNIGRAPH_TOKEN",
            "MY_OMNIGRAPH_TOKEN",
            "OMNIGRAPHé_",
            "",
            "PATH",
        ] {
            assert!(!is_omnigraph_variable(name), "{name}");
        }
    }

    /// Every Omnigraph variable is removed but those kept, which are matched in any ASCII case
    /// too; each once, sorted.
    #[test]
    fn what_is_removed_is_every_omnigraph_variable_but_those_kept() {
        let names = [
            "PATH",
            "OMNIGRAPH_TOKEN_PERSONAL",
            "OMNIGRAPH_HOME",
            "AWS_PROFILE",
            "OMNIGRAPH_PROFILE",
            "OMNIGRAPH_PROFILE",
        ]
        .map(std::ffi::OsString::from);
        assert_eq!(
            omnigraph_variables(names.clone(), &["OMNIGRAPH_HOME"]),
            ["OMNIGRAPH_PROFILE", "OMNIGRAPH_TOKEN_PERSONAL"]
        );
        assert_eq!(
            omnigraph_variables(names, &[]),
            [
                "OMNIGRAPH_HOME",
                "OMNIGRAPH_PROFILE",
                "OMNIGRAPH_TOKEN_PERSONAL"
            ]
        );
        let names = ["omnigraph_home", "Omnigraph_Token_Work"].map(std::ffi::OsString::from);
        assert_eq!(
            omnigraph_variables(names, &["OMNIGRAPH_HOME"]),
            ["Omnigraph_Token_Work"]
        );
    }
}

/// What holds on every system, Windows included, where the stand-in CLI (a shell script) cannot
/// run.
#[cfg(test)]
mod every_system_tests {
    use super::*;

    /// The CLI is looked for on every system: with no program where Studio looks, Studio says
    /// so, never that the system has no Unix pipe. Before, every system but Unix was refused
    /// before a program was looked at, so a Windows Studio listed no CLI target at all.
    #[test]
    fn the_cli_is_looked_for_on_every_system() {
        let why = smol::block_on(async {
            let turn = take_turn(&Arc::default(), READ_TIMEOUT, "a look for the CLI").await?;
            find_cli(&[], &turn).await
        })
        .unwrap_err();
        assert!(why.contains("no `omnigraph` is on PATH"), "{why}");
        assert!(!why.contains("Unix pipe"), "{why}");
    }
}

/// Against the person's own CLI and operator home, which CI has neither.
#[cfg(all(test, unix))]
mod live_tests {
    use super::*;
    use crate::cli_config::{CliAuth, Platform, operator_dir, read_targets};

    /// Every server target that takes a token is given one when all are asked at once, each
    /// from a thread of its own, as the Open dialog asks them. Only the tokens' lengths are
    /// looked at; nothing is printed.
    #[test]
    #[ignore = "needs: an Omnigraph CLI from main ff542202 on where Studio looks for one, and an \
                operator home with server targets whose credentials are configured"]
    fn every_server_target_is_given_its_token_when_asked_together() {
        let dir = operator_dir();
        let listed = smol::block_on(read_targets(
            dir.as_deref(),
            &candidates(),
            Platform::CURRENT,
        ));
        let servers = listed.servers.expect("the CLI lists its targets");
        let threads: Vec<_> = servers
            .iter()
            .filter_map(|server| match &server.auth {
                CliAuth::Token(credential) => {
                    let source = CliTokenSource::for_server(server, credential);
                    let name = server.name.clone();
                    Some(std::thread::spawn(move || {
                        let given = futures::executor::block_on(source.token(Arc::default()))
                            .map(|token| token.into_string().len());
                        (name, given)
                    }))
                }
                CliAuth::Anonymous => None,
            })
            .collect();
        assert!(!threads.is_empty(), "no server target takes a token");
        for thread in threads {
            let (name, given) = thread.join().unwrap();
            assert!(
                given.as_ref().is_ok_and(|length| *length > 0),
                "{name}: {given:?}"
            );
        }
    }
}
