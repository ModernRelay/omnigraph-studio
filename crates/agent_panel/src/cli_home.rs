//! The agent's `omnigraph` CLI for one conversation (spec 013). A graph's conversations share
//! one agent process and one working directory, so nothing per conversation can travel in the
//! agent's environment or in one instructions file. Each conversation gets a directory of its
//! own, `<cwd>/.studio/<branch>/`:
//!
//! - `config.yaml` names the server `studio` (this conversation's gateway) and the graph;
//! - `credentials` (`0600`) holds the gateway's session token, never the person's;
//! - `omnigraph` (`0700`) runs the CLI Studio found with that directory as `OMNIGRAPH_HOME`,
//!   loopback outside any proxy, and no other OmniGraph token in its environment.
//!
//! The agent learns the wrapper's path from its conversation's MCP instructions and the
//! `omnigraph_cli` tool; `<cwd>/AGENTS.md` gives the rules shared by the graph's conversations.
//! The token lives in a file because Codex's environment filter drops names like `*TOKEN*`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::FutureExt as _;

/// One conversation's CLI home. Dropping it removes the session token.
pub struct CliHome {
    pub dir: PathBuf,
    /// The wrapper the agent runs.
    pub command: PathBuf,
    pub graph: String,
    pub branch: String,
    pub base: String,
}

impl Drop for CliHome {
    fn drop(&mut self) {
        std::fs::remove_file(self.dir.join("credentials")).ok();
    }
}

impl CliHome {
    /// Writes the conversation's directory, config and credentials. The wrapper is written by
    /// [`CliHome::use_cli`] once a CLI is chosen.
    pub fn write(
        cwd: &Path,
        port: u16,
        token: &str,
        graph: &str,
        branch: &str,
        base: &str,
    ) -> std::io::Result<Self> {
        let parent = cwd.join(".studio");
        let dir = parent.join(branch.replace('/', "-"));
        std::fs::create_dir_all(&dir)?;
        private_dir(&parent)?;
        private_dir(&dir)?;
        write_file(
            &dir.join("config.yaml"),
            &format!(
                "# Written by OmniGraph Studio for one agent conversation (spec 013).\n\
                 servers:\n  studio: {{ url: \"http://127.0.0.1:{port}\" }}\n\
                 defaults:\n  server: studio\n  default_graph: {}\n",
                yaml_string(graph)
            ),
            0o600,
        )?;
        write_file(
            &dir.join("credentials"),
            &format!("[studio]\ntoken = {token}\n"),
            0o600,
        )?;
        Ok(Self {
            command: dir.join("omnigraph"),
            dir,
            graph: graph.to_owned(),
            branch: branch.to_owned(),
            base: base.to_owned(),
        })
    }

    /// Points the wrapper at `cli`.
    pub fn use_cli(&self, cli: &Path) -> std::io::Result<()> {
        let script = format!(
            "#!/bin/sh\n\
             # Written by OmniGraph Studio for one agent conversation (spec 013): the omnigraph\n\
             # CLI, pointed at Studio's gateway for this conversation.\n\
             for name in $(env | sed -n 's/^\\(OMNIGRAPH_TOKEN_[A-Za-z0-9_]*\\)=.*/\\1/p'); do unset \"$name\"; done\n\
             unset OMNIGRAPH_BEARER_TOKEN OMNIGRAPH_PROFILE OMNIGRAPH_CONTROL_TOKEN OMNIGRAPH_CONTROL_API\n\
             OMNIGRAPH_HOME={home}\n\
             NO_PROXY=\"127.0.0.1,localhost${{NO_PROXY:+,$NO_PROXY}}\"\n\
             no_proxy=\"$NO_PROXY\"\n\
             export OMNIGRAPH_HOME NO_PROXY no_proxy\n\
             exec {cli} \"$@\"\n",
            home = shell_quote(&self.dir.to_string_lossy()),
            cli = shell_quote(&cli.to_string_lossy()),
        );
        write_file(&self.command, &script, 0o700)
    }

    /// Makes the wrapper run nothing and say `why`: the conversation has no CLI that speaks the
    /// graph's contract, and one the server would refuse must not run in its place.
    pub fn refuse(&self, why: &str) -> std::io::Result<()> {
        let script = format!(
            "#!/bin/sh\n\
             # Written by OmniGraph Studio for one agent conversation (spec 013).\n\
             echo {why} >&2\n\
             exit 1\n",
            why = shell_quote(why),
        );
        write_file(&self.command, &script, 0o700)
    }

    /// What the agent is told about this conversation's CLI: in its MCP instructions and by the
    /// `omnigraph_cli` tool.
    pub fn instructions(&self, cli_ready: Option<bool>) -> String {
        let command = self.command.to_string_lossy();
        let flags = format!("--server studio --graph {}", self.graph);
        match cli_ready {
            Some(false) => format!(
                "Studio found no omnigraph CLI that speaks this graph's HTTP API, so the graph is \
                 not available in this conversation. Tell the person: they can install a \
                 matching omnigraph CLI or set `agent.omnigraph_cli`. Your branch would be \
                 {branch}.",
                branch = self.branch
            ),
            _ => format!(
                "This conversation's omnigraph CLI is {command} (run it by that path; another \
                 omnigraph on PATH is not connected to Studio). Pass {flags} to every command. \
                 Read any branch. Write only to your branch, {branch}: create it first with \
                 `{command} branch create {branch} --from {base} {flags}`, then pass \
                 `--branch {branch}` to mutate and load. Prefer one load of NDJSON to many \
                 mutations. Studio refuses other writes before sending them (\"Nothing was \
                 sent\"). If your shell has no network (\"server discovery connection \
                 failed\"), run the same commands with the omnigraph_run tool. See AGENTS.md for \
                 the rules.",
                branch = self.branch,
                base = self.base,
            ),
        }
    }
}

/// Writes `<cwd>/AGENTS.md`, the rules every conversation on the graph shares. Codex reads
/// `AGENTS.md`; Claude reads it when there is no `CLAUDE.md`.
pub fn write_agents_md(cwd: &Path, graph: &str) -> std::io::Result<()> {
    let text = format!(
        "# OmniGraph graph `{graph}`\n\n\
         Written by OmniGraph Studio. You work on the OmniGraph graph `{graph}` with a person in \
         Studio.\n\n\
         Before writing a query, a mutation or a load, read the `omnigraph-studio` skill \
         (`.claude/skills/omnigraph-studio/SKILL.md` or `.agents/skills/omnigraph-studio/SKILL.md` \
         here): GQ syntax, loads, search, and what differs in Studio.\n\n\
         ## The omnigraph CLI\n\n\
         - Studio sets up an `omnigraph` CLI for each conversation. Its absolute path is in the \
         instructions of the `omnigraph` MCP server, and the `omnigraph_cli` tool returns it \
         with your branch. Run it by that path: another `omnigraph` on PATH is not connected \
         to Studio.\n\
         - Pass `--server studio --graph {graph}` to every command.\n\
         - Read any branch or commit (`--branch`, `--snapshot`).\n\
         - Write only to your own branch. Create it first with `branch create <your branch> \
         --from <its base>`, then pass `--branch <your branch>` to `mutate`, `load` and Blob \
         writes.\n\
         - Batch writes with `load` (NDJSON): each `mutate` is a commit of its own.\n\
         - Branch and commit statements inside `mutate -e`, merges into other branches, and \
         publishing or dropping commits are refused. The person merges your branch in Studio's \
         review.\n\n\
         ## Outcomes\n\n\
         - A refusal from Studio says \"Nothing was sent\": nothing was written, even when the \
         CLI also prints `effects: unknown`.\n\
         - `effects: unknown` on a write Studio sent means the server's answer was lost. Check \
         `commit list --branch <your branch>` before trying again; never repeat a write blindly.\n\
         - \"server discovery connection failed\" means your sandbox has no network access. \
         Run the same commands with the `omnigraph_run` MCP tool instead: pass the arguments \
         after `omnigraph` as a list (Studio adds `--server studio` and the graph), and set \
         `save_to` to write a large result to a file in your working directory, then process \
         that file in your shell. Files it reads or writes must be in your working \
         directory.\n"
    );
    write_file(&cwd.join("AGENTS.md"), &text, 0o644)
}

/// The CLIs to try, in order: the configured one, `omnigraph` on PATH, then the usual places.
pub fn candidates(configured: Option<&str>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if is_executable(&path)
            && !found
                .iter()
                .any(|known| std::fs::canonicalize(known).unwrap_or(known.clone()) == canonical)
        {
            found.push(path);
        }
    };
    if let Some(configured) = configured.filter(|path| !path.is_empty()) {
        push(expand_home(configured));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            push(dir.join("omnigraph"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        push(PathBuf::from(home).join(".local/bin/omnigraph"));
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        push(Path::new(dir).join("omnigraph"));
    }
    found
}

/// What a CLI says about itself, read without a server: its release and the HTTP contract it
/// speaks (the `Omnigraph-Http-Api` value, which a server takes only when it is its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliFacts {
    /// Its release, as it reports it ("0.13.0").
    pub version: Option<String>,
    /// `None` for a CLI from before 0.13, or one that did not answer.
    pub contract: Option<String>,
}

/// How long a CLI may take to print its version.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Reads a CLI's release and contract by running it, never a server. `version --json` reports
/// the contract (`http_contract`, OmniGraph #987); 0.13.0, which predates it, speaks `0.13`.
/// Studio speaks 0.13 and later only, so an older release has no contract here.
pub async fn cli_facts(cli: &Path) -> CliFacts {
    if let Some(json) = output(cli, &["version", "--json"]).await
        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&json)
        && let Some(contract) = json.get("http_contract").and_then(|c| c.as_str())
    {
        return CliFacts {
            version: json
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            contract: Some(contract.to_owned()),
        };
    }
    let version = output(cli, &["--version"])
        .await
        .and_then(|text| release(&String::from_utf8_lossy(&text)));
    CliFacts {
        contract: version
            .as_deref()
            .filter(|version| version.starts_with("0.13."))
            .map(|_| "0.13".to_owned()),
        version,
    }
}

/// The release in a `--version` line: `omnigraph 0.13.0` is `0.13.0`.
fn release(text: &str) -> Option<String> {
    let word = text.lines().next()?.split_whitespace().nth(1)?;
    word.chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
        .then(|| word.to_owned())
}

/// A CLI's standard output when it exits with success within [`VERSION_TIMEOUT`].
async fn output(cli: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let run = smol::process::Command::new(cli)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .fuse();
    let timeout = smol::Timer::after(VERSION_TIMEOUT).fuse();
    futures::pin_mut!(run, timeout);
    futures::select! {
        output = run => output.ok().filter(|output| output.status.success()).map(|output| output.stdout),
        _ = timeout => None,
    }
}

/// The conversation's CLI, or why it has none.
#[derive(Debug)]
pub enum CliChoice {
    Chosen {
        cli: PathBuf,
        version: Option<String>,
    },
    /// No candidate speaks the graph's contract: each one found, with what it said.
    NoMatch { found: Vec<(PathBuf, CliFacts)> },
}

/// Chooses the conversation's CLI from `candidates`, in their order: the first that speaks
/// `contract`, as the CLI itself reports it, so a slow or unavailable server never decides it.
/// With no match the wrapper runs nothing and says why, rather than pointing at a CLI the server
/// would refuse.
pub async fn choose_cli(home: &CliHome, candidates: Vec<PathBuf>, contract: &str) -> CliChoice {
    let mut found = Vec::new();
    for cli in candidates {
        let facts = cli_facts(&cli).await;
        found.push((cli, facts));
    }
    if let Some((cli, facts)) = found
        .iter()
        .find(|(_, facts)| facts.contract.as_deref() == Some(contract))
        && home.use_cli(cli).is_ok()
    {
        return CliChoice::Chosen {
            cli: cli.clone(),
            version: facts.version.clone(),
        };
    }
    home.refuse(&no_cli_text(contract, &found)).ok();
    CliChoice::NoMatch { found }
}

/// Why a conversation has no CLI: the contract the graph needs and each CLI found.
pub fn no_cli_text(contract: &str, found: &[(PathBuf, CliFacts)]) -> String {
    let mut text = format!("No omnigraph CLI here speaks this graph's HTTP API ({contract})");
    if found.is_empty() {
        text.push_str(": Studio found none");
    } else {
        let each: Vec<String> = found
            .iter()
            .map(|(cli, facts)| {
                let what = match (&facts.version, &facts.contract) {
                    (Some(version), Some(speaks)) => format!("{version}, which speaks {speaks}"),
                    (Some(version), None) => format!("{version}, older than 0.13"),
                    (None, _) => "did not say its version".to_owned(),
                };
                format!("{} is {what}", cli.display())
            })
            .collect();
        text.push_str(": ");
        text.push_str(&each.join("; "));
    }
    text.push_str(&format!(
        ". Install omnigraph {contract}, or set `agent.omnigraph_cli` to one."
    ));
    text
}

/// Subcommands `omnigraph_run` passes on: the ones that speak to a server, which the gateway
/// then checks. Storage, cluster, policy and login commands refuse `--server` anyway.
const RUN_COMMANDS: &[&str] = &[
    "query", "mutate", "load", "branch", "snapshot", "export", "blob", "commit", "changes",
    "schema", "graphs", "version", "lint",
];
/// What `lint` may take: it checks queries against a schema file offline, never a graph.
const LINT_FLAGS: &[&str] = &["--query", "--schema", "--json"];
/// Flags that would choose another address, credentials or actor than the conversation's.
const RUN_FORBIDDEN: &[&str] = &[
    "--server",
    "--store",
    "--uri",
    "--cluster",
    "--profile",
    "--direct",
    "--managed",
    "--api",
    "--as",
];
/// Flags whose value is a file the CLI reads or writes; it must be in the working directory.
const RUN_PATH_FLAGS: &[&str] = &[
    "--query",
    "--data",
    "--params-file",
    "--file",
    "--out",
    "--schema",
];
/// The most output `omnigraph_run` returns in its answer; more is cut, with a pointer to
/// `save_to`, so a large result does not fill the agent's context.
pub const RUN_INLINE_MAX: usize = 100 * 1024;
/// The most output `omnigraph_run` reads and saves.
pub const RUN_OUTPUT_MAX: usize = 64 * 1024 * 1024;
/// How long one `omnigraph_run` may take; a write held for approval waits within it.
const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long an `omnigraph_run` call waits before it answers with the run's number instead.
/// Agents give up on a tool call without telling Studio (Codex after 300 seconds), and a write
/// waiting for the person can take longer; an agent that thought it failed might repeat it.
pub const RUN_ANSWER_WITHIN: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(240)
};

/// The answer to a call whose run is still going.
pub fn still_running(run: u64) -> (String, bool) {
    (
        format!(
            "Still running in Studio as run {run}; nothing about its outcome is known yet. Do not \
             run the command again. Call omnigraph_run with {{\"wait\": {run}}} for its outcome."
        ),
        false,
    )
}

/// `run`'s answer if it finishes within `within`, else `None` (the run keeps going).
pub async fn answer_within<F>(run: F, within: Duration) -> Option<(String, bool)>
where
    F: Future<Output = (String, bool)> + Unpin,
{
    let mut timer = futures::FutureExt::fuse(smol::Timer::after(within));
    let mut run = futures::FutureExt::fuse(run);
    futures::select! {
        answer = run => Some(answer),
        _ = timer => None,
    }
}

/// Checks the arguments the agent gave `omnigraph_run` (spec 013): the CLI runs in Studio's
/// process, outside the agent's sandbox, so it may only speak to this conversation's gateway
/// and touch files in the conversation's working directory. Returns the arguments to run, with
/// `--server studio` (and the conversation's graph unless one is named) added.
pub fn check_run_args(args: &[String], cwd: &Path, graph: &str) -> Result<Vec<String>, String> {
    let Some(command) = args.first() else {
        return Err("Pass the omnigraph arguments, for example [\"query\", \"-e\", \"…\"].".into());
    };
    if command == "omnigraph" {
        return Err(
            "Leave out the program name: start with the subcommand, such as \"query\".".into(),
        );
    }
    if !RUN_COMMANDS.contains(&command.as_str()) {
        return Err(format!(
            "omnigraph_run runs {}; `{command}` is not one of them.",
            RUN_COMMANDS.join(", ")
        ));
    }
    if (command == "schema" && args.get(1).map(String::as_str) != Some("show"))
        || (command == "graphs" && args.get(1).map(String::as_str) != Some("list"))
    {
        return Err(format!("Only `{command} show`/`list` read through Studio."));
    }
    // `lint` reads a schema file, not the graph: a positional storage URI would make Studio
    // open storage directly, so only its file flags pass, and no server is added.
    let lint = command == "lint";
    if lint {
        let mut rest = args[1..].iter();
        while let Some(arg) = rest.next() {
            let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
            if !LINT_FLAGS.contains(&flag) {
                return Err(format!(
                    "`lint` takes only --query, --schema and --json; save the schema with \
                     `schema show` (save_to) and lint against that file. `{arg}` is not passed."
                ));
            }
            if flag != "--json" && !arg.contains('=') {
                rest.next();
            }
        }
        if !args.iter().any(|arg| arg.starts_with("--schema")) {
            return Err("`lint` needs --schema <file>: save it first with `schema show`.".into());
        }
    }
    let mut checked = Vec::with_capacity(args.len() + 4);
    let mut has_graph = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        if RUN_FORBIDDEN.contains(&flag) {
            return Err(format!(
                "`{flag}` is not passed: this conversation's CLI speaks only to Studio, which adds \
                 `--server studio` itself."
            ));
        }
        if flag == "--graph" {
            has_graph = true;
        }
        if RUN_PATH_FLAGS.contains(&flag) {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| format!("`{flag}` needs a path."))?
                }
            };
            let path = within(cwd, &value, flag == "--out")?;
            checked.push(flag.to_owned());
            checked.push(path.to_string_lossy().into_owned());
        } else {
            checked.push(arg.clone());
        }
        index += 1;
    }
    if lint {
        return Ok(checked);
    }
    checked.extend(["--server".to_owned(), "studio".to_owned()]);
    if !has_graph {
        checked.extend(["--graph".to_owned(), graph.to_owned()]);
    }
    Ok(checked)
}

/// `value` as a path inside `cwd`, resolved against it. An existing path is resolved through
/// its links; a file to be written (`create`) must have an existing parent inside `cwd`.
pub fn within(cwd: &Path, value: &str, create: bool) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(cwd).map_err(|error| error.to_string())?;
    let path = cwd.join(value);
    let resolved = if create && !path.exists() {
        let parent = path.parent().ok_or("no parent directory")?;
        let parent = std::fs::canonicalize(parent)
            .map_err(|_| format!("{value}: its directory does not exist"))?;
        parent.join(path.file_name().ok_or("no file name")?)
    } else {
        std::fs::canonicalize(&path).map_err(|_| format!("{value}: no such file"))?
    };
    if !resolved.starts_with(&root) {
        return Err(format!(
            "{value}: omnigraph_run reads and writes files only in your working directory, {}.",
            cwd.display()
        ));
    }
    Ok(resolved)
}

/// Runs this conversation's CLI with checked arguments in `cwd`, with no shell. Returns the
/// tool's text and whether the command failed. `save_to` writes standard output to a file in
/// `cwd` instead of returning it.
pub async fn run(
    home: &CliHome,
    cwd: &Path,
    args: Vec<String>,
    save_to: Option<PathBuf>,
) -> (String, bool) {
    use futures::AsyncReadExt as _;
    let child = smol::process::Command::new(&home.command)
        .args(&args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return (format!("Studio could not run the CLI: {error}"), true),
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return ("Studio could not read the CLI's output.".into(), true);
    };
    let read = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let limit = RUN_OUTPUT_MAX as u64 + 1;
        let (mut stdout, mut stderr) = (stdout.take(limit), stderr.take(limit));
        let (a, b) = futures::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a.ok();
        b.ok();
        let status = child.status().await;
        (out, err, status)
    }
    .fuse();
    let timeout = smol::Timer::after(RUN_TIMEOUT).fuse();
    futures::pin_mut!(read, timeout);
    let (out, err, status) = futures::select! {
        done = read => done,
        _ = timeout => return ("The command took longer than 30 minutes and was stopped.".into(), true),
    };
    let code = status.ok().and_then(|status| status.code());
    let failed = code != Some(0);
    let clip = |bytes: &[u8]| {
        let mut text =
            String::from_utf8_lossy(&bytes[..bytes.len().min(RUN_INLINE_MAX)]).into_owned();
        if bytes.len() > RUN_INLINE_MAX {
            text.push_str(&format!(
                "\n[cut at {} KB of {} bytes; run it again with save_to and read the file]",
                RUN_INLINE_MAX / 1024,
                bytes.len()
            ));
        }
        text
    };
    let mut text = format!(
        "exit {}\n",
        code.map_or("signal".into(), |code| code.to_string())
    );
    match save_to {
        Some(path) if !failed => match std::fs::write(&path, &out[..out.len().min(RUN_OUTPUT_MAX)])
        {
            Ok(()) => text.push_str(&format!(
                "output saved to {} ({} bytes)\n",
                path.display(),
                out.len().min(RUN_OUTPUT_MAX)
            )),
            Err(error) => return (format!("Studio could not save the output: {error}"), true),
        },
        _ => text.push_str(&clip(&out)),
    }
    if !err.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&clip(&err));
    }
    (text, failed)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Writes `path` with `mode` from the start, so a secret is never readable by others.
fn write_file(path: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    // An existing file keeps its old mode on open; set it.
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    file.write_all(text.as_bytes())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn yaml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// The first candidate that speaks the graph's contract is chosen, as the CLI reports it and
    /// with no server behind the gateway; with none, the wrapper runs nothing and names each CLI
    /// found.
    #[test]
    fn the_cli_that_speaks_the_contract_is_chosen_without_a_server() {
        assert_eq!(release("omnigraph 0.13.0\n").as_deref(), Some("0.13.0"));
        assert_eq!(release("usage: …"), None);
        let dir = std::env::temp_dir().join(format!("studio-cli-choice-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let fake = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // 0.11 and 0.13.0 predate `version --json`; every other command fails, as one would with
        // no server behind the gateway.
        let old = fake(
            "omnigraph-0.11",
            r#"[ "$1" = --version ] && { echo "omnigraph 0.11.0"; exit 0; }; exit 2"#,
        );
        let current = fake(
            "omnigraph-0.13",
            r#"[ "$1" = --version ] && { echo "omnigraph 0.13.0"; exit 0; }; exit 2"#,
        );
        let reports = fake(
            "omnigraph-0.14",
            r#"[ "$1 $2" = "version --json" ] && { echo '{"version":"0.14.0","http_contract":"0.13"}'; exit 0; }; exit 2"#,
        );
        let other = fake(
            "omnigraph-0.15",
            r#"[ "$1 $2" = "version --json" ] && { echo '{"version":"0.15.0","http_contract":"0.15"}'; exit 0; }; exit 2"#,
        );
        let home = CliHome::write(&dir, 1, "t", "knowledge", "agent-x", "main").unwrap();
        let wrapper = || std::fs::read_to_string(&home.command).unwrap();
        let choose =
            |candidates: Vec<PathBuf>| smol::block_on(choose_cli(&home, candidates, "0.13"));

        match choose(vec![old.clone(), current.clone()]) {
            CliChoice::Chosen { cli, version } => {
                assert_eq!(cli, current);
                assert_eq!(version.as_deref(), Some("0.13.0"));
            }
            other => panic!("{other:?}"),
        }
        assert!(wrapper().contains(&*current.to_string_lossy()));

        match choose(vec![other.clone(), reports.clone()]) {
            CliChoice::Chosen { cli, .. } => assert_eq!(cli, reports),
            other => panic!("{other:?}"),
        }

        match choose(vec![old.clone(), other.clone()]) {
            CliChoice::NoMatch { found } => {
                let text = no_cli_text("0.13", &found);
                assert!(text.contains("0.11.0, older than 0.13"), "{text}");
                assert!(text.contains("0.15.0, which speaks 0.15"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        // It names the CLIs it found but runs none of them.
        assert!(!wrapper().contains("exec "), "{}", wrapper());
        let run = std::process::Command::new(&home.command)
            .arg("query")
            .output()
            .unwrap();
        assert!(!run.status.success());
        assert!(String::from_utf8_lossy(&run.stderr).contains("No omnigraph CLI here speaks"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `omnigraph_run` runs outside the agent's sandbox: it speaks only to the conversation's
    /// gateway and touches files only in the conversation's working directory.
    #[test]
    fn run_arguments_stay_on_the_gateway_and_in_the_working_directory() {
        let cwd = std::env::temp_dir().join(format!("studio-run-args-{}", std::process::id()));
        std::fs::create_dir_all(cwd.join("data")).unwrap();
        std::fs::write(cwd.join("data/rows.jsonl"), "{}\n").unwrap();
        std::fs::write(cwd.join("q.gq"), "").unwrap();
        std::fs::write(cwd.join("schema.pg"), "").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", cwd.join("escape")).ok();
        let cwd = std::fs::canonicalize(&cwd).unwrap();
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let check = |list: &[&str]| check_run_args(&args(list), &cwd, "knowledge");

        assert_eq!(
            check(&["query", "-e", "query q() { … }", "--json"]).unwrap(),
            args(&[
                "query",
                "-e",
                "query q() { … }",
                "--json",
                "--server",
                "studio",
                "--graph",
                "knowledge"
            ])
        );
        // A graph the agent names stays; a relative file becomes an absolute path inside.
        assert_eq!(
            check(&[
                "load",
                "--data",
                "data/rows.jsonl",
                "--mode",
                "append",
                "--graph",
                "notes"
            ])
            .unwrap(),
            args(&[
                "load",
                "--data",
                &cwd.join("data/rows.jsonl").to_string_lossy(),
                "--mode",
                "append",
                "--graph",
                "notes",
                "--server",
                "studio",
            ])
        );
        // `lint` checks against a saved schema file, offline: no server is added.
        assert_eq!(
            check(&["lint", "--query", "q.gq", "--schema=schema.pg", "--json"]).unwrap(),
            args(&[
                "lint",
                "--query",
                &cwd.join("q.gq").to_string_lossy(),
                "--schema",
                &cwd.join("schema.pg").to_string_lossy(),
                "--json",
            ])
        );
        let out = check(&["blob", "get", "--out=data/picture.png"]).unwrap();
        assert_eq!(out[2], "--out");
        assert_eq!(out[3], cwd.join("data/picture.png").to_string_lossy());

        for refused in [
            &["init", "--store", "x"][..],
            &["omnigraph", "query"],
            &["schema", "apply"],
            &["query", "--server", "http://elsewhere"],
            &["query", "--store=file:///tmp/g"],
            &["mutate", "--as", "someone"],
            &["load", "--data", "/etc/passwd"],
            &["load", "--data", "../outside.jsonl"],
            &["blob", "put", "--file", "escape"],
            &["blob", "get", "--out", "missing/dir/x"],
            &["lint", "--query", "q.gq"],
            &[
                "lint",
                "s3://bucket/graph",
                "--query",
                "q.gq",
                "--schema",
                "schema.pg",
            ],
            &["lint", "--query", "q.gq", "--schema", "/etc/passwd"],
            &[],
        ] {
            assert!(check(refused).is_err(), "{refused:?} was let through");
        }
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn a_slow_run_answers_with_its_number_and_keeps_going() {
        smol::block_on(async {
            let (done_tx, done_rx) = futures::channel::oneshot::channel::<()>();
            let run = futures::FutureExt::shared(Box::pin(async move {
                done_rx.await.ok();
                ("exit 0\nrows".to_owned(), false)
            }));
            // Still going when the call answers: the number, not an outcome.
            assert_eq!(answer_within(run.clone(), RUN_ANSWER_WITHIN).await, None);
            let (text, error) = still_running(4);
            assert!(!error);
            assert!(
                text.contains("run 4") && text.contains(r#"{"wait": 4}"#),
                "{text}"
            );
            // A later wait gets the outcome once the run finishes.
            done_tx.send(()).ok();
            assert_eq!(
                answer_within(run, RUN_ANSWER_WITHIN).await,
                Some(("exit 0\nrows".to_owned(), false))
            );
        });
    }

    #[test]
    fn a_conversation_home_holds_its_config_token_and_wrapper() {
        let cwd = std::env::temp_dir().join(format!("studio-cli-home-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let home = CliHome::write(
            &cwd,
            49152,
            "abc123",
            "knowledge",
            "agent/20261013-101500",
            "main",
        )
        .unwrap();
        assert_eq!(home.dir, cwd.join(".studio/agent-20261013-101500"));
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home.dir), 0o700);
        assert_eq!(mode(&home.dir.join("credentials")), 0o600);
        let config = std::fs::read_to_string(home.dir.join("config.yaml")).unwrap();
        assert!(
            config.contains("studio: { url: \"http://127.0.0.1:49152\" }"),
            "{config}"
        );
        assert!(config.contains("default_graph: \"knowledge\""), "{config}");
        assert_eq!(
            std::fs::read_to_string(home.dir.join("credentials")).unwrap(),
            "[studio]\ntoken = abc123\n"
        );

        home.use_cli(Path::new("/opt/it's here/omnigraph")).unwrap();
        assert_eq!(mode(&home.command), 0o700);
        let script = std::fs::read_to_string(&home.command).unwrap();
        assert!(
            script.contains("exec '/opt/it'\\''s here/omnigraph' \"$@\""),
            "{script}"
        );
        assert!(script.contains(&format!("OMNIGRAPH_HOME='{}'", home.dir.display())));

        // The wrapper runs with its own home, loopback outside any proxy, and no other token.
        home.use_cli(Path::new("/usr/bin/env")).unwrap();
        let output = std::process::Command::new(&home.command)
            .env("OMNIGRAPH_TOKEN_PERSONAL", "secret")
            .env("OMNIGRAPH_BEARER_TOKEN", "secret")
            .env("NO_PROXY", "corp.example")
            .output()
            .unwrap();
        let env = String::from_utf8_lossy(&output.stdout);
        assert!(!env.contains("secret"), "{env}");
        assert!(
            env.contains("NO_PROXY=127.0.0.1,localhost,corp.example"),
            "{env}"
        );
        assert!(
            env.contains(&format!("OMNIGRAPH_HOME={}", home.dir.display())),
            "{env}"
        );

        // Claude Code keeps the first 2,048 characters of a server's instructions, so the
        // server's and the conversation's fit, with a long data directory in the command path.
        let long = cwd
            .join("d".repeat(120))
            .join(".studio/agent-20261013-101500");
        let longest = CliHome {
            command: long.join("omnigraph"),
            dir: long,
            graph: "g".repeat(64),
            branch: "agent/20261013-101500".into(),
            base: "b".repeat(64),
        };
        for ready in [None, Some(true), Some(false)] {
            let length = crate::mcp::INSTRUCTIONS.len() + 2 + longest.instructions(ready).len();
            assert!(length <= 2048, "{length} characters");
        }

        let credentials = home.dir.join("credentials");
        drop(home);
        assert!(
            !credentials.exists(),
            "the token goes with the conversation"
        );
        write_agents_md(&cwd, "knowledge").unwrap();
        let rules = std::fs::read_to_string(cwd.join("AGENTS.md")).unwrap();
        assert!(rules.contains("--server studio --graph knowledge"));
        std::fs::remove_dir_all(&cwd).ok();
    }
}
