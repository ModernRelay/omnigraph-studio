//! The OmniGraph CLI, as Studio runs it for the CLI's targets and their tokens (spec 003).
//!
//! Studio parses no operator configuration and no credential source: what the person's CLI
//! targets are comes from `omnigraph target list --json` and `target inspect NAME --json`, which
//! read the configuration without a server or a secret; whether a target has a credential comes
//! from `credentials status --target NAME --json`, which reads no secret either; and a server
//! target's token comes from `credentials export --target NAME`, which writes the token and a
//! newline to a pipe and refuses a terminal or a file. So an environment variable, a token file,
//! the CLI's secure store and a credential helper are each resolved by OmniGraph's own code
//! (OmniGraph RFC 1017, "Static tokens for clients"). Studio keeps a token in memory only, never
//! logs or writes it, and sends it only to the endpoint of the target it was exported for.
//!
//! The CLI is chosen by what `version --json` reports ([`PROTOCOLS`]) and by the commands Studio
//! runs being there ([`COMMANDS`]). It runs off the main thread with a timeout, one run at a time
//! ([`TURN`]: the CLI refuses a second command on a home whose lock another holds), its standard
//! input empty, and none of OmniGraph's variables but `OMNIGRAPH_HOME` and the one a target's own
//! token source names ([`scrubbed`]). A token is asked for when its server connects, once more
//! after a 401, and on Refresh (`Connection`), not on a timer: the secure store cannot be
//! watched.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{AsyncReadExt as _, FutureExt as _};
use serde_json::Value;

use crate::cli_config::{CliCredential, CliServer, FileStamp};

/// The program's name.
const CLI_NAME: &str = if cfg!(windows) {
    "omnigraph.exe"
} else {
    "omnigraph"
};

/// What the name of every OmniGraph variable starts with.
const VARIABLE_PREFIX: &str = "OMNIGRAPH_";

/// The protocols `version --json` reports, and the numbers of each that Studio drives: the
/// target, credential and access protocols of OmniGraph main `7fff9dfd` (all 1), and the bump of
/// the first two that ModernRelay/omnigraph#1127 asks for, which marks the same commands. A CLI
/// that reports another number, or none, speaks commands Studio does not know.
pub(crate) const PROTOCOLS: &[(&str, &[u64])] = &[
    ("target", &[1, 2]),
    ("credential", &[1, 2]),
    ("access", &[1]),
];

/// The commands Studio runs, which must be there too: builds before OmniGraph main `f7db38da`
/// report the same protocol numbers without them (ModernRelay/omnigraph#1127).
const COMMANDS: &[&[&str]] = &[
    &["target", "list"],
    &["target", "inspect"],
    &["credentials", "status"],
    &["credentials", "export"],
];

/// The CLI's own bound on a token (`credential_sources::MAX_CREDENTIAL_BYTES`, 64 KiB).
const MAX_TOKEN_BYTES: usize = 64 * 1024;

/// How much of a JSON answer, or of what the CLI writes to standard error, is read.
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

/// How long a CLI may take to answer a command that reads only its configuration. A test's
/// stand-in is a shell script, which a loaded machine can take seconds to start.
pub(crate) const READ_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(60)
} else {
    Duration::from_secs(10)
};

/// How long an export may take when no credential helper runs.
pub(crate) const EXPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// How much longer than its helper's own timeout an export may take: the helper is the CLI's to
/// stop, and this is the CLI's start and its own work.
pub(crate) const HELPER_MARGIN: Duration = Duration::from_secs(15);

/// A token, kept in memory only: its `Debug` never shows it.
pub struct Token(String);

impl Token {
    pub(crate) fn into_string(self) -> String {
        self.0
    }

    #[cfg(test)]
    pub(crate) fn new(token: &str) -> Self {
        Self(token.to_owned())
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(…)")
    }
}

/// Where a connection's token comes from each time it is asked (`Connection`): on each connect,
/// once more after a 401, and on Refresh. A failure says why, in words the person reads.
pub trait TokenSource: Send + Sync + 'static {
    fn token(&self) -> BoxFuture<'static, Result<Token, String>>;
}

/// The CLI Studio runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoundCli {
    pub path: PathBuf,
    /// Its release, as `version --json` reports it.
    pub version: String,
}

/// Where Studio looks for the CLI, in order: each directory on `PATH`, then `~/.local/bin`,
/// `/opt/homebrew/bin` and `/usr/local/bin`, where its installers put it. Every place is listed
/// whether or not a program is there, so a stamp of them sees one installed later.
pub(crate) fn candidates() -> Vec<PathBuf> {
    let mut places: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !places.contains(&path) {
            places.push(path);
        }
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
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

/// What each program Studio looked at was found to be, with its file's stamp then: a program is
/// probed again only when its file changes, so a connect runs only the commands it needs.
type Probed = (PathBuf, FileStamp, Result<FoundCli, String>);
static PROBED: Mutex<Vec<Probed>> = Mutex::new(Vec::new());

/// The first of `programs` that is an OmniGraph CLI Studio can drive: one that reports
/// [`PROTOCOLS`] and has [`COMMANDS`]. Otherwise why none is, naming each program found.
pub(crate) async fn find_cli(programs: &[PathBuf]) -> Result<FoundCli, String> {
    if !cfg!(unix) {
        return Err(
            "the OmniGraph CLI gives a token only through a Unix pipe, which this system \
                    does not have"
                .to_owned(),
        );
    }
    let stamp_of = |path: &Path| crate::cli_config::stamp(&[path.to_owned()]).pop().flatten();
    let mut unfit: Vec<String> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for program in programs {
        if !is_executable(program) {
            continue;
        }
        let canonical = std::fs::canonicalize(program).unwrap_or_else(|_| program.clone());
        if seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical);
        let stamp = stamp_of(program);
        let held = stamp.and_then(|stamp| {
            PROBED.lock().ok()?.iter().find_map(|(path, at, probed)| {
                (path == program && *at == stamp).then(|| probed.clone())
            })
        });
        let probed = match held {
            Some(probed) => probed,
            None => {
                let probed = probe(program).await;
                if let (Some(stamp), Ok(mut held)) = (stamp, PROBED.lock()) {
                    held.retain(|(path, _, _)| path != program);
                    held.push((program.clone(), stamp, probed.clone()));
                }
                probed
            }
        };
        match probed {
            Ok(found) => return Ok(found),
            Err(why) => unfit.push(format!("{} {why}", shown(program))),
        }
    }
    let looked = if unfit.is_empty() {
        "no `omnigraph` is on PATH or in ~/.local/bin, /opt/homebrew/bin or /usr/local/bin"
            .to_owned()
    } else {
        unfit.join("; ")
    };
    Err(format!(
        "Studio found no OmniGraph CLI it can drive (an OmniGraph build from main ff542202 on, \
         reporting target, credential and access protocols it knows): {looked}"
    ))
}

/// `program` as the CLI Studio runs, or why it is not one.
async fn probe(program: &Path) -> Result<FoundCli, String> {
    let json = run(
        || command(program, &[], &["version", "--json"]),
        READ_TIMEOUT,
        MAX_ANSWER_BYTES,
    )
    .await
    .ok()
    .filter(|ran| ran.status.success())
    .and_then(|ran| serde_json::from_slice::<Value>(&ran.stdout).ok())
    .ok_or("answers no `version --json` (older than OmniGraph 0.14, or not its CLI)")?;
    let version = json
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("a build that names no version")
        .to_owned();
    for (protocol, known) in PROTOCOLS {
        let reported = json.pointer(&format!("/protocols/{protocol}"));
        match reported.and_then(Value::as_u64) {
            Some(number) if known.contains(&number) => {}
            Some(number) => {
                return Err(format!(
                    "is {version}, whose {protocol} protocol {number} Studio does not know"
                ));
            }
            None => {
                return Err(format!(
                    "is {version}, which reports no {protocol} protocol"
                ));
            }
        }
    }
    for words in COMMANDS {
        let mut args: Vec<&str> = words.to_vec();
        args.push("--help");
        let there = run(
            || command(program, &[], &args),
            READ_TIMEOUT,
            MAX_ANSWER_BYTES,
        )
        .await
        .is_ok_and(|ran| ran.status.success());
        if !there {
            return Err(format!("is {version}, which has no `{}`", words.join(" ")));
        }
    }
    Ok(FoundCli {
        path: program.to_owned(),
        version,
    })
}

/// The targets `target list --json` names: each name with its entry as the CLI describes it.
pub(crate) async fn list_targets(cli: &FoundCli) -> Result<serde_json::Map<String, Value>, String> {
    match json_answer(cli, &[], &["target", "list", "--json"]).await? {
        Value::Object(targets) => Ok(targets),
        _ => Err("`omnigraph target list --json` did not answer an object of targets".to_owned()),
    }
}

/// What `target inspect NAME --json` says of one target (`resolved`: with its managed profile
/// expanded, `--resolved`), without a server or a secret.
pub(crate) async fn inspect_target(
    cli: &FoundCli,
    name: &str,
    resolved: bool,
) -> Result<Value, String> {
    let mut args = vec!["target", "inspect", name, "--json"];
    if resolved {
        args.push("--resolved");
    }
    json_answer(cli, &[], &args).await
}

/// What `cli` prints as JSON for `args`, or what it says when it refuses: a refusal asked for as
/// JSON comes on standard output as `{"error": …, "code": …}`, any other failure on standard
/// error.
async fn json_answer(cli: &FoundCli, kept: &[String], args: &[&str]) -> Result<Value, String> {
    let asked = format!("`omnigraph {}`", args.join(" "));
    let ran = match run(
        || command(&cli.path, kept, args),
        READ_TIMEOUT,
        MAX_ANSWER_BYTES,
    )
    .await
    {
        Ok(ran) => ran,
        Err(Unrun::Timeout) => {
            return Err(format!(
                "the OmniGraph CLI did not answer {asked} within {} s, and was stopped",
                READ_TIMEOUT.as_secs()
            ));
        }
        Err(Unrun::Failed(error)) => {
            return Err(format!(
                "Studio could not run the OmniGraph CLI at {} ({asked}): {error}",
                shown(&cli.path)
            ));
        }
    };
    let json = serde_json::from_slice::<Value>(&ran.stdout).ok();
    if ran.status.success() {
        return json.ok_or_else(|| format!("the OmniGraph CLI's answer to {asked} is not JSON"));
    }
    let refused = json
        .as_ref()
        .and_then(|json| json.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| said(&ran.stderr));
    Err(format!("{refused} ({asked}, {})", exit(&ran.status)))
}

/// A CLI server's token: `credentials status`, then `credentials export`, then `target inspect`
/// for its target, run with the CLI Studio finds.
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
    /// Where to look for the CLI; `None` for the usual places ([`candidates`]).
    pub programs: Option<Vec<PathBuf>>,
}

impl CliTokenSource {
    pub(crate) fn for_server(server: &CliServer, credential: &CliCredential) -> Self {
        let timeout = match credential {
            CliCredential::Helper { timeout } => EXPORT_TIMEOUT.max(*timeout + HELPER_MARGIN),
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

    /// The OmniGraph variables the CLI keeps for this target: the one its token source names.
    fn kept(&self) -> Vec<String> {
        match &self.credential {
            CliCredential::Environment { variable } => vec![variable.clone()],
            _ => Vec::new(),
        }
    }

    async fn resolve(&self) -> Result<Token, String> {
        let programs = self.programs.clone().unwrap_or_else(candidates);
        let cli = find_cli(&programs).await?;
        let target = self.target.as_str();
        let kept = self.kept();

        // Whether a credential is there at all, read without the secret.
        let status = json_answer(
            &cli,
            &kept,
            &["credentials", "status", "--target", target, "--json"],
        )
        .await?;
        let availability = status.get("availability").and_then(Value::as_str);
        let route = status.pointer("/owner/route").and_then(Value::as_str);
        match availability {
            Some("absent" | "unavailable") => return Err(self.no_credential()),
            Some("anonymous") => return Err(self.changed()),
            _ if route != Some(self.endpoint.as_str()) => return Err(self.changed()),
            _ => {}
        }

        let token = self.export(&cli, &kept).await?;

        // The export read the configuration again: a token for another endpoint is dropped.
        let inspected = inspect_target(&cli, target, false).await?;
        let still = inspected.pointer("/target/kind").and_then(Value::as_str) == Some("server")
            && inspected
                .pointer("/target/endpoint")
                .and_then(Value::as_str)
                == Some(self.endpoint.as_str());
        if !still {
            return Err(self.changed());
        }
        Ok(token)
    }

    /// Runs the export with `cli` and takes the token it prints.
    async fn export(&self, cli: &FoundCli, kept: &[String]) -> Result<Token, String> {
        let args = ["credentials", "export", "--target", self.target.as_str()];
        let asked = format!("`omnigraph credentials export --target {}`", self.target);
        let ran = match run(
            || command(&cli.path, kept, &args),
            self.timeout,
            MAX_TOKEN_BYTES + 1,
        )
        .await
        {
            Ok(ran) => ran,
            Err(Unrun::Timeout) => {
                return Err(format!(
                    "the OmniGraph CLI gave no token within {} s ({asked}), and was stopped",
                    self.timeout.as_secs()
                ));
            }
            Err(Unrun::Failed(error)) => {
                return Err(format!(
                    "Studio could not run the OmniGraph CLI at {} ({asked}): {error}",
                    shown(&cli.path)
                ));
            }
        };
        if !ran.status.success() {
            // What it printed is never shown: it may be part of a token.
            return Err(format!(
                "{} ({asked}, {})",
                said(&ran.stderr),
                exit(&ran.status)
            ));
        }
        exported_token(ran.stdout).ok_or_else(|| {
            format!("the OmniGraph CLI's answer to {asked} was not one token and a newline")
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
    fn token(&self) -> BoxFuture<'static, Result<Token, String>> {
        let source = self.clone();
        async move { source.resolve().await }.boxed()
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
    valid.then(|| Token(token.to_owned()))
}

/// `program` with `args`, as Studio starts it ([`scrubbed`]).
fn command(program: &Path, kept: &[String], args: &[&str]) -> std::process::Command {
    let mut command = scrubbed(program, kept, std::env::vars_os().map(|(name, _)| name));
    command.args(args);
    command
}

/// `program` as Studio starts it: with no OmniGraph variable from Studio's environment (whose
/// names are `names`) but `OMNIGRAPH_HOME`, which names the operator home, and those `kept`,
/// which a target's token source reads; and with no colours in what it says.
fn scrubbed(
    program: &Path,
    kept: &[String],
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    for name in names {
        let Some(text) = name.to_str() else {
            continue;
        };
        let omnigraph = text
            .get(..VARIABLE_PREFIX.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(VARIABLE_PREFIX));
        if omnigraph && text != "OMNIGRAPH_HOME" && !kept.iter().any(|kept| kept == text) {
            command.env_remove(&name);
        }
    }
    command.env("NO_COLOR", "1");
    command
}

/// What a finished run printed.
struct Ran {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Why a run gave no output.
enum Unrun {
    Timeout,
    Failed(String),
}

/// Studio's turn at the CLI: one of its runs at a time. The CLI takes its operator home's locks
/// without waiting (`ConfigEditLock` takes `try_lock`), so two runs at once on one home refuse
/// each other ("could not lock operator configuration"), as every server's token asked together
/// did when the Open dialog connected them all.
static TURN: smol::lock::Mutex<()> = smol::lock::Mutex::new(());

/// What the CLI says when another program holds its operator home's lock
/// (`targets::ConfigEditLock::acquire`).
const LOCK_REFUSED: &str = "could not lock operator configuration";

/// How many times a run the CLI refused for another program's lock is made again, and how long
/// apart: the person's own CLI holds the lock for one command.
const LOCK_RETRIES: u32 = 4;
const LOCK_PAUSE: Duration = Duration::from_millis(250);

/// Runs the command `make` builds, in Studio's turn ([`TURN`]), with its standard input empty and
/// its output read from pipes, at most `limit` bytes of standard output and
/// [`MAX_ANSWER_BYTES`] of standard error. Past `timeout` the process is stopped. A run refused
/// for another program's lock is made again, [`LOCK_RETRIES`] times at most.
async fn run(
    make: impl Fn() -> std::process::Command,
    timeout: Duration,
    limit: usize,
) -> Result<Ran, Unrun> {
    let _turn = TURN.lock().await;
    let mut retries = 0;
    loop {
        let ran = run_once(make(), timeout, limit).await?;
        let locked =
            !ran.status.success() && String::from_utf8_lossy(&ran.stderr).contains(LOCK_REFUSED);
        if !locked || retries == LOCK_RETRIES {
            return Ok(ran);
        }
        retries += 1;
        smol::Timer::after(LOCK_PAUSE).await;
    }
}

/// [`run`], once.
async fn run_once(
    command: std::process::Command,
    timeout: Duration,
    limit: usize,
) -> Result<Ran, Unrun> {
    let mut child = smol::process::Command::from(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| Unrun::Failed(error.to_string()))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(Unrun::Failed("its output could not be read".to_owned()));
    };
    // Each pipe is closed once read to its bound, so a program that writes more stops.
    let read = |pipe: Box<dyn futures::AsyncRead + Send + Unpin>, bound: usize| async move {
        let mut read = Vec::new();
        pipe.take(bound as u64).read_to_end(&mut read).await?;
        Ok::<_, std::io::Error>(read)
    };
    let work = async move {
        let (out, err) = futures::join!(
            read(Box::new(stdout), limit),
            read(Box::new(stderr), MAX_ANSWER_BYTES),
        );
        let (out, err) = (
            out.map_err(|error| error.to_string())?,
            err.map_err(|error| error.to_string())?,
        );
        let status = child.status().await.map_err(|error| error.to_string())?;
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
        return "the OmniGraph CLI failed and said nothing".to_owned();
    }
    let mut line = causes.join("; ");
    const MAX: usize = 400;
    if line.len() > MAX {
        let mut end = MAX;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
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
    use std::sync::Arc;

    use gpui::{AppContext as _, TestAppContext};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    use super::*;
    use crate::cli_config::{CliAuth, read_targets};
    use crate::connection::{Connection, ConnectionState, ServerEntry, ServerSource};

    /// What `version --json` reports at OmniGraph main `7fff9dfd` (`0.14.0-edge+7fff9df`).
    const VERSION: &str = r#"{"version":"0.14.0-edge+7fff9df","channel":"edge","commit":"7fff9dfd91835201986d72f3e3e014b36c3e636a","http_contract":"0.14","protocols":{"target":1,"credential":1,"access":1},"internal_schema":{"writes":16,"serves":[14,16]}}"#;

    /// A stand-in for the OmniGraph CLI: a shell script that answers the commands Studio runs
    /// from files in its directory, and records each call and the OmniGraph variables of each
    /// export. Its export refuses a standard output that is not a pipe, as the CLI's does.
    struct StandIn {
        dir: PathBuf,
        program: PathBuf,
    }

    impl StandIn {
        fn new(name: &str) -> Self {
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
  mkdir "$dir/lock" 2>/dev/null || {{ echo "Error: could not lock operator configuration: lock acquisition failed because the operation would block" >&2; exit 1; }}
  sleep 0.2
  rmdir "$dir/lock"
}}
case "$*" in
  "version --json") cat "$dir/version.json"; exit 0 ;;
  *" --help")
    [ -f "$dir/no-$1-$2" ] && {{ echo "error: unrecognized subcommand '$1'" >&2; exit 2; }}
    echo "Usage: omnigraph $1 $2"; exit 0 ;;
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
  "credentials export --target "*)
    lock
    env | sed 's/=.*//' | grep '^OMNIGRAPH_' > "$dir/export-env"
    [ -p /dev/stdout ] || {{ echo "credentials export requires stdout to be a pipe; terminals and regular files are refused" >&2; exit 1; }}
    if [ -f "$dir/fail-$4" ]; then cat "$dir/fail-$4"; printf 'Error: \n   0: \033[91m{{"type":"credential_missing","title":"no credential","detail":"no token is stored for %s"}}\033[0m\n' "$4" >&2; exit 1; fi
    [ -f "$dir/move-$4" ] && cp "$dir/move-$4" "$dir/inspect-$4.json"
    [ -f "$dir/slow-$4" ] && sleep 5
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

        fn status(&self, name: &str, endpoint: &str, availability: &str) {
            self.write(
                &format!("status-{name}.json"),
                &format!(
                    r#"{{"owner":{{"kind":"server","owner_id":"o","profile_id":"p","route":"{endpoint}","organization_id":null}},"profile":"{name}","availability":"{availability}","identity":"unknown","server_validity":"unverified","observed_at":"2026-10-22T06:07:34Z","verified_at":null}}"#
                ),
            );
        }

        fn source(&self, name: &str, endpoint: &str, credential: CliCredential) -> CliTokenSource {
            CliTokenSource {
                target: name.into(),
                endpoint: endpoint.into(),
                credential,
                timeout: Duration::from_secs(60),
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

    /// A CLI is chosen only when it reports the target, credential and access protocols Studio
    /// drives (those of OmniGraph main `7fff9dfd`, and the bump #1127 asks for) and has every
    /// command Studio runs; each one passed over is named with why.
    #[test]
    fn a_cli_is_chosen_by_its_protocols_and_commands() {
        let old = StandIn::new("old");
        old.write("version.json", "not json");
        let pre_access = StandIn::new("pre-access");
        pre_access.write(
            "version.json",
            r#"{"version":"0.14.0-edge+50283ce","http_contract":"0.14","protocols":{"target":1,"credential":1}}"#,
        );
        let unknown = StandIn::new("unknown");
        unknown.write(
            "version.json",
            r#"{"version":"0.15.0","http_contract":"0.14","protocols":{"target":3,"credential":2,"access":1}}"#,
        );
        let no_status = StandIn::new("no-status");
        no_status.write("no-credentials-status", "");
        let current = StandIn::new("current");
        let bumped = StandIn::new("bumped");
        bumped.write(
            "version.json",
            r#"{"version":"0.14.1","http_contract":"0.14","protocols":{"target":2,"credential":2,"access":1}}"#,
        );
        let missing = old.dir.join("not-there");
        let not_executable = old.dir.join("plain");
        std::fs::write(&not_executable, "#!/bin/sh\n").unwrap();

        let unfit = [&old, &pre_access, &unknown, &no_status];
        let mut programs: Vec<PathBuf> = vec![missing, not_executable];
        programs.extend(unfit.iter().map(|stand_in| stand_in.program.clone()));
        let why = smol::block_on(find_cli(&programs)).unwrap_err();
        for said in [
            "answers no `version --json`",
            "is 0.14.0-edge+50283ce, which reports no access protocol",
            "is 0.15.0, whose target protocol 3 Studio does not know",
            "is 0.14.0-edge+7fff9df, which has no `credentials status`",
        ] {
            assert!(why.contains(said), "{said}: {why}");
        }
        assert!(
            !why.contains("not-there") && !why.contains("plain"),
            "{why}"
        );

        programs.push(current.program.clone());
        programs.push(bumped.program.clone());
        let found = smol::block_on(find_cli(&programs)).unwrap();
        assert_eq!(found.path, current.program);
        assert_eq!(found.version, "0.14.0-edge+7fff9df");
        let found = smol::block_on(find_cli(&bumped.programs())).unwrap();
        assert_eq!(found.version, "0.14.1");
        // Found again with no probe while the program is unchanged.
        bumped.calls();
        smol::block_on(find_cli(&bumped.programs())).unwrap();
        assert!(bumped.calls().is_empty());

        let none = smol::block_on(find_cli(&[])).unwrap_err();
        assert!(none.contains("no `omnigraph` is on PATH"), "{none}");
    }

    /// The targets are what `target list --json` says, in the shape the CLI at OmniGraph main
    /// `7fff9dfd` prints: server targets land on servers (an anonymous one with no token, the
    /// others with where their credential is), direct and managed targets are refused, each
    /// with why, a managed target is kept as `target inspect --resolved` describes it, a custom
    /// CA is not supported yet, and a second target on an address is named as an alternative. A
    /// configuration the CLI refuses lists nothing, with the CLI's own words.
    #[test]
    fn the_targets_are_listed_as_the_cli_describes_them() {
        let cli = StandIn::new("targets");
        let token_file = cli.dir.join("tokens").join("filed");
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
  "strategy": {{"kind": "server", "endpoint": "https://127.0.0.1:8443/", "auth": {{"provider": "token", "source": {{"store": {{}}}}}}, "transport": {{"tls": {{"ca": {{"mode": "append", "file": "/tmp/ca.pem"}}}}, "proxy": {{"mode": "direct"}}}}, "default_graph": "strategy"}},
  "stored": {{"kind": "server", "endpoint": "https://store.example.com/", "auth": {{"provider": "token", "source": {{"store": {{}}}}}}}},
  "stored-too": {{"kind": "server", "endpoint": "https://store.example.com", "auth": {{"provider": "anonymous"}}}}
}}"#,
                file = token_file.display()
            ),
        );
        for (name, cluster, endpoint) in [
            (
                "cloud",
                "c1",
                r#","endpoint":"https://cluster.example.com/","default_graph":"crm""#,
            ),
            ("control", "c2", ""),
        ] {
            cli.write(
                &format!("inspect-{name}-resolved.json"),
                &format!(
                    r#"{{"name":"{name}","target":{{"kind":"managed","profile":"work","cluster_id":"{cluster}","cluster_incarnation":"i{n}"{endpoint}}},"profile":{{"api":"https://platform.example.com/","organization_id":"org_example","auth":{{"provider":"omnigraph"}}}},"identity":{{"status":"unknown","source":"offline inspection"}}}}"#,
                    n = &cluster[1..]
                ),
            );
        }
        let home = cli.dir.join("home");
        let listed = smol::block_on(read_targets(Some(&home), &cli.programs()));
        let servers = listed.servers.clone().unwrap();
        let shown: Vec<_> = servers
            .iter()
            .map(|server| {
                (
                    server.name.as_str(),
                    server.url.as_str(),
                    server.graphs.clone(),
                    server.auth.clone(),
                    server.proxy.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            vec![
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
            ]
        );
        assert_eq!(servers[4].alternatives, ["stored-too"]);
        assert_eq!(servers[0].endpoint, "https://env.example.com/og/");
        assert_eq!(listed.watched, vec![home.join("config.yaml"), token_file]);
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
        assert!(said("cloud").contains("managed cluster, which needs Studio's own sign-in"));
        assert!(said("control").contains("does not borrow the CLI's managed token"));
        assert!(said("strategy").contains("does not support yet"));
        assert!(said("stored-too").contains("which Studio reaches as 'stored', the first by name"));
        assert_eq!(listed.errors.len(), 6, "{:?}", listed.errors);
        let managed: Vec<_> = listed
            .managed
            .iter()
            .map(|target| {
                (
                    target.name.as_str(),
                    target.profile.as_str(),
                    target.api.as_str(),
                    target.organization_id.as_str(),
                    target.cluster_id.as_str(),
                    target.cluster_incarnation.as_str(),
                    target.endpoint.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            managed,
            vec![
                (
                    "cloud",
                    "work",
                    "https://platform.example.com/",
                    "org_example",
                    "c1",
                    "i1",
                    Some("https://cluster.example.com/")
                ),
                (
                    "control",
                    "work",
                    "https://platform.example.com/",
                    "org_example",
                    "c2",
                    "i2",
                    None
                ),
            ]
        );
        let calls = cli.calls();
        assert!(
            calls.contains(&"target list --json".to_owned()),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .all(|call| !call.starts_with("credentials") || call.ends_with("--help")),
            "listing reads no credential: {calls:?}"
        );

        // The CLI's own refusal of an old-format file, as it answers `--json`.
        cli.write(
            "list-refused.json",
            r#"{"error":"target_config_invalid: operator config '/h/config.yaml': legacy target_config_version is unsupported; write a version: 1 operator document using docs/user/cli/targets.md#upgrade-from-the-old-format; there is no automatic importer","code":"target_config_invalid"}"#,
        );
        let refused = smol::block_on(read_targets(Some(&home), &cli.programs()));
        assert_eq!(refused.servers, None);
        assert_eq!(refused.errors.len(), 1);
        assert!(
            refused.errors[0].contains("legacy target_config_version is unsupported")
                && refused.errors[0].contains("upgrade-from-the-old-format"),
            "{:?}",
            refused.errors
        );
    }

    /// A token comes from `credentials export` through a pipe, after `credentials status` found
    /// one there, and stands only while `target inspect` still names the endpoint it was asked
    /// for. The CLI runs with no OmniGraph variable but `OMNIGRAPH_HOME` and the one its target
    /// names, and the token reaches no file, error or debug output.
    #[test]
    fn a_token_is_exported_through_a_pipe() {
        let cli = StandIn::new("export");
        cli.server("stored", "https://store.example.com/");
        cli.write("token-stored", "secret-stored-token");
        let source = cli.source("stored", "https://store.example.com/", CliCredential::Store);
        let token = smol::block_on(source.token()).unwrap();
        assert_eq!(format!("{token:?}"), "Token(…)");
        assert_eq!(token.into_string(), "secret-stored-token");
        assert_eq!(
            cli.calls()
                .into_iter()
                .filter(|call| !call.ends_with("--help") && call != "version --json")
                .collect::<Vec<_>>(),
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
        let moved = smol::block_on(
            cli.source("moving", "https://store.example.com/", CliCredential::Store)
                .token(),
        )
        .unwrap_err();
        assert!(
            moved.contains("changed while Studio read its token"),
            "{moved}"
        );
        assert!(!moved.contains("secret-moving-token"));
    }

    /// A refused export says the CLI's reason, without its colours or its note of the target,
    /// and never what it printed; a credential `credentials status` finds missing is said
    /// without an export; an export past its time is stopped.
    #[test]
    fn a_refused_export_says_why_and_never_the_token() {
        let cli = StandIn::new("refused");
        cli.server("stored", "https://store.example.com/");
        cli.write("fail-stored", "secret-half-printed");
        let why = smol::block_on(
            cli.source("stored", "https://store.example.com/", CliCredential::Store)
                .token(),
        )
        .unwrap_err();
        assert_eq!(
            why,
            "no credential: no token is stored for stored (`omnigraph credentials export \
             --target stored`, exit 1)"
        );
        assert!(!why.contains("secret-half-printed"));

        cli.server("filed", "https://file.example.com/");
        cli.status("filed", "https://file.example.com/", "unavailable");
        cli.calls();
        let missing = smol::block_on(
            cli.source(
                "filed",
                "https://file.example.com/",
                CliCredential::File {
                    path: "/tokens/filed".into(),
                },
            )
            .token(),
        )
        .unwrap_err();
        assert_eq!(
            missing,
            "No credential is configured for 'filed': its token file /tokens/filed is not there"
        );
        assert!(
            cli.calls()
                .iter()
                .all(|call| !call.starts_with("credentials export --target"))
        );

        cli.server("slow", "https://slow.example.com/");
        cli.write("token-slow", "secret-slow");
        cli.write("slow-slow", "");
        let mut slow = cli.source("slow", "https://slow.example.com/", CliCredential::Store);
        slow.timeout = Duration::from_millis(300);
        let stopped = smol::block_on(slow.token()).unwrap_err();
        assert!(stopped.contains("gave no token within 0 s"), "{stopped}");
        assert!(!stopped.contains("secret-slow"));
    }

    /// The CLI takes its operator home's lock without waiting, so two of its commands at once
    /// refuse each other. Every server's token asked together, as the Open dialog asks them,
    /// each from a thread of its own, takes its turn and is given; a lock another program holds
    /// is waited out a bounded number of times, then said.
    #[test]
    fn tokens_asked_together_take_turns_at_the_cli() {
        let cli = StandIn::new("turns");
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
                    futures::executor::block_on(source.token()).map(Token::into_string)
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

        // Another program holds the lock for a moment, then forever.
        let held = cli.dir.join("lock");
        std::fs::create_dir(&held).unwrap();
        let releasing = held.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            std::fs::remove_dir(&releasing).unwrap();
        });
        let source = cli.source("a", "https://a.example.com/", CliCredential::Store);
        assert_eq!(
            smol::block_on(source.token()).map(Token::into_string),
            Ok("secret-a".to_owned())
        );
        release.join().unwrap();
        std::fs::create_dir(&held).unwrap();
        let refused = smol::block_on(source.token()).unwrap_err();
        assert!(refused.contains(LOCK_REFUSED), "{refused}");
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
        assert_eq!(said(b""), "the OmniGraph CLI failed and said nothing");
    }

    /// The CLI keeps no OmniGraph variable from Studio's environment but `OMNIGRAPH_HOME` and
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
        answers: std::sync::Mutex<Vec<Result<&'static str, &'static str>>>,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl TokenSource for Answers {
        fn token(&self) -> BoxFuture<'static, Result<Token, String>> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut answers = self.answers.lock().unwrap();
            let answer = if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0]
            };
            async move { answer.map(Token::new).map_err(str::to_owned) }.boxed()
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
        answers: Vec<Result<&'static str, &'static str>>,
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
            assert!(connection.refused_with_token);
            assert_eq!(connection.label(), "Token refused");
        });
        stale.update(cx, |connection, cx| connection.refresh(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1, "Refresh asks again");
    }

    /// A source that gives no token sends the server nothing: the connection says why, and a
    /// token typed in then connects and stands until Refresh, which asks the source again.
    #[gpui::test]
    async fn no_credential_sends_nothing(cx: &mut TestAppContext) {
        let reason = "No credential is configured for 'stored': its token file /t is not there";
        let (connection, source, lists) = connection_with(vec![Err(reason)], cx);
        connection.update(cx, |connection, cx| connection.connect(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1);
        assert_eq!(lists.load(std::sync::atomic::Ordering::SeqCst), 0);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized);
            assert!(!connection.refused_with_token);
            assert_eq!(connection.token_unread.as_deref(), Some(reason));
            assert_eq!(connection.label(), "No token from the CLI");
        });
        connection.update(cx, |connection, cx| {
            connection.set_token(Some("good".into()), cx)
        });
        cx.run_until_parked();
        assert_eq!(asked(&source), 0);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Ready);
            assert_eq!(connection.token_unread, None);
        });
        connection.update(cx, |connection, cx| connection.refresh(cx));
        cx.run_until_parked();
        assert_eq!(asked(&source), 1);
        connection.read_with(cx, |connection, _| {
            assert_eq!(connection.state, ConnectionState::Unauthorized)
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
            assert!(!connection.refused_with_token);
            assert_eq!(connection.label(), "Asks for a token");
        });
    }
}

/// Against the person's own CLI and operator home, which CI has neither.
#[cfg(all(test, unix))]
mod live_tests {
    use super::*;
    use crate::cli_config::{CliAuth, operator_dir, read_targets};

    /// Every server target that takes a token is given one when all are asked at once, each
    /// from a thread of its own, as the Open dialog asks them. Only the tokens' lengths are
    /// looked at; nothing is printed.
    #[test]
    #[ignore = "needs: an OmniGraph CLI from main ff542202 on where Studio looks for one, and an \
                operator home with server targets whose credentials are configured"]
    fn every_server_target_is_given_its_token_when_asked_together() {
        let dir = operator_dir();
        let listed = smol::block_on(read_targets(dir.as_deref(), &candidates()));
        let servers = listed.servers.expect("the CLI lists its targets");
        let threads: Vec<_> = servers
            .iter()
            .filter_map(|server| match &server.auth {
                CliAuth::Token(credential) => {
                    let source = CliTokenSource::for_server(server, credential);
                    let name = server.name.clone();
                    Some(std::thread::spawn(move || {
                        let given = futures::executor::block_on(source.token())
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
