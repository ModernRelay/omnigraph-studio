//! The agent's `omnigraph` CLI for one conversation (spec 013). A graph's conversations share
//! one agent process and one working directory, so nothing per conversation can travel in the
//! agent's environment or in one instructions file. Each conversation gets a directory of its
//! own, `<cwd>/.studio/<branch>/`:
//!
//! - `config.yaml` names this conversation's gateway as the target `studio`, its default, with the
//!   graph;
//! - `credentials` (`0600`) holds the gateway's session token, never the person's, bound to the
//!   gateway's address;
//! - `omnigraph` (`0700`) runs the CLI Studio found with that directory as `OMNIGRAPH_HOME`,
//!   loopback outside any proxy, and no other OmniGraph token in its environment: a shell
//!   script on Unix, a batch file `omnigraph.cmd` on Windows ([`Wrapper`]).
//!
//! The agent learns the wrapper's path from its conversation's MCP instructions and the
//! `omnigraph_cli` tool; `<cwd>/AGENTS.md` gives the rules shared by the graph's conversations.
//! The token lives in a file because Codex's environment filter drops names like `*TOKEN*`.
//!
//! The agent can rewrite anything in its working directory, so `omnigraph_run`, which runs
//! outside the agent's sandbox, never uses that folder: it starts the chosen CLI itself, with a
//! second home of the same config in a private folder outside the working directory, and runs
//! there.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::confined::Confined;
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
    /// The agent's working directory, and the home's folder in it. The agent writes there too,
    /// so each write opens the folder again by handle (`confined`).
    cwd: PathBuf,
    folder: String,
    /// The credentials file as written, so a later conversation's file in the same folder is
    /// left alone when this one ends.
    credentials: String,
    /// The home `omnigraph_run` uses, outside the working directory; its runs stage there too.
    private: PathBuf,
    /// The CLI chosen for the conversation, which `omnigraph_run` starts itself, or why there
    /// is none.
    cli: std::sync::Mutex<Result<PathBuf, String>>,
    /// Whether the wrapper runs the chosen CLI. On Windows the batch file names it only by a
    /// path it can hold ([`cmd_program`]); otherwise the wrapper refuses, and the agent is told
    /// to use `omnigraph_run`, which starts the CLI by its path.
    wrapper_runs: std::sync::atomic::AtomicBool,
}

/// The target protocol of the home Studio writes (`target_config_version: 1`). A CLI is chosen
/// only when its `version --json` reports it.
const TARGET_PROTOCOL: u64 = 1;

/// The CLI config naming the gateway on `port` as the target `studio`, its default, with `graph`.
fn config(port: u16, graph: &str) -> String {
    format!(
        "# Written by OmniGraph Studio for one agent conversation (spec 013).\n\
         target_config_version: 1\n\
         targets:\n  studio: {{ kind: server, endpoint: \"http://127.0.0.1:{port}\", \
         credential: studio, default_graph: {} }}\n\
         defaults:\n  target: studio\n",
        yaml_string(graph)
    )
}

impl Drop for CliHome {
    fn drop(&mut self) {
        // Opened, not made: a folder removed meanwhile is not made again to be cleaned.
        if let Ok(home) = Confined::open(&self.cwd).and_then(|cwd| cwd.existing_dirs(&self.folder))
            && home.holds("credentials", self.credentials.as_bytes())
        {
            home.remove("credentials".as_ref()).ok();
        }
        std::fs::remove_dir_all(&self.private).ok();
    }
}

impl CliHome {
    /// Writes the conversation's directory, config and credentials, and the private home
    /// `omnigraph_run` uses, in a folder of its own under `private_root`. The wrapper is written
    /// by [`CliHome::use_cli`] once a CLI is chosen.
    pub fn write(
        cwd: &Path,
        private_root: &Path,
        port: u16,
        token: &str,
        graph: &str,
        branch: &str,
        base: &str,
    ) -> std::io::Result<Self> {
        let name = branch.replace('/', "-");
        let config = config(port, graph);
        // The CLI sends a saved token only under the address it was saved for.
        let credentials =
            format!("[studio]\ntoken = {token}\nendpoint = http://127.0.0.1:{port}\n");
        // Private: the credentials hold the gateway's token.
        let studio = Confined::open(cwd)?.dir(".studio")?;
        studio.set_mode(0o700)?;
        let home = studio.dir(&name)?;
        home.set_mode(0o700)?;
        home.write("config.yaml", config.as_bytes(), 0o600)?;
        home.write("credentials", credentials.as_bytes(), 0o600)?;

        std::fs::create_dir_all(private_root)?;
        let root = Confined::open(private_root)?;
        root.set_mode(0o700)?;
        let folder = format!("{name}-{}", crate::confined::random_hex(8)?);
        let own = root.dir(&folder)?;
        own.set_mode(0o700)?;
        own.write("config.yaml", config.as_bytes(), 0o600)?;
        own.write("credentials", credentials.as_bytes(), 0o600)?;

        let dir = cwd.join(".studio").join(&name);
        Ok(Self {
            command: dir.join(WRAPPER_NAME),
            dir,
            graph: graph.to_owned(),
            branch: branch.to_owned(),
            base: base.to_owned(),
            cwd: cwd.to_owned(),
            folder: format!(".studio/{name}"),
            credentials,
            private: private_root.join(folder),
            cli: std::sync::Mutex::new(Err("Studio has not chosen an omnigraph CLI yet.".into())),
            wrapper_runs: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// The CLI `omnigraph_run` starts, or why there is none.
    fn cli(&self) -> Result<PathBuf, String> {
        self.cli
            .lock()
            .map_err(|_| "Studio lost track of the conversation's CLI.".to_owned())?
            .clone()
    }

    fn set_cli(&self, cli: Result<&Path, &str>) {
        if let Ok(mut chosen) = self.cli.lock() {
            *chosen = cli.map(Path::to_owned).map_err(str::to_owned);
        }
    }

    /// The chosen CLI as `omnigraph_run` starts it, in `dir`: the private home, loopback
    /// outside any proxy, and none of Studio's own OmniGraph variables (its tokens, profile or
    /// home), so it speaks only to this conversation's gateway.
    fn command(&self, cli: &Path, dir: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(cli);
        for (name, _) in std::env::vars_os() {
            // Windows reads a variable by its name in any case.
            if crate::host::is_omnigraph_variable(&name.to_string_lossy(), cfg!(windows)) {
                command.env_remove(name);
            }
        }
        let no_proxy = match std::env::var("NO_PROXY") {
            Ok(also) if !also.is_empty() => format!("{LOOPBACK},{also}"),
            _ => LOOPBACK.to_owned(),
        };
        command
            .env("OMNIGRAPH_HOME", &self.private)
            .env("NO_PROXY", &no_proxy)
            .env("no_proxy", &no_proxy)
            .current_dir(dir);
        command
    }

    /// The home's folder, opened by handle.
    fn home(&self) -> std::io::Result<Confined> {
        Confined::open(&self.cwd)?.dirs(&self.folder)
    }

    /// Points the wrapper and `omnigraph_run` at `cli`: whether the wrapper runs it. On Windows
    /// a batch file that cannot name the CLI refuses and points at `omnigraph_run`, which still
    /// runs it.
    pub fn use_cli(&self, cli: &Path) -> std::io::Result<bool> {
        self.use_cli_on(cli, cfg!(windows))
    }

    /// [`CliHome::use_cli`] with the batch file when `windows`.
    fn use_cli_on(&self, cli: &Path, windows: bool) -> std::io::Result<bool> {
        let wrapper = Wrapper::Run {
            home: &self.dir,
            cli,
        };
        self.home()?
            .write(WRAPPER_NAME, wrapper.script_on(windows).as_bytes(), 0o700)?;
        let runs = !windows || cmd_program(&self.dir, cli).is_some();
        self.wrapper_runs
            .store(runs, std::sync::atomic::Ordering::Relaxed);
        self.set_cli(Ok(cli));
        Ok(runs)
    }

    /// Makes the wrapper run nothing and say `why`: the conversation has no CLI that speaks the
    /// graph's contract, and one the server would refuse must not run in its place.
    pub fn refuse(&self, why: &str) -> std::io::Result<()> {
        self.set_cli(Err(why));
        self.home()?.write(
            WRAPPER_NAME,
            Wrapper::Refuse { why }.script().as_bytes(),
            0o700,
        )
    }

    /// What the agent is told about this conversation's CLI: in its MCP instructions and by the
    /// `omnigraph_cli` tool. The home's config makes the gateway and the graph the default, so
    /// the agent passes neither.
    pub fn instructions(&self, cli_ready: Option<bool>) -> String {
        self.instructions_on(cli_ready, cfg!(windows))
    }

    /// [`CliHome::instructions`] on Windows when `windows`, where the wrapper is a batch file
    /// and the agent is told what that means for its arguments ([`cmd_script`]), and that cmd
    /// cannot work in a network folder, should its working directory be one.
    fn instructions_on(&self, cli_ready: Option<bool>, windows: bool) -> String {
        let command = self.command.to_string_lossy();
        let batch = match (windows, on_network(&self.cwd)) {
            (false, _) => "",
            (true, false) => {
                " It is a batch file, so cmd reads its arguments first: pass a query or \
                 parameters holding \" with %, &, |, <, > or ^ in a file (--query, \
                 --params-file), or use omnigraph_run."
            }
            // cmd starts in the Windows folder instead, and the CLI reads relative paths from
            // there.
            (true, true) => {
                " It is a batch file, so cmd reads its arguments first: pass a query or \
                 parameters holding \" with %, &, |, <, > or ^ in a file (--query, \
                 --params-file), or use omnigraph_run. Your working directory is a network \
                 folder, which cmd cannot work in: give it every file by its absolute path."
            }
        };
        let wrapper_runs = self.wrapper_runs.load(std::sync::atomic::Ordering::Relaxed);
        match cli_ready {
            Some(false) => format!(
                "Studio found no omnigraph CLI that speaks this graph's HTTP API, so the graph is \
                 not available in this conversation. Tell the person: they can install a \
                 matching omnigraph CLI or set `agent.omnigraph_cli`. Your branch would be \
                 {branch}.",
                branch = self.branch
            ),
            _ if !wrapper_runs => format!(
                "Your shell cannot run this conversation's omnigraph CLI: its path is not plain \
                 ASCII, which a batch file cannot hold. Run omnigraph commands with \
                 the omnigraph_run tool: pass the arguments after `omnigraph` as a list, and \
                 Studio adds the gateway and the graph {graph}. Read any branch. Write only to \
                 your branch, {branch}: create it first with [\"branch\", \"create\", \
                 \"{branch}\", \"--from\", \"{base}\"], then pass --branch {branch} to mutate \
                 and load. Prefer one load of NDJSON to many mutations. Studio refuses other \
                 writes before sending them (\"Nothing was sent\"). See AGENTS.md for the rules.",
                graph = self.graph,
                branch = self.branch,
                base = self.base,
            ),
            _ => format!(
                "This conversation's omnigraph CLI is {command} (run it by that path; another \
                 omnigraph on PATH is not connected to Studio).{batch} Its config already points \
                 it at Studio and the graph {graph}: give no --target or --graph. Read any \
                 branch. Write only to your branch, {branch}: create it first with `{command} \
                 branch create {branch} --from {base}`, then pass `--branch {branch}` to mutate \
                 and load. Prefer one load of NDJSON to many \
                 mutations. Studio refuses other writes before sending them (\"Nothing was \
                 sent\"). If your shell has no network (\"server discovery connection \
                 failed\"), run the same commands with the omnigraph_run tool. See AGENTS.md for \
                 the rules.",
                graph = self.graph,
                branch = self.branch,
                base = self.base,
            ),
        }
    }
}

/// Writes `<cwd>/AGENTS.md`, the rules every conversation on the graph shares. Codex reads
/// `AGENTS.md`; Claude reads it when there is no `CLAUDE.md`.
pub fn write_agents_md(cwd: &Path, graph: &str) -> std::io::Result<()> {
    let windows = if cfg!(windows) {
        "- On Windows the CLI is a batch file, `omnigraph.cmd`, and cmd reads its arguments \
         before the CLI does: pass a query or parameters that hold `\"` together with `%`, `&`, \
         `|`, `<`, `>` or `^` in a file (`--query`, `--params-file`), or run the command with \
         the `omnigraph_run` tool, which takes its arguments as a list.\n"
    } else {
        ""
    };
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
         {windows}\
         - Its config already points it at Studio and this graph: give no `--target` or \
         `--graph`.\n\
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
         after `omnigraph` as a list (Studio adds the gateway and the graph), and set \
         `save_to` to write a large result to a file in your working directory, then process \
         that file in your shell. Files it reads or writes must be in your working \
         directory.\n"
    );
    Confined::open(cwd)?.write("AGENTS.md", text.as_bytes(), 0o644)
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
    found
}

/// What a CLI says about itself, read without a server: its release and the HTTP contract it
/// speaks (the `Omnigraph-Http-Api` value, which a server takes only when it is its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliFacts {
    /// Its release, as it reports it ("0.14.0").
    pub version: Option<String>,
    /// `None` for a CLI that did not answer `version --json`: one older than 0.14, or not
    /// OmniGraph's.
    pub contract: Option<String>,
    /// The target protocol it reports (`protocols.target`); `None` for a CLI without targets.
    pub target_protocol: Option<u64>,
}

/// How long a CLI may take to print its version. The tests' stand-ins are shell scripts that a
/// loaded machine can take seconds to start, and the choice, not the limit, is what they check.
const VERSION_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(60)
} else {
    Duration::from_secs(5)
};

/// Reads a CLI's release, contract and target protocol by running it, never a server:
/// `version --json` reports them (`http_contract` in every OmniGraph Studio supports,
/// `protocols.target` in one with targets).
pub async fn cli_facts(cli: &Path) -> CliFacts {
    let json = output(cli, &["version", "--json"])
        .await
        .and_then(|json| serde_json::from_slice::<serde_json::Value>(&json).ok());
    let field = |name: &str| {
        json.as_ref()?
            .get(name)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    CliFacts {
        version: field("version"),
        contract: field("http_contract"),
        target_protocol: json
            .as_ref()
            .and_then(|json| json.pointer("/protocols/target"))
            .and_then(serde_json::Value::as_u64),
    }
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
        /// Whether the agent's shell can run it through the wrapper, which on Windows names
        /// the CLI only by a path a batch file can hold; `omnigraph_run` runs it either way.
        wrapper: bool,
    },
    /// No candidate speaks the graph's contract: each one found, with what it said.
    NoMatch { found: Vec<(PathBuf, CliFacts)> },
}

/// Chooses the conversation's CLI from `candidates`, in their order: the first that speaks
/// `contract`, as the CLI itself reports it, so a slow or unavailable server never decides it.
/// With no match the wrapper runs nothing and says why, rather than pointing at a CLI the server
/// would refuse.
pub async fn choose_cli(home: &CliHome, candidates: Vec<PathBuf>, contract: &str) -> CliChoice {
    // Every candidate is asked at once and heard in order: the first that speaks the contract
    // is chosen as soon as those before it have answered, and the probes after it are dropped,
    // which stops their processes. A hung wrapper later on the path delays nothing.
    let mut probes: futures::stream::FuturesOrdered<_> = candidates
        .iter()
        .cloned()
        .map(|cli| async move { cli_facts(&cli).await })
        .collect();
    let mut found = Vec::new();
    for cli in candidates {
        let Some(facts) = futures::StreamExt::next(&mut probes).await else {
            break;
        };
        if facts.contract.as_deref() == Some(contract)
            && facts.target_protocol == Some(TARGET_PROTOCOL)
            && let Ok(wrapper) = home.use_cli(&cli)
        {
            return CliChoice::Chosen {
                cli,
                version: facts.version,
                wrapper,
            };
        }
        found.push((cli, facts));
    }
    home.refuse(&no_cli_text(contract, &found)).ok();
    CliChoice::NoMatch { found }
}

/// Why a conversation has no CLI: the contract the graph needs, CLI targets, and each CLI found.
pub fn no_cli_text(contract: &str, found: &[(PathBuf, CliFacts)]) -> String {
    let mut text =
        format!("No omnigraph CLI here speaks this graph's HTTP API ({contract}) with CLI targets");
    if found.is_empty() {
        text.push_str(": Studio found none");
    } else {
        let each: Vec<String> = found
            .iter()
            .map(|(cli, facts)| {
                let it = facts.version.as_deref().unwrap_or("a CLI");
                let what = match (&facts.contract, facts.target_protocol) {
                    (None, _) => "older than 0.14, or not OmniGraph's: it did not answer \
                                  `version --json`"
                        .to_owned(),
                    (Some(speaks), _) if speaks != contract => {
                        format!("{it}, which speaks {speaks}")
                    }
                    (Some(_), None) => format!("{it}, which has no CLI targets"),
                    (Some(_), Some(protocol)) => {
                        format!("{it}, which reads target protocol {protocol}")
                    }
                };
                format!("{} is {what}", cli.display())
            })
            .collect();
        text.push_str(": ");
        text.push_str(&each.join("; "));
    }
    text.push_str(&format!(
        ". Install an omnigraph that speaks {contract} with CLI targets (OmniGraph \
         edge-20261020-7fd9cc9 or later), or set `agent.omnigraph_cli` to one."
    ));
    text
}

/// Subcommands `omnigraph_run` passes on: the ones that speak to a server, which the gateway
/// then checks. Storage, cluster, policy and login commands do not run on a server anyway.
const RUN_COMMANDS: &[&str] = &[
    "query", "mutate", "load", "branch", "snapshot", "export", "blob", "commit", "changes",
    "schema", "graphs", "version", "lint",
];
/// What `lint` may take: it checks queries against a schema file offline, never a graph.
const LINT_FLAGS: &[&str] = &["--query", "--schema", "--json"];
/// Flags that would choose another destination or actor than the conversation's. Studio names
/// the gateway itself, so `--target` is refused even naming `studio`.
const RUN_FORBIDDEN: &[&str] = &["--target", "--as"];
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
/// `--target studio` (and the conversation's graph unless one is named) added.
pub fn check_run_args(args: &[String], cwd: &Path, graph: &str) -> Result<RunPlan, String> {
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
    // open storage directly, so only its file flags pass, and no target is added.
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
    let mut files = Vec::new();
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
                 `--target studio` itself."
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
            let file = RunFile {
                parts: inside(cwd, &value)?,
                output: flag == "--out",
            };
            checked.push(flag.to_owned());
            // Filled in with the staged copy's path when the run starts.
            files.push((checked.len(), file));
            checked.push(value);
        } else {
            checked.push(arg.clone());
        }
        index += 1;
    }
    if !lint {
        checked.extend(["--target".to_owned(), "studio".to_owned()]);
        if !has_graph {
            checked.extend(["--graph".to_owned(), graph.to_owned()]);
        }
    }
    Ok(RunPlan {
        args: checked,
        files,
        save_to: None,
    })
}

/// A checked run: the CLI's arguments, its file arguments by position, and where its standard
/// output is saved.
#[derive(Debug, Clone, PartialEq)]
pub struct RunPlan {
    pub args: Vec<String>,
    pub files: Vec<(usize, RunFile)>,
    pub save_to: Option<RunFile>,
}

/// A file in the working directory, by its path's parts there, that a run reads or writes.
///
/// The CLI runs outside the agent's sandbox, so it never opens the working directory's files
/// itself: the agent could change a part of the path into a link between Studio's check and
/// the CLI's use. Studio copies each file it reads into a private folder before the run, by
/// handle and without following a link (`confined`), and writes each output back the same way.
#[derive(Debug, Clone, PartialEq)]
pub struct RunFile {
    pub parts: Vec<String>,
    pub output: bool,
}

impl RunFile {
    fn name(&self) -> &str {
        self.parts.last().map_or("", String::as_str)
    }

    /// The folders it is in, `a/b`, or `None` when it is in the working directory itself.
    fn folder(&self) -> Option<String> {
        (self.parts.len() > 1).then(|| self.parts[..self.parts.len() - 1].join("/"))
    }

    fn shown(&self) -> String {
        self.parts.join("/")
    }

    /// Copies the file from `cwd` to `to`.
    fn copy_in(&self, cwd: &Path, to: &Path) -> std::io::Result<()> {
        let root = Confined::open(cwd)?;
        let mut file = match self.folder() {
            Some(folder) => root.existing_dirs(&folder)?.file(self.name())?,
            None => root.file(self.name())?,
        };
        std::io::copy(&mut file, &mut std::fs::File::create(to)?)?;
        Ok(())
    }

    /// Writes what `fill` writes as the file in `cwd`, its folders made as needed.
    fn write(
        &self,
        cwd: &Path,
        fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let root = Confined::open(cwd)?;
        match self.folder() {
            Some(folder) => root.dirs(&folder)?.write_with(self.name(), 0o644, fill),
            None => root.write_with(self.name(), 0o644, fill),
        }
    }
}

/// `value`, a path the agent gave, as its parts inside `cwd`: relative to it, or absolute and
/// under it. Decided from the text alone, since Studio later opens each part by handle; `..`
/// is refused.
pub fn inside(cwd: &Path, value: &str) -> Result<Vec<String>, String> {
    let outside = || {
        format!(
            "{value}: omnigraph_run reads and writes files only in your working directory, {}.",
            cwd.display()
        )
    };
    let path = Path::new(value);
    let relative = if path.is_absolute() {
        let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
        path.strip_prefix(cwd)
            .or_else(|_| path.strip_prefix(&canonical))
            .map_err(|_| outside())?
    } else {
        path
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| format!("{value}: the path is not text"))?
                    .to_owned(),
            ),
            std::path::Component::CurDir => {}
            _ => return Err(outside()),
        }
    }
    if parts.is_empty() {
        return Err(format!("{value}: name a file"));
    }
    if parts[0] == ".studio" {
        return Err(format!(
            "{value}: the .studio folder holds Studio's own files for the conversation."
        ));
    }
    Ok(parts)
}

/// Reads `from` into `into` until its end, or until it holds more than `cap` bytes: whether it
/// went past. Large reads into one buffer: a byte stream read in small steps is slow.
async fn read_capped(
    mut from: impl futures::AsyncRead + Unpin,
    into: &mut Vec<u8>,
    cap: usize,
) -> bool {
    use futures::AsyncReadExt as _;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        match from.read(&mut buffer).await {
            Ok(0) | Err(_) => return false,
            Ok(read) => {
                into.extend_from_slice(&buffer[..read]);
                if into.len() > cap {
                    return true;
                }
            }
        }
    }
}

/// A run's private folder, in the conversation's private home, removed when the run ends. Not
/// in the temporary directory: a sandboxed agent may write there.
struct Stage(PathBuf);

impl Stage {
    fn new(home: &Path) -> std::io::Result<Self> {
        let tag = crate::confined::random_hex(8)?;
        let dir = home.join(format!("run-{tag}"));
        #[cfg(unix)]
        let builder = {
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
        };
        // Windows has no mode bits: the folder takes the private home's access.
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        builder.create(&dir)?;
        Ok(Self(dir))
    }

    /// Where the run's `index`th argument's file is staged; its name keeps the extension the
    /// CLI may read the format from.
    fn path(&self, index: usize, file: &RunFile) -> PathBuf {
        self.0.join(format!("{index}-{}", file.name()))
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Runs this conversation's CLI with a checked plan, with no shell. Returns the tool's text and
/// whether the command failed. The plan's files are staged (see [`RunFile`]); its `save_to`
/// writes standard output to a file in `cwd` instead of returning it.
pub async fn run(home: &CliHome, cwd: &Path, plan: RunPlan) -> (String, bool) {
    let cli = match home.cli() {
        Ok(cli) => cli,
        Err(why) => return (why, true),
    };
    let stage = match Stage::new(&home.private) {
        Ok(stage) => stage,
        Err(error) => {
            return (
                format!("Studio could not stage the run's files: {error}"),
                true,
            );
        }
    };
    let mut args = plan.args;
    for (index, file) in &plan.files {
        let staged = stage.path(*index, file);
        if !file.output
            && let Err(error) = file.copy_in(cwd, &staged)
        {
            return (
                format!("Studio could not read {}: {error}", file.shown()),
                true,
            );
        }
        args[*index] = staged.to_string_lossy().into_owned();
    }
    let mut command = home.command(&cli, &stage.0);
    command.args(&args);
    let child = smol::process::Command::from(command)
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
        // Past the cap a stream is no longer read, and a CLI writing more would wait on the
        // full pipe until the timeout: the first stream past it stops the command, without
        // waiting for the other to close.
        let cut = {
            let out_read = read_capped(stdout, &mut out, RUN_OUTPUT_MAX).fuse();
            let err_read = read_capped(stderr, &mut err, RUN_OUTPUT_MAX).fuse();
            futures::pin_mut!(out_read, err_read);
            let mut cut = false;
            loop {
                futures::select! {
                    past = out_read => cut |= past,
                    past = err_read => cut |= past,
                    complete => break,
                }
                if cut {
                    break;
                }
            }
            cut
        };
        if cut {
            child.kill().ok();
        }
        let status = child.status().await;
        (out, err, status, cut)
    }
    .fuse();
    let timeout = smol::Timer::after(RUN_TIMEOUT).fuse();
    futures::pin_mut!(read, timeout);
    let (out, err, status, cut) = futures::select! {
        done = read => done,
        _ = timeout => return ("The command took longer than 30 minutes and was stopped.".into(), true),
    };
    if cut {
        return (
            format!(
                "The command wrote more than {} MiB and was stopped; nothing was saved. Ask for \
                 less (fewer rows or columns), or write a file with the command's own flag \
                 (--out).",
                RUN_OUTPUT_MAX / (1024 * 1024)
            ),
            true,
        );
    }
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
    // The files the CLI wrote, back into the working directory.
    if !failed {
        for (index, file) in plan.files.iter().filter(|(_, file)| file.output) {
            let staged = stage.path(*index, file);
            let Ok(mut written) = std::fs::File::open(&staged) else {
                continue;
            };
            if let Err(error) = file.write(cwd, |to| std::io::copy(&mut written, to).map(|_| ())) {
                return (
                    format!("Studio could not save {}: {error}", file.shown()),
                    true,
                );
            }
        }
    }
    match &plan.save_to {
        Some(file) if !failed => {
            let saved = &out[..out.len().min(RUN_OUTPUT_MAX)];
            match file.write(cwd, |to| to.write_all(saved)) {
                Ok(()) => text.push_str(&format!(
                    "output saved to {} ({} bytes)\n",
                    file.shown(),
                    saved.len()
                )),
                Err(error) => return (format!("Studio could not save the output: {error}"), true),
            }
        }
        _ => text.push_str(&clip(&out)),
    }
    if !err.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&clip(&err));
    }
    (text, failed)
}

/// The CLI's file name: `omnigraph`, or `omnigraph.exe` on Windows.
const CLI_NAME: &str = if cfg!(windows) {
    "omnigraph.exe"
} else {
    "omnigraph"
};

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// On Windows a program is a file; its name says what runs it.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// `path` with a leading `~/` (or `~\` on Windows) as the person's home folder.
pub fn expand_home(path: &str) -> PathBuf {
    let rest = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("~\\").filter(|_| cfg!(windows)));
    match (rest, std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn yaml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// The wrapper's file name in the conversation's home: the name the agent runs it by.
const WRAPPER_NAME: &str = if cfg!(windows) {
    "omnigraph.cmd"
} else {
    "omnigraph"
};

/// Variables the wrapper removes from the CLI's environment by the start of their name: tokens
/// kept for other targets (`OMNIGRAPH_TOKEN_<NAME>`), which the CLI would send to them.
const WRAPPER_UNSET_PREFIXES: &[&str] = &["OMNIGRAPH_TOKEN_"];

/// Variables the wrapper removes by name: a token the CLI's older releases read, and its profile
/// and control-plane addressing, which would choose another destination.
const WRAPPER_UNSET: &[&str] = &[
    "OMNIGRAPH_BEARER_TOKEN",
    "OMNIGRAPH_PROFILE",
    "OMNIGRAPH_CONTROL_TOKEN",
    "OMNIGRAPH_CONTROL_API",
];

/// The hosts put first in the CLI's `NO_PROXY`, so it reaches the gateway outside any proxy.
const LOOPBACK: &str = "127.0.0.1,localhost";

/// What the agent's `omnigraph` wrapper does, the one account both scripts are written from
/// ([`posix_script`], [`cmd_script`]), so only their syntax differs. Run, it removes the
/// variables of [`WRAPPER_UNSET_PREFIXES`] and [`WRAPPER_UNSET`] from the CLI's environment,
/// sets `OMNIGRAPH_HOME` to `home`, its own folder, puts [`LOOPBACK`] first in `NO_PROXY` (and
/// `no_proxy`), and runs `cli` with the agent's arguments as they came. Refusing, it runs
/// nothing and says `why`.
enum Wrapper<'a> {
    Run { home: &'a Path, cli: &'a Path },
    Refuse { why: &'a str },
}

impl Wrapper<'_> {
    /// The wrapper as this platform runs it.
    fn script(&self) -> String {
        self.script_on(cfg!(windows))
    }

    /// The batch file when `windows`, else the shell script.
    fn script_on(&self, windows: bool) -> String {
        if windows {
            cmd_script(self)
        } else {
            posix_script(self)
        }
    }
}

/// The wrapper as a `/bin/sh` script. Every path is in single quotes, and the agent's
/// arguments pass on as `"$@"`, which the shell hands over as they came.
fn posix_script(wrapper: &Wrapper<'_>) -> String {
    let (home, cli) = match wrapper {
        Wrapper::Run { home, cli } => (home, cli),
        Wrapper::Refuse { why } => {
            return format!(
                "#!/bin/sh\n\
                 # Written by OmniGraph Studio for one agent conversation (spec 013).\n\
                 echo {why} >&2\n\
                 exit 1\n",
                why = shell_quote(why),
            );
        }
    };
    let mut script = String::from(
        "#!/bin/sh\n\
         # Written by OmniGraph Studio for one agent conversation (spec 013): the omnigraph\n\
         # CLI, pointed at Studio's gateway for this conversation.\n",
    );
    for prefix in WRAPPER_UNSET_PREFIXES {
        script.push_str(&format!(
            "for name in $(env | sed -n 's/^\\({prefix}[A-Za-z0-9_]*\\)=.*/\\1/p'); do unset \
             \"$name\"; done\n"
        ));
    }
    script.push_str(&format!(
        "unset {unset}\n\
         OMNIGRAPH_HOME={home}\n\
         NO_PROXY=\"{LOOPBACK}${{NO_PROXY:+,$NO_PROXY}}\"\n\
         no_proxy=\"$NO_PROXY\"\n\
         export OMNIGRAPH_HOME NO_PROXY no_proxy\n\
         exec {cli} \"$@\"\n",
        unset = WRAPPER_UNSET.join(" "),
        home = shell_quote(&home.to_string_lossy()),
        cli = shell_quote(&cli.to_string_lossy()),
    ));
    script
}

/// The wrapper as a Windows batch file, `omnigraph.cmd`, which a shell runs by its path as it
/// runs `omnigraph` (cmd and PowerShell find the `.cmd` by PATHEXT; Git Bash, where Claude Code
/// runs its commands, is given the whole name).
///
/// A batch file, not a launcher of Studio's own: Studio's executable opens its window when run
/// as `omnigraph`, is large to copy into each home (the debug build is 490 MB), and a hard link
/// to it would let the agent write Studio's own executable. The cost is cmd's reading of the
/// arguments. Windows starts a batch file through `cmd /c` with the caller's command line, and
/// cmd reads that line before the batch file runs: it expands `%NAME%`, and outside double
/// quotes takes `&`, `|`, `<` and `>` as its own and drops `^`. A caller that escapes for cmd,
/// as Rust's standard library does, gets every argument through as given. A shell that quotes
/// only for programs (Git Bash, PowerShell) leaves text between two quotes inside one argument
/// outside cmd's quotes, where those characters act. That is true of every `.cmd`, npm's own
/// shims included; the agent's instructions say so, and `omnigraph_run` takes arguments as a
/// list.
///
/// The batch file adds no reading of its own beyond that one. `%*` is the arguments as cmd
/// read them and is expanded once, never again: its quotes stand where they stood, so what was
/// quoted stays quoted, and a `%` in it stays. (A `^` the caller wrote before one of `&|<>`
/// outside quotes is read twice, as in any batch file that passes `%*` on.) Delayed expansion
/// is off, so `!` is plain. Each path Studio writes is in double quotes, which a Windows path
/// cannot hold, with its `%` doubled: inside quotes `&`, `|`, `<`, `>`, `^`, `(` and `)` are
/// plain. Lines end with CRLF, as cmd reads them.
///
/// cmd reads a batch file's text in the console's code page, so the file is ASCII only. Its
/// folder, the home, may hold any character: cmd knows it as `%~dp0`, from its own Unicode
/// path. The CLI is named by its path when that is ASCII, else from `%~dp0` when that path is
/// (as when the CLI and the home are both under the person's profile, whose name is not);
/// otherwise the wrapper refuses and points at `omnigraph_run`, which starts the CLI by its
/// path without cmd.
fn cmd_script(wrapper: &Wrapper<'_>) -> String {
    let (home, cli) = match wrapper {
        Wrapper::Run { home, cli } => (home, cli),
        Wrapper::Refuse { why } => return cmd_refusal(why),
    };
    let Some(cli_named) = cmd_program(home, cli) else {
        return cmd_refusal(&format!(
            "The omnigraph CLI Studio chose for this conversation, {}, is in a folder whose \
             name a batch file cannot hold. Run the same commands with the omnigraph_run tool, \
             or set agent.omnigraph_cli to a CLI whose path is in plain ASCII.",
            cli.display()
        ));
    };
    let mut lines = vec![
        "@echo off".to_owned(),
        "rem Written by OmniGraph Studio for one agent conversation (spec 013): the omnigraph"
            .to_owned(),
        "rem CLI, pointed at Studio's gateway for this conversation.".to_owned(),
        "setlocal EnableExtensions DisableDelayedExpansion".to_owned(),
    ];
    // `set NAME` lists every variable whose name starts with NAME, in any case, as Windows
    // reads names.
    for prefix in WRAPPER_UNSET_PREFIXES {
        lines.push(format!(
            "for /f \"delims==\" %%N in ('set {prefix} 2^>nul') do set \"%%N=\""
        ));
    }
    for name in WRAPPER_UNSET {
        lines.push(format!("set \"{name}=\""));
    }
    lines.extend([
        // The batch file's own folder, without its last `\`.
        "set \"OMNIGRAPH_HOME=%~dp0\"".to_owned(),
        "set \"OMNIGRAPH_HOME=%OMNIGRAPH_HOME:~0,-1%\"".to_owned(),
        // `NO_PROXY` and `no_proxy` are one variable to Windows.
        format!(
            "if defined NO_PROXY (set \"NO_PROXY={LOOPBACK},%NO_PROXY%\") else set \
             \"NO_PROXY={LOOPBACK}\""
        ),
        format!("{cli_named} %*"),
    ]);
    lines.iter().map(|line| format!("{line}\r\n")).collect()
}

/// How the batch file in `home` names `cli`: its path in double quotes, or the same from the
/// batch file's own folder (`%~dp0`), whichever is ASCII first; `None` when neither is. Both are
/// read as Windows paths, whatever platform reads them.
fn cmd_program(home: &Path, cli: &Path) -> Option<String> {
    let quoted = |text: &str| format!("\"{}\"", text.replace('%', "%%"));
    let ascii = |text: &str| text.chars().all(|c| c.is_ascii() && !c.is_ascii_control());
    let cli = cli.to_str()?;
    if ascii(cli) {
        return Some(quoted(cli));
    }
    // Windows matches names in any case; the walk up ends where the two paths part.
    let parts = |path: &str| -> Vec<String> {
        path.split(['\\', '/'])
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let (home, cli) = (parts(home.to_str()?), parts(cli));
    let shared = home
        .iter()
        .zip(&cli)
        .take_while(|(a, b)| a.to_lowercase() == b.to_lowercase())
        .count();
    // Not even the drive in common.
    if shared == 0 {
        return None;
    }
    let mut relative: Vec<&str> = vec![".."; home.len() - shared];
    relative.extend(cli[shared..].iter().map(String::as_str));
    let relative = relative.join("\\");
    ascii(&relative).then(|| format!("\"%~dp0{}\"", relative.replace('%', "%%")))
}

/// Whether `path`, read as a Windows path whatever platform reads it, is a network path
/// (`\\server\share\…`, or `\\?\UNC\…`). cmd cannot work in one: started there, it works in the
/// Windows folder instead, so a program it runs reads relative paths from there.
fn on_network(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('/', "\\");
    text.starts_with(r"\\?\UNC\")
        || (text.starts_with(r"\\") && !text.starts_with(r"\\?\") && !text.starts_with(r"\\.\"))
}

/// A batch file that runs nothing and writes `why` to standard error, as plain ASCII: a
/// character outside it shows as `?`, a double quote as `'`, and every character cmd would
/// read (`%`, `^`, `&`, `|`, `<`, `>`, `(`, `)`) is escaped. Delayed expansion is off, as in
/// the wrapper that runs the CLI, so a `!` stays (a person's registry can turn it on for every
/// batch file). A long text is split over lines short of cmd's limit.
fn cmd_refusal(why: &str) -> String {
    let plain: String = why
        .chars()
        .map(|c| match c {
            '"' => '\'',
            c if c.is_ascii() && !c.is_ascii_control() => c,
            _ => '?',
        })
        .collect();
    let mut lines = vec![
        "@echo off".to_owned(),
        "rem Written by OmniGraph Studio for one agent conversation (spec 013).".to_owned(),
        "setlocal EnableExtensions DisableDelayedExpansion".to_owned(),
    ];
    for chunk in plain.as_bytes().chunks(1000) {
        let mut escaped = String::new();
        for &byte in chunk {
            match byte {
                b'%' => escaped.push_str("%%"),
                b'^' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' => {
                    escaped.push('^');
                    escaped.push(char::from(byte));
                }
                _ => escaped.push(char::from(byte)),
            }
        }
        // `echo(` prints any text, an empty one, `on`, `off` or `/?` included.
        lines.push(format!("1>&2 echo({escaped}"));
    }
    lines.push("exit /b 1".to_owned());
    lines.iter().map(|line| format!("{line}\r\n")).collect()
}

// Most tests run fake CLIs, which are shell scripts: those run on Unix.
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    /// The first candidate that speaks the graph's contract is chosen, as the CLI reports it and
    /// with no server behind the gateway; with none, the wrapper runs nothing and names each CLI
    /// found.
    #[cfg(unix)]
    #[test]
    fn the_cli_that_speaks_the_contract_is_chosen_without_a_server() {
        let dir = std::env::temp_dir().join(format!("studio-cli-choice-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let fake = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // 0.13.0 predates `version --json`; every other command fails, as one would with no
        // server behind the gateway.
        let old = fake(
            "omnigraph-0.13",
            r#"[ "$1" = --version ] && { echo "omnigraph 0.13.0"; exit 0; }; exit 2"#,
        );
        // Speaks the contract but has no targets, so it would refuse the home's config.
        let untargeted = fake(
            "omnigraph-0.14",
            r#"[ "$1 $2" = "version --json" ] && { echo '{"version":"0.14.0","http_contract":"0.14"}'; exit 0; }; exit 2"#,
        );
        let current = fake(
            "omnigraph-targets",
            r#"[ "$1 $2" = "version --json" ] && { echo '{"version":"0.14.0-edge+7fd9cc9","http_contract":"0.14","protocols":{"target":1,"credential":1}}'; exit 0; }; exit 2"#,
        );
        // A wrapper that hangs: a match before it is chosen without waiting for it.
        let hung = fake("omnigraph-hung", "sleep 30");
        let other = fake(
            "omnigraph-0.15",
            r#"[ "$1 $2" = "version --json" ] && { echo '{"version":"0.15.0","http_contract":"0.15","protocols":{"target":1}}'; exit 0; }; exit 2"#,
        );
        let home = CliHome::write(
            &dir,
            &dir.join("private"),
            1,
            "t",
            "knowledge",
            "agent-x",
            "main",
        )
        .unwrap();
        let wrapper = || std::fs::read_to_string(&home.command).unwrap();
        let choose = |candidates: Vec<PathBuf>| {
            smol::block_on(choose_cli(&home, candidates, graph_client::CONTRACT))
        };

        match choose(vec![old.clone(), untargeted.clone(), current.clone()]) {
            CliChoice::Chosen {
                cli,
                version,
                wrapper,
            } => {
                assert_eq!(cli, current);
                assert_eq!(version.as_deref(), Some("0.14.0-edge+7fd9cc9"));
                assert!(wrapper, "a shell script names any path");
            }
            other => panic!("{other:?}"),
        }
        assert!(wrapper().contains(&*current.to_string_lossy()));
        // Both homes name the gateway as the target `studio`, the default.
        for path in [
            home.dir.join("config.yaml"),
            home.private.join("config.yaml"),
        ] {
            let config = std::fs::read_to_string(path).unwrap();
            assert!(config.contains("target_config_version: 1\n"), "{config}");
            assert!(
                config.contains(
                    "studio: { kind: server, endpoint: \"http://127.0.0.1:1\", credential: studio, default_graph: \"knowledge\" }"
                ),
                "{config}"
            );
            assert!(config.contains("defaults:\n  target: studio\n"), "{config}");
        }
        // Studio's own reading of the CLI's files sends the token to the gateway: the endpoint
        // it is saved for covers the target's.
        let files = graph_store::cli_config::read_operator(Some(&home.private));
        assert!(files.errors.is_empty(), "{:?}", files.errors);
        let servers = files.servers.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(
            (servers[0].0.name.as_str(), servers[0].1.as_deref()),
            ("studio", Some("t"))
        );

        match choose(vec![other.clone(), current.clone()]) {
            CliChoice::Chosen { cli, .. } => assert_eq!(cli, current),
            other => panic!("{other:?}"),
        }

        let started = std::time::Instant::now();
        match choose(vec![current.clone(), hung.clone()]) {
            CliChoice::Chosen { cli, .. } => assert_eq!(cli, current),
            other => panic!("{other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "waited for the hung CLI after the match: {:?}",
            started.elapsed()
        );

        match choose(vec![old.clone(), untargeted.clone(), other.clone()]) {
            CliChoice::NoMatch { found } => {
                let text = no_cli_text(graph_client::CONTRACT, &found);
                assert!(text.contains("older than 0.14"), "{text}");
                assert!(text.contains("0.14.0, which has no CLI targets"), "{text}");
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
    #[cfg(unix)]
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
        let file = |parts: &[&str], output: bool| RunFile {
            parts: parts.iter().map(|part| (*part).to_owned()).collect(),
            output,
        };

        assert_eq!(
            check(&["query", "-e", "query q() { … }", "--json"])
                .unwrap()
                .args,
            args(&[
                "query",
                "-e",
                "query q() { … }",
                "--json",
                "--target",
                "studio",
                "--graph",
                "knowledge"
            ])
        );
        // A graph the agent names stays; a file is noted by its place in the working
        // directory, to be staged when the run starts.
        let load = check(&[
            "load",
            "--data",
            "data/rows.jsonl",
            "--mode",
            "append",
            "--graph",
            "notes",
        ])
        .unwrap();
        assert_eq!(
            load.args,
            args(&[
                "load",
                "--data",
                "data/rows.jsonl",
                "--mode",
                "append",
                "--graph",
                "notes",
                "--target",
                "studio",
            ])
        );
        assert_eq!(load.files, vec![(2, file(&["data", "rows.jsonl"], false))]);
        // An absolute path inside the working directory is the same file.
        let absolute = cwd.join("data/rows.jsonl");
        let absolute = check(&["load", "--data", &absolute.to_string_lossy()]).unwrap();
        assert_eq!(absolute.files, load.files);
        // `lint` checks against a saved schema file, offline: no target is added.
        let lint = check(&["lint", "--query", "q.gq", "--schema=schema.pg", "--json"]).unwrap();
        assert_eq!(
            lint.args,
            args(&["lint", "--query", "q.gq", "--schema", "schema.pg", "--json"])
        );
        assert_eq!(
            lint.files,
            vec![
                (2, file(&["q.gq"], false)),
                (4, file(&["schema.pg"], false))
            ]
        );
        let out = check(&["blob", "get", "--out=data/picture.png"]).unwrap();
        assert_eq!(out.args[2], "--out");
        assert_eq!(out.files, vec![(3, file(&["data", "picture.png"], true))]);

        for refused in [
            &["init", "--store", "x"][..],
            &["omnigraph", "query"],
            &["schema", "apply"],
            &["query", "--target", "http://elsewhere"],
            &["query", "--target=file:///tmp/g"],
            &["query", "--target=studio"],
            &["mutate", "--as", "someone"],
            &["load", "--data", "/etc/passwd"],
            &["load", "--data", "../outside.jsonl"],
            &["load", "--data", "data/../../outside.jsonl"],
            &["blob", "get", "--out", "."],
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

    /// The agent can change a part of a path into a link after the check: Studio stages each
    /// file by handle, so a link is refused on the way in and replaced on the way out.
    #[cfg(unix)]
    #[test]
    fn a_runs_files_are_staged_without_following_links() {
        let cwd = std::env::temp_dir().join(format!("studio-run-stage-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("studio-run-outside-{}", std::process::id()));
        for dir in [&cwd, &outside] {
            std::fs::remove_dir_all(dir).ok();
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(outside.join("secret"), "private").unwrap();
        std::fs::write(cwd.join("rows.jsonl"), "{}\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), cwd.join("escape")).unwrap();
        std::os::unix::fs::symlink(&outside, cwd.join("linked")).unwrap();
        let stage = Stage::new(&cwd).unwrap();
        let input = |parts: &[&str]| RunFile {
            parts: parts.iter().map(|part| (*part).to_owned()).collect(),
            output: false,
        };

        let rows = input(&["rows.jsonl"]);
        rows.copy_in(&cwd, &stage.path(0, &rows)).unwrap();
        assert_eq!(std::fs::read(stage.path(0, &rows)).unwrap(), b"{}\n");
        for link in [input(&["escape"]), input(&["linked", "secret"])] {
            assert!(
                link.copy_in(&cwd, &stage.path(1, &link)).is_err(),
                "{link:?}"
            );
        }

        // An output written through a link replaces the link; its target is left alone.
        let out = RunFile {
            parts: vec!["escape".into()],
            output: true,
        };
        out.write(&cwd, |file| file.write_all(b"result")).unwrap();
        assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"private");
        assert_eq!(std::fs::read(cwd.join("escape")).unwrap(), b"result");
        let through = RunFile {
            parts: vec!["linked".into(), "out.json".into()],
            output: true,
        };
        assert!(through.write(&cwd, |file| file.write_all(b"x")).is_err());
        assert!(!outside.join("out.json").exists());

        let path = stage.0.clone();
        drop(stage);
        assert!(!path.exists(), "the stage is removed with the run");
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// The agent can rewrite its wrapper and its home's config, but `omnigraph_run` runs outside
    /// its sandbox: it starts the chosen CLI itself, with the private home, in a private folder,
    /// and without Studio's own OmniGraph variables. Ending the conversation leaves a later one's
    /// credentials in the same folder alone.
    #[cfg(unix)]
    #[test]
    fn a_run_starts_the_chosen_cli_with_its_private_home() {
        let cwd = std::env::temp_dir().join(format!("studio-run-private-{}", std::process::id()));
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = std::fs::canonicalize(&cwd).unwrap();
        let private = cwd
            .parent()
            .unwrap()
            .join(format!("studio-run-homes-{}", std::process::id()));
        let home = CliHome::write(&cwd, &private, 1, "t", "knowledge", "agent/x", "main").unwrap();
        let plan = || RunPlan {
            args: vec!["version".into()],
            files: Vec::new(),
            save_to: None,
        };
        let (text, failed) = smol::block_on(run(&home, &cwd, plan()));
        assert!(failed && text.contains("not chosen"), "{text}");
        home.refuse("No omnigraph CLI here speaks 0.14").unwrap();
        let (text, failed) = smol::block_on(run(&home, &cwd, plan()));
        assert!(
            failed && text == "No omnigraph CLI here speaks 0.14",
            "{text}"
        );

        let cli = cwd.join("fake-cli");
        std::fs::write(&cli, "#!/bin/sh\npwd\nenv\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        home.use_cli(&cli).unwrap();
        // What the agent may do in its own folder.
        std::fs::write(&home.command, "#!/bin/sh\necho rewritten\n").unwrap();
        std::fs::write(home.dir.join("config.yaml"), "targets: {}\n").unwrap();

        let (text, failed) = smol::block_on(run(&home, &cwd, plan()));
        assert!(!failed, "{text}");
        assert!(!text.contains("rewritten"), "{text}");
        assert!(
            text.contains(&format!("OMNIGRAPH_HOME={}", home.private.display())),
            "{text}"
        );
        assert!(!home.private.starts_with(&cwd));
        let pwd = text.lines().nth(1).unwrap();
        assert!(
            Path::new(pwd).starts_with(std::fs::canonicalize(&home.private).unwrap()),
            "{text}"
        );
        for line in text.lines().filter(|line| line.starts_with("OMNIGRAPH_")) {
            assert!(line.starts_with("OMNIGRAPH_HOME="), "{line}");
        }
        assert!(
            std::fs::read_to_string(home.private.join("config.yaml"))
                .unwrap()
                .contains("studio: { kind: server, endpoint: \"http://127.0.0.1:1\"")
        );
        assert!(
            check_run_args(
                &[
                    "load".into(),
                    "--data".into(),
                    ".studio/agent-x/config.yaml".into()
                ],
                &cwd,
                "knowledge"
            )
            .is_err()
        );

        // A conversation started again on the branch writes its own token before this one
        // ends: the new token stays.
        let again = CliHome::write(&cwd, &private, 2, "u", "knowledge", "agent/x", "main").unwrap();
        let held = home.private.clone();
        drop(home);
        assert!(!held.exists());
        assert_eq!(
            std::fs::read_to_string(again.dir.join("credentials")).unwrap(),
            "[studio]\ntoken = u\nendpoint = http://127.0.0.1:2\n"
        );
        drop(again);
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::remove_dir_all(&private).ok();
    }

    /// A command that writes past the output cap is stopped, not left blocked on its pipe
    /// until the 30-minute timeout.
    #[cfg(unix)]
    #[test]
    fn a_run_that_writes_too_much_is_stopped() {
        let cwd = std::env::temp_dir().join(format!("studio-run-cap-{}", std::process::id()));
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::create_dir_all(&cwd).unwrap();
        let home = CliHome::write(
            &cwd,
            &cwd.join("private"),
            1,
            "t",
            "knowledge",
            "agent/x",
            "main",
        )
        .unwrap();
        let cli = cwd.join("loud-cli");
        let bytes = RUN_OUTPUT_MAX + 4 * 1024 * 1024;
        std::fs::write(
            &cli,
            format!("#!/bin/sh\nhead -c {bytes} /dev/zero\nsleep 600\n"),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        home.use_cli(&cli).unwrap();
        let plan = RunPlan {
            args: vec!["export".into()],
            files: Vec::new(),
            save_to: None,
        };
        let started = std::time::Instant::now();
        let (text, failed) = smol::block_on(run(&home, &cwd, plan));
        assert!(failed && text.contains("was stopped"), "{text}");
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
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

    #[cfg(unix)]
    #[test]
    fn a_conversation_home_holds_its_config_token_and_wrapper() {
        let cwd = std::env::temp_dir().join(format!("studio-cli-home-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let home = CliHome::write(
            &cwd,
            &cwd.join("private"),
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
            config.contains(
                "studio: { kind: server, endpoint: \"http://127.0.0.1:49152\", credential: studio, default_graph: \"knowledge\" }"
            ),
            "{config}"
        );
        assert!(config.contains("defaults:\n  target: studio\n"), "{config}");
        assert_eq!(
            std::fs::read_to_string(home.dir.join("credentials")).unwrap(),
            "[studio]\ntoken = abc123\nendpoint = http://127.0.0.1:49152\n"
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
        // Windows's batch file adds a note on its arguments, and its name is longer; a working
        // directory in a network folder adds another, and a CLI the batch file cannot name
        // turns the agent to omnigraph_run.
        let long = cwd
            .join("d".repeat(120))
            .join(".studio/agent-20261013-101500");
        let network =
            PathBuf::from(r"\\server\share\ada\AppData\Roaming\OmniGraph Studio\agent\graphs\g");
        for (windows, wrapper, at, runs) in [
            (false, "omnigraph", &cwd, true),
            (true, "omnigraph.cmd", &cwd, true),
            (true, "omnigraph.cmd", &network, true),
            (true, "omnigraph.cmd", &cwd, false),
        ] {
            let longest = CliHome {
                command: long.join(wrapper),
                dir: long.clone(),
                graph: "g".repeat(64),
                branch: "agent/20261013-101500".into(),
                base: "b".repeat(64),
                cwd: at.clone(),
                folder: ".studio/agent-20261013-101500".into(),
                credentials: String::new(),
                private: cwd.join("private/longest"),
                cli: std::sync::Mutex::new(Err(String::new())),
                wrapper_runs: std::sync::atomic::AtomicBool::new(runs),
            };
            for ready in [None, Some(true), Some(false)] {
                let instructions = longest.instructions_on(ready, windows);
                let length = crate::mcp::INSTRUCTIONS.len() + 2 + instructions.len();
                assert!(length <= 2048, "{length} characters (Windows: {windows})");
                let told = ready != Some(false);
                assert_eq!(
                    instructions.contains("batch file"),
                    windows && told,
                    "{instructions}"
                );
                assert_eq!(
                    instructions.contains("network folder"),
                    *at == network && runs && told,
                    "{instructions}"
                );
                assert_eq!(
                    instructions.contains(&*long.join(wrapper).to_string_lossy()),
                    runs && told,
                    "{instructions}"
                );
            }
        }

        let credentials = home.dir.join("credentials");
        drop(home);
        assert!(
            !credentials.exists(),
            "the token goes with the conversation"
        );
        write_agents_md(&cwd, "knowledge").unwrap();
        let rules = std::fs::read_to_string(cwd.join("AGENTS.md")).unwrap();
        assert!(rules.contains("give no `--target` or `--graph`"), "{rules}");
        std::fs::remove_dir_all(&cwd).ok();
    }

    /// The shell script and the batch file are written from one account of the wrapper: the
    /// same variables removed, its own folder as the home, loopback first in `NO_PROXY`, and the
    /// CLI run with the agent's arguments as they came. Only the syntax differs.
    #[test]
    fn both_wrappers_are_written_from_one_account() {
        let posix = posix_script(&Wrapper::Run {
            home: Path::new("/data/agent/graphs/g/.studio/agent-x"),
            cli: Path::new("/opt/it's here/omnigraph"),
        });
        assert_eq!(
            posix,
            "#!/bin/sh\n\
             # Written by OmniGraph Studio for one agent conversation (spec 013): the omnigraph\n\
             # CLI, pointed at Studio's gateway for this conversation.\n\
             for name in $(env | sed -n 's/^\\(OMNIGRAPH_TOKEN_[A-Za-z0-9_]*\\)=.*/\\1/p'); do unset \"$name\"; done\n\
             unset OMNIGRAPH_BEARER_TOKEN OMNIGRAPH_PROFILE OMNIGRAPH_CONTROL_TOKEN OMNIGRAPH_CONTROL_API\n\
             OMNIGRAPH_HOME='/data/agent/graphs/g/.studio/agent-x'\n\
             NO_PROXY=\"127.0.0.1,localhost${NO_PROXY:+,$NO_PROXY}\"\n\
             no_proxy=\"$NO_PROXY\"\n\
             export OMNIGRAPH_HOME NO_PROXY no_proxy\n\
             exec '/opt/it'\\''s here/omnigraph' \"$@\"\n"
        );
        let cmd = cmd_script(&Wrapper::Run {
            home: Path::new(
                r"C:\Users\ada\AppData\Roaming\OmniGraph Studio\agent\graphs\g\.studio\agent-x",
            ),
            cli: Path::new(r"C:\Users\ada\.local\bin\omnigraph.exe"),
        });
        let lines = [
            "@echo off",
            "rem Written by OmniGraph Studio for one agent conversation (spec 013): the omnigraph",
            "rem CLI, pointed at Studio's gateway for this conversation.",
            "setlocal EnableExtensions DisableDelayedExpansion",
            "for /f \"delims==\" %%N in ('set OMNIGRAPH_TOKEN_ 2^>nul') do set \"%%N=\"",
            "set \"OMNIGRAPH_BEARER_TOKEN=\"",
            "set \"OMNIGRAPH_PROFILE=\"",
            "set \"OMNIGRAPH_CONTROL_TOKEN=\"",
            "set \"OMNIGRAPH_CONTROL_API=\"",
            "set \"OMNIGRAPH_HOME=%~dp0\"",
            "set \"OMNIGRAPH_HOME=%OMNIGRAPH_HOME:~0,-1%\"",
            "if defined NO_PROXY (set \"NO_PROXY=127.0.0.1,localhost,%NO_PROXY%\") else set \
             \"NO_PROXY=127.0.0.1,localhost\"",
            "\"C:\\Users\\ada\\.local\\bin\\omnigraph.exe\" %*",
        ];
        assert_eq!(cmd, lines.map(|line| format!("{line}\r\n")).concat());
        for name in WRAPPER_UNSET_PREFIXES.iter().chain(WRAPPER_UNSET) {
            assert!(posix.contains(name) && cmd.contains(name), "{name}");
        }
    }

    /// The batch file quotes every path it names, doubling `%`, the one character cmd reads
    /// inside double quotes, and holds nothing but ASCII, which every console code page reads
    /// alike. A CLI whose path is not ASCII is named from the batch file's own folder when that
    /// way is; otherwise the wrapper says so and runs nothing. Its refusals are escaped too.
    #[test]
    fn the_batch_file_names_hostile_paths_and_refuses_what_it_cannot_name() {
        let home = r"C:\Users\Jürgen\AppData\Roaming\OmniGraph Studio\agent\graphs\g\.studio\a";
        let name = |cli: &str| cmd_program(Path::new(home), Path::new(cli));
        assert_eq!(
            name(r"C:\Tools\50% & ^y (z) !w! 'q';=,\omnigraph.exe").as_deref(),
            Some(r#""C:\Tools\50%% & ^y (z) !w! 'q';=,\omnigraph.exe""#)
        );
        // Under the same profile, in any case, the way up is ASCII.
        let up = r"..\..\..\..\..\..\..\..";
        assert_eq!(
            name(r"C:\Users\Jürgen\.local\bin\omnigraph.exe"),
            Some(format!(r#""%~dp0{up}\.local\bin\omnigraph.exe""#))
        );
        assert_eq!(
            name(r"c:\users\JÜRGEN\100%\omnigraph.exe"),
            Some(format!(r#""%~dp0{up}\100%%\omnigraph.exe""#))
        );
        for cli in [
            r"C:\Users\Jürgen\Ünïcode\omnigraph.exe",
            r"D:\Jürgen\omnigraph.exe",
            r"C:\日本\omnigraph.exe",
        ] {
            assert_eq!(name(cli), None, "{cli}");
            let refusal = cmd_script(&Wrapper::Run {
                home: Path::new(home),
                cli: Path::new(cli),
            });
            assert!(refusal.contains("omnigraph_run") && !refusal.contains("%*"));
            assert!(refusal.is_ascii(), "{refusal}");
        }

        let refusal = cmd_refusal("No CLI: \"C:\\Jürgen\\x\" 100% & <a|b> ^c (d)");
        assert_eq!(
            refusal,
            "@echo off\r\n\
             rem Written by OmniGraph Studio for one agent conversation (spec 013).\r\n\
             setlocal EnableExtensions DisableDelayedExpansion\r\n\
             1>&2 echo(No CLI: 'C:\\J?rgen\\x' 100%% ^& ^<a^|b^> ^^c ^(d^)\r\n\
             exit /b 1\r\n"
        );
        // A long text is split short of cmd's 8,191-character line.
        let long = cmd_refusal(&"x".repeat(2500));
        assert_eq!(long.matches("echo(").count(), 3);
        for script in [refusal, long] {
            assert!(script.lines().all(|line| line.len() < 8191));
        }
    }

    /// A CLI the batch file cannot name leaves the wrapper refusing, so the agent is not told
    /// to run it from its shell: it is told why it cannot and to use `omnigraph_run`, which
    /// starts the CLI by its path and still runs it. A shell script names any path.
    #[test]
    fn a_cli_the_batch_file_cannot_name_is_not_offered_to_the_shell() {
        let cwd = std::env::temp_dir().join(format!("studio-cli-unnamed-{}", std::process::id()));
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::create_dir_all(&cwd).unwrap();
        let home = CliHome::write(
            &cwd,
            &cwd.join("private"),
            1,
            "t",
            "knowledge",
            "agent/x",
            "main",
        )
        .unwrap();
        let unnamed = Path::new(r"D:\Jürgen\omnigraph.exe");
        assert!(!home.use_cli_on(unnamed, true).unwrap());
        let wrapper = std::fs::read_to_string(&home.command).unwrap();
        assert!(
            wrapper.contains("omnigraph_run") && !wrapper.contains("%*"),
            "{wrapper}"
        );
        assert_eq!(home.cli().unwrap(), unnamed, "omnigraph_run still runs it");
        let command = home.command.to_string_lossy().into_owned();
        let told = home.instructions_on(Some(true), true);
        assert!(
            told.starts_with("Your shell cannot run") && told.contains("omnigraph_run"),
            "{told}"
        );
        assert!(!told.contains(&command), "{told}");

        // A CLI it can name runs from the shell again, and a shell script names any.
        assert!(
            home.use_cli_on(Path::new(r"C:\Tools\omnigraph.exe"), true)
                .unwrap()
        );
        assert!(home.instructions_on(Some(true), true).contains(&command));
        assert!(home.use_cli_on(unnamed, false).unwrap());
        assert!(home.instructions_on(Some(true), false).contains(&command));
        drop(home);
        std::fs::remove_dir_all(&cwd).ok();
    }

    /// cmd cannot work in a network folder, so a working directory there is told apart.
    #[test]
    fn network_folders_are_told_apart() {
        for network in [
            r"\\server\share\ada\AppData\Roaming",
            r"\\?\UNC\server\share\ada",
            "//server/share/ada",
        ] {
            assert!(on_network(Path::new(network)), "{network}");
        }
        for local in [
            r"C:\Users\ada\AppData\Local",
            r"\\?\C:\Users\ada",
            r"\\.\pipe\x",
            "/Users/ada",
        ] {
            assert!(!on_network(Path::new(local)), "{local}");
        }
    }

    /// A person's registry can turn delayed expansion on for every batch file, as `cmd /V:ON`
    /// does: the refusal turns it off, so a `!` and a `^` in its text stay as written.
    #[cfg(windows)]
    #[test]
    fn a_refusal_keeps_its_text_with_delayed_expansion_on() {
        let root = std::env::temp_dir().join(format!("studio-cmd-refusal-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let home = CliHome::write(
            &root,
            &root.join("private"),
            1,
            "t",
            "knowledge",
            "agent/x",
            "main",
        )
        .unwrap();
        home.refuse("No CLI: !y! ^c 100%").unwrap();
        let output = std::process::Command::new("cmd")
            .args(["/D", "/V:ON", "/C"])
            .arg(&home.command)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim_end(),
            "No CLI: !y! ^c 100%"
        );
        drop(home);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Set for this test binary when it runs as the CLI.
    #[cfg(windows)]
    const STAND_IN: &str = "STUDIO_TEST_CLI_STAND_IN";
    /// The test that is the CLI when this test binary runs as one.
    #[cfg(windows)]
    const STAND_IN_TEST: &str = "cli_home::tests::stands_in_for_the_cli";
    /// Where the stand-in CLI's own arguments start.
    #[cfg(windows)]
    const CLI: &str = "studio-test-cli";

    /// What the batch file test starts this test binary as: the CLI, which prints its
    /// arguments and what it inherited, and exits with 7. Run as a test, it does nothing.
    #[cfg(windows)]
    #[test]
    fn stands_in_for_the_cli() {
        if std::env::var_os(STAND_IN).is_none() {
            return;
        }
        let args: Vec<String> = std::env::args().collect();
        let Some(at) = args.iter().position(|arg| arg == CLI) else {
            return;
        };
        let var = |name: &str| std::env::var(name).ok();
        let seen = serde_json::json!({
            "args": &args[at + 1..],
            "home": var("OMNIGRAPH_HOME"),
            "no_proxy": var("NO_PROXY"),
            "token": var("OMNIGRAPH_TOKEN_PERSONAL"),
            "bearer": var("OMNIGRAPH_BEARER_TOKEN"),
            "kept": var("STUDIO_TEST_KEPT"),
        });
        println!("cli {seen}");
        std::process::exit(7);
    }

    /// The batch file runs the CLI from a home whose folder holds what cmd reads and what a
    /// batch file's text cannot, with its own home, loopback first in `NO_PROXY` and no other
    /// token, and every argument as given by a caller that escapes for cmd, as Rust's standard
    /// library does. This test binary stands in for the CLI. Its refusal says why and fails.
    #[cfg(windows)]
    #[test]
    fn the_batch_file_runs_the_cli_with_every_argument_as_given() {
        // No `%`: the standard library escapes a batch file's arguments for cmd, not its path,
        // which cmd would read; the batch file never writes its folder's name in its text.
        let root =
            std::env::temp_dir().join(format!("studio-cmd & ^x (ü) !y! {}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let home = CliHome::write(
            &root,
            &root.join("private"),
            1,
            "t",
            "knowledge",
            "agent/x",
            "main",
        )
        .unwrap();
        assert!(home.command.ends_with("omnigraph.cmd"));
        let cli = std::env::current_exe().unwrap();
        assert!(home.use_cli(&cli).unwrap(), "the batch file names {cli:?}");
        let script = std::fs::read_to_string(&home.command).unwrap();
        assert!(script.is_ascii(), "{script}");

        let given = [
            "one",
            "two words",
            "a \"quoted\" word",
            "",
            "100%",
            "%PATH%",
            "^caret",
            "a&b",
            "x|y",
            "<in>",
            "(paren)",
            "!bang!",
            "trailing\\",
            "naïve 日本",
        ];
        let output = std::process::Command::new(&home.command)
            .args(["--exact", STAND_IN_TEST, "--nocapture", "--", CLI])
            .args(given)
            .env(STAND_IN, "1")
            .env("OMNIGRAPH_TOKEN_PERSONAL", "secret")
            .env("OMNIGRAPH_BEARER_TOKEN", "secret")
            .env("NO_PROXY", "corp.example")
            .env("STUDIO_TEST_KEPT", "kept")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find_map(|line| line.strip_prefix("cli "))
            .unwrap_or_else(|| {
                panic!(
                    "the CLI did not run: {stdout}{}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        let seen: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(seen["args"], serde_json::json!(given));
        assert_eq!(seen["home"], serde_json::json!(home.dir.to_str().unwrap()));
        assert_eq!(seen["no_proxy"], "127.0.0.1,localhost,corp.example");
        assert!(
            seen["token"].is_null() && seen["bearer"].is_null(),
            "{seen}"
        );
        assert_eq!(seen["kept"], "kept");
        assert_eq!(
            output.status.code(),
            Some(7),
            "the wrapper exits as the CLI does"
        );

        home.refuse("No CLI: \"x\" 100% & <a|b> ^c (d)").unwrap();
        let output = std::process::Command::new(&home.command)
            .arg("query")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim_end(),
            "No CLI: 'x' 100% & <a|b> ^c (d)"
        );
        drop(home);
        std::fs::remove_dir_all(&root).ok();
    }
}
