//! The agent's `omnigraph` CLI for one conversation (spec 013). Each conversation has a working
//! directory of its own ([`crate::sessions::working_dir`]), so what is the conversation's lives
//! there, at the same names in every conversation, and the agent needs no more than `AGENTS.md`
//! to find it:
//!
//! - `omnigraph` (`0700`) runs the CLI Studio found with `.studio` as `OMNIGRAPH_HOME`,
//!   loopback outside any proxy, and no other Omnigraph variable in its environment: a shell
//!   script on Unix, a batch file `omnigraph.cmd` on Windows ([`Wrapper`]). Until Studio has
//!   chosen a CLI, and when none fits, it runs nothing and says why;
//! - `.studio/config.yaml`, Omnigraph's version-1 operator configuration, names this
//!   conversation's gateway as the server target `studio`, its default, with the graph, at
//!   `https://127.0.0.1:<port>`;
//! - `.studio/token` (`0600`) holds the gateway's session token, never the person's, which the
//!   target reads as its token (`auth: {provider: token, source: {file: …}}`);
//! - `.studio/ca.pem` (`0600`) holds the certificate the gateway serves, the only one the target
//!   trusts (`transport.tls.ca`, `mode: replace`);
//! - `AGENTS.md` says how to use them: the graph, the agent's branch and the rules.
//!
//! The token lives in a file because Codex's environment filter drops names like `*TOKEN*`.
//! A conversation saved before working directories were per conversation runs in its graph's
//! shared directory, where two such conversations open at once write over each other's files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use graph_store::cli;

use crate::confined::Confined;

/// One conversation's CLI home. Dropping it removes the session token and the wrapper.
pub struct CliHome {
    /// The home, `.studio` in the working directory.
    pub dir: PathBuf,
    /// The agent's working directory. The agent writes there too, so each write opens it again
    /// by handle (`confined`).
    cwd: PathBuf,
}

/// What a conversation's CLI home needs to reach its gateway ([`crate::gateway::Gateway::access`]).
#[derive(Debug, Clone, Copy)]
pub struct GatewayAccess<'a> {
    pub port: u16,
    /// The session token the gateway checks on every request but its health probe.
    pub token: &'a str,
    /// The certificate the gateway serves (PEM), which the CLI trusts alone.
    pub certificate: &'a str,
}

/// The home's folder in the working directory.
const HOME_DIR: &str = ".studio";
/// A home's operator configuration.
const CONFIG_FILE: &str = "config.yaml";
/// The files of a home besides its config: the session token and the gateway's certificate.
const TOKEN_FILE: &str = "token";
const CA_FILE: &str = "ca.pem";

/// What the wrapper says before Studio has chosen the conversation's CLI.
const CHOOSING: &str = "Studio is still choosing the omnigraph CLI for this conversation. Try \
again in a few seconds.";

/// The CLI config for the home in `home`, Omnigraph's version-1 operator configuration: the
/// gateway on `port` as the server target `studio`, its default, with `graph`, reached over HTTPS
/// with the token in `home`'s `token`, trusting only the certificate in its `ca.pem`, and never
/// through a proxy. Written by hand, in the plain YAML the CLI reads strictly (no null, tag,
/// anchor or key it does not know), each file by its absolute path.
fn config(home: &Path, port: u16, graph: &str) -> String {
    let home = std::path::absolute(home).unwrap_or_else(|_| home.to_owned());
    let file = |name: &str| yaml_string(&home.join(name).to_string_lossy());
    [
        "# Written by Omnigraph Studio for one agent conversation (spec 013).".to_owned(),
        "version: 1".into(),
        "targets:".into(),
        "  studio:".into(),
        "    kind: server".into(),
        format!(
            "    endpoint: {}",
            yaml_string(&format!("https://127.0.0.1:{port}"))
        ),
        "    auth:".into(),
        "      provider: token".into(),
        "      source:".into(),
        format!("        file: {}", file(TOKEN_FILE)),
        "    transport:".into(),
        "      tls:".into(),
        "        ca:".into(),
        "          mode: replace".into(),
        format!("          file: {}", file(CA_FILE)),
        "      proxy:".into(),
        "        mode: direct".into(),
        format!("    default_graph: {}", yaml_string(graph)),
        "defaults:".into(),
        "  target: studio".into(),
    ]
    .map(|line| format!("{line}\n"))
    .concat()
}

impl Drop for CliHome {
    fn drop(&mut self) {
        // Opened, not made: a folder removed meanwhile is not made again to be cleaned.
        let Ok(cwd) = Confined::open(&self.cwd) else {
            return;
        };
        cwd.remove(WRAPPER_NAME.as_ref()).ok();
        // What Studio wrote goes, then the folder, unless the agent left a file of its own in it.
        if let Ok(home) = cwd.existing_dirs(HOME_DIR) {
            for name in [TOKEN_FILE, CA_FILE, CONFIG_FILE] {
                home.remove(name.as_ref()).ok();
            }
        }
        cwd.remove_dir(HOME_DIR).ok();
    }
}

impl CliHome {
    /// Writes the conversation's home in `cwd`: its config, token and certificate, and a
    /// wrapper that says Studio is still choosing the CLI. [`CliHome::use_cli`] points the
    /// wrapper at the CLI once one is chosen.
    pub fn write(cwd: &Path, gateway: GatewayAccess<'_>, graph: &str) -> std::io::Result<Self> {
        let dir = cwd.join(HOME_DIR);
        // Private: the token is the gateway's.
        let home = Confined::open(cwd)?.dir(HOME_DIR)?;
        home.set_mode(0o700)?;
        // The config names its files by their absolute paths.
        home.write(
            CONFIG_FILE,
            config(&dir, gateway.port, graph).as_bytes(),
            0o600,
        )?;
        home.write(TOKEN_FILE, gateway.token.as_bytes(), 0o600)?;
        home.write(CA_FILE, gateway.certificate.as_bytes(), 0o600)?;
        let written = Self {
            dir,
            cwd: cwd.to_owned(),
        };
        written.refuse(CHOOSING)?;
        Ok(written)
    }

    /// The wrapper the agent runs.
    #[cfg(test)]
    pub fn command(&self) -> PathBuf {
        self.cwd.join(WRAPPER_NAME)
    }

    /// Points the wrapper at `cli`: whether it runs it. On Windows a batch file that cannot
    /// name the CLI refuses and says why.
    pub fn use_cli(&self, cli: &Path) -> std::io::Result<bool> {
        self.use_cli_on(cli, cfg!(windows))
    }

    /// [`CliHome::use_cli`] with the batch file when `windows`.
    fn use_cli_on(&self, cli: &Path, windows: bool) -> std::io::Result<bool> {
        let wrapper = Wrapper::Run {
            home: &self.dir,
            cli,
        };
        Confined::open(&self.cwd)?.write(
            WRAPPER_NAME,
            wrapper.script_on(windows).as_bytes(),
            0o700,
        )?;
        Ok(!windows || cmd_program(&self.cwd, cli).is_some())
    }

    /// Makes the wrapper run nothing and say `why`: the conversation has no CLI that speaks the
    /// graph's contract, and one the server would refuse must not run in its place.
    pub fn refuse(&self, why: &str) -> std::io::Result<()> {
        Confined::open(&self.cwd)?.write(
            WRAPPER_NAME,
            Wrapper::Refuse { why }.script().as_bytes(),
            0o700,
        )
    }
}

/// Writes `<cwd>/AGENTS.md`, what the conversation's agent needs to work on the graph: the CLI
/// to run, its branch and the rules. Codex reads `AGENTS.md`; Claude reads it when there is no
/// `CLAUDE.md`.
pub fn write_agents_md(cwd: &Path, graph: &str, branch: &str, base: &str) -> std::io::Result<()> {
    let text = agents_md(graph, branch, base, cfg!(windows), on_network(cwd));
    Confined::open(cwd)?.write("AGENTS.md", text.as_bytes(), 0o644)
}

/// `AGENTS.md`'s text, with the batch file's notes when `windows` (and that cmd cannot work in a
/// network folder when the working directory is one).
fn agents_md(graph: &str, branch: &str, base: &str, windows: bool, network: bool) -> String {
    let command = if windows {
        ".\\omnigraph.cmd"
    } else {
        "./omnigraph"
    };
    let batch = match (windows, network) {
        (false, _) => "",
        (true, false) => {
            "- It is a batch file, so cmd reads its arguments before the CLI does: pass a query \
             or parameters that hold `\"` together with `%`, `&`, `|`, `<`, `>` or `^` in a \
             file (`--query`, `--params-file`).\n"
        }
        // cmd starts in the Windows folder instead, and the CLI reads relative paths from there.
        (true, true) => {
            "- It is a batch file, so cmd reads its arguments before the CLI does: pass a query \
             or parameters that hold `\"` together with `%`, `&`, `|`, `<`, `>` or `^` in a \
             file (`--query`, `--params-file`). This folder is a network folder, which cmd \
             cannot work in: give the CLI every file by its absolute path.\n"
        }
    };
    format!(
        "# Omnigraph graph `{graph}`\n\n\
         Written by Omnigraph Studio. You work on the Omnigraph graph `{graph}` with a person in \
         Studio, on your own branch `{branch}`.\n\n\
         Before writing a query, a mutation or a load, read the `omnigraph-studio` skill \
         (`.claude/skills/omnigraph-studio/SKILL.md` or `.agents/skills/omnigraph-studio/SKILL.md` \
         here): GQ syntax, loads, search, and what differs in Studio.\n\n\
         ## The omnigraph CLI\n\n\
         - Run the graph's CLI as `{command}`, in this folder. Another `omnigraph` on PATH is not \
         connected to Studio.\n\
         {batch}\
         - Its config already points it at Studio and this graph: give no `--target` or \
         `--graph`.\n\
         - Read any branch or commit (`--branch`, `--snapshot`).\n\
         - Write only to your branch, `{branch}`. Create it first with `{command} branch create \
         {branch} --from {base}`, then pass `--branch {branch}` to `mutate`, `load` and Blob \
         writes.\n\
         - Batch writes with `load` (NDJSON): each `mutate` is a commit of its own.\n\
         - Branch and commit statements inside `mutate -e`, merges into other branches, and \
         publishing or dropping commits are refused. The person merges your branch in Studio's \
         review.\n\n\
         ## Outcomes\n\n\
         - A refusal from Studio says \"Nothing was sent\": nothing was written.\n\
         - `effects: unknown` on a write Studio sent means the server's answer was lost. Check \
         `commit list --branch {branch}` before trying again; never repeat a write blindly.\n\
         - When `{command}` says Studio has no CLI for this conversation, tell the person what \
         it says.\n"
    )
}

/// How to install the newest Omnigraph release, as its install guide gives it.
const INSTALL: &str = if cfg!(windows) {
    "powershell -NoProfile -ExecutionPolicy Bypass -Command \"iwr -UseBasicParsing \
     https://raw.githubusercontent.com/ModernRelay/omnigraph/main/scripts/install.ps1 | iex\""
} else {
    "curl -fsSL https://raw.githubusercontent.com/ModernRelay/omnigraph/main/scripts/install.sh \
     | bash"
};

/// How long the choice of a conversation's CLI waits for its turn at the CLI, which Studio's own
/// commands may hold (a token's export waits for its credential helper). The choice goes before
/// the asks nobody waits on.
const CHOICE_WAIT: Duration = Duration::from_secs(120);

/// Why a CLI Studio found is not the conversation's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unfit {
    /// Studio does not run it at all ([`cli::probe`], which Studio's own CLI passes too): its
    /// words, which follow the CLI's path ("is 0.15.0, whose target protocol 3 Studio does not
    /// know").
    Probe(String),
    /// It is `version`, whose wrapper Studio could not write.
    Wrapper { version: String, error: String },
}

/// The conversation's CLI, or why it has none.
#[derive(Debug)]
pub enum CliChoice {
    Chosen {
        cli: PathBuf,
        version: String,
        /// Whether the wrapper runs it: on Windows the batch file names the CLI only by a path
        /// it can hold, and otherwise refuses.
        wrapper: bool,
    },
    /// No candidate fits: each one found, with why it does not ([`no_cli_text`]).
    NoMatch { found: Vec<(PathBuf, Unfit)> },
    /// Studio's other commands held the CLI past [`CHOICE_WAIT`], so none was asked: why.
    Busy(String),
}

impl CliChoice {
    /// Why the conversation has no CLI, as its wrapper says it; `None` for one chosen.
    pub fn refusal(&self) -> Option<String> {
        match self {
            CliChoice::Chosen { .. } => None,
            CliChoice::NoMatch { found } => Some(no_cli_text(found)),
            CliChoice::Busy(why) => Some(why.clone()),
        }
    }
}

/// Chooses the conversation's CLI from `candidates` ([`cli::candidates`] in Studio), in their
/// order, with Studio's own finder: the first program there that Studio runs ([`cli::probe`],
/// remembered while its file is unchanged), whose protocols say it reads the version-1 operator
/// configuration Studio writes, so a slow or unavailable server never decides it. The commands run one at a time with Studio's others ([`cli::take_turn`]).
/// With no match the wrapper runs nothing and says why, rather than pointing at a CLI the server
/// or the home would refuse.
pub async fn choose_cli(home: &CliHome, candidates: Vec<PathBuf>) -> CliChoice {
    let asked = "the look for the agent's omnigraph CLI";
    // A person waits on the conversation.
    let hurry = Arc::new(AtomicBool::new(true));
    let turn = match cli::take_turn(&hurry, CHOICE_WAIT, asked).await {
        Ok(turn) => turn,
        Err(busy) => {
            let why = format!(
                "Studio could not choose an omnigraph CLI for this conversation: {busy}. Start \
                 a new conversation to try again."
            );
            home.refuse(&why).ok();
            return CliChoice::Busy(why);
        }
    };
    let mut found = Vec::new();
    for program in cli::installed(&candidates) {
        let unfit = match cli::probe(&program, &turn).await {
            Err(why) => Unfit::Probe(why),
            Ok(fit) => match home.use_cli(&fit.path) {
                Ok(wrapper) => {
                    return CliChoice::Chosen {
                        cli: fit.path,
                        version: fit.version,
                        wrapper,
                    };
                }
                Err(error) => Unfit::Wrapper {
                    version: fit.version,
                    error: error.to_string(),
                },
            },
        };
        found.push((program, unfit));
    }
    home.refuse(&no_cli_text(&found)).ok();
    CliChoice::NoMatch { found }
}

/// Why a conversation has no CLI: what the graph and the home need, each CLI found with why it
/// does not fit, and what to install.
fn no_cli_text(found: &[(PathBuf, Unfit)]) -> String {
    let mut text = format!(
        "No omnigraph CLI here speaks this graph's HTTP API ({}) and reads Omnigraph's \
         version-1 operator configuration, which Studio writes for the agent's CLI",
        graph_client::CONTRACT
    );
    if found.is_empty() {
        text.push_str(": Studio found none");
    } else {
        let each: Vec<String> = found
            .iter()
            .map(|(program, unfit)| {
                let program = program.display();
                match unfit {
                    Unfit::Probe(why) => format!("{program} {why}"),
                    Unfit::Wrapper { version, error } => format!(
                        "{program} is {version}, whose wrapper Studio could not write ({error})"
                    ),
                }
            })
            .collect();
        text.push_str(": ");
        text.push_str(&each.join("; "));
    }
    text.push_str(&format!(
        ". Install Omnigraph {} or later: `{INSTALL}`. Or set `omnigraph_cli` in Studio's \
         settings to such a CLI.",
        cli::FIRST_CLI_RELEASE
    ));
    text
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn yaml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// The wrapper's file name in the working directory: the name the agent runs it by.
const WRAPPER_NAME: &str = if cfg!(windows) {
    "omnigraph.cmd"
} else {
    "omnigraph"
};

/// The hosts put first in the CLI's `NO_PROXY`, so it reaches the gateway outside any proxy.
const LOOPBACK: &str = "127.0.0.1,localhost";

/// What the agent's `omnigraph` wrapper does, the one account both scripts are written from
/// ([`posix_script`], [`cmd_script`]), so only their syntax differs. Run, it removes every
/// Omnigraph variable from the CLI's environment, each whose name starts with
/// [`cli::VARIABLE_PREFIX`] in any ASCII case, as the agent's launch and
/// Studio's own CLI do ([`cli::omnigraph_variables`]), so no token kept for another target,
/// profile, home or control-plane address the agent's environment holds reaches the
/// conversation's CLI. Then it sets `OMNIGRAPH_HOME` to `home`, `.studio` beside it, puts
/// [`LOOPBACK`] first in `NO_PROXY` (and `no_proxy`), and runs `cli` with the agent's arguments as they came. Refusing, it runs
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
                 # Written by Omnigraph Studio for one agent conversation (spec 013).\n\
                 echo {why} >&2\n\
                 exit 1\n",
                why = shell_quote(why),
            );
        }
    };
    let mut script = String::from(
        "#!/bin/sh\n\
         # Written by Omnigraph Studio for one agent conversation (spec 013): the omnigraph\n\
         # CLI, pointed at Studio's gateway for this conversation.\n",
    );
    // Only names a shell can hold match: `unset` of any other would end the script, and the CLI
    // reads none. The prefix matches in any ASCII case, as everywhere Studio removes it.
    let prefix: String = cli::VARIABLE_PREFIX
        .chars()
        .map(|c| match c {
            c if c.is_ascii_alphabetic() => {
                format!("[{}{}]", c.to_ascii_uppercase(), c.to_ascii_lowercase())
            }
            c => c.to_string(),
        })
        .collect();
    script.push_str(&format!(
        "for name in $(env | sed -n 's/^\\({prefix}[A-Za-z0-9_]*\\)=.*/\\1/p'); do \
         unset \"$name\"; done\n\
         OMNIGRAPH_HOME={home}\n\
         NO_PROXY=\"{LOOPBACK}${{NO_PROXY:+,$NO_PROXY}}\"\n\
         no_proxy=\"$NO_PROXY\"\n\
         export OMNIGRAPH_HOME NO_PROXY no_proxy\n\
         exec {cli} \"$@\"\n",
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
/// shims included; `AGENTS.md` says so.
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
/// folder, the working directory, may hold any character: cmd knows it as `%~dp0`, from its own Unicode
/// path. The CLI is named by its path when that is ASCII, else from `%~dp0` when that path is
/// (as when the CLI and the working directory are both under the person's profile, whose name
/// is not);
/// otherwise the wrapper refuses and says why.
fn cmd_script(wrapper: &Wrapper<'_>) -> String {
    let (home, cli) = match wrapper {
        Wrapper::Run { home, cli } => (home, cli),
        Wrapper::Refuse { why } => return cmd_refusal(why),
    };
    // The batch file sits in the working directory, the home's folder.
    let Some(cli_named) = cmd_program(home.parent().unwrap_or(home), cli) else {
        return cmd_refusal(&format!(
            "The omnigraph CLI Studio chose for this conversation, {}, is in a folder whose \
             name a batch file cannot hold. Tell the person to set omnigraph_cli in Studio's \
             settings to a CLI whose path is in plain ASCII.",
            cli.display()
        ));
    };
    let mut lines = vec![
        "@echo off".to_owned(),
        "rem Written by Omnigraph Studio for one agent conversation (spec 013): the omnigraph"
            .to_owned(),
        "rem CLI, pointed at Studio's gateway for this conversation.".to_owned(),
        "setlocal EnableExtensions DisableDelayedExpansion".to_owned(),
    ];
    // `set NAME` lists every variable whose name starts with NAME, in any case, as Windows
    // reads names.
    lines.push(format!(
        "for /f \"delims==\" %%N in ('set {prefix} 2^>nul') do set \"%%N=\"",
        prefix = cli::VARIABLE_PREFIX,
    ));
    lines.extend([
        // The home in the batch file's own folder, which `%~dp0` ends with `\`.
        format!("set \"OMNIGRAPH_HOME=%~dp0{HOME_DIR}\""),
        // `NO_PROXY` and `no_proxy` are one variable to Windows.
        format!(
            "if defined NO_PROXY (set \"NO_PROXY={LOOPBACK},%NO_PROXY%\") else set \
             \"NO_PROXY={LOOPBACK}\""
        ),
        format!("{cli_named} %*"),
    ]);
    lines.iter().map(|line| format!("{line}\r\n")).collect()
}

/// How the batch file in `folder` names `cli`: its path in double quotes, or the same from the
/// batch file's own folder (`%~dp0`), whichever is ASCII first; `None` when neither is. Both are
/// read as Windows paths, whatever platform reads them.
fn cmd_program(folder: &Path, cli: &Path) -> Option<String> {
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
    let (folder, cli) = (parts(folder.to_str()?), parts(cli));
    let shared = folder
        .iter()
        .zip(&cli)
        .take_while(|(a, b)| a.to_lowercase() == b.to_lowercase())
        .count();
    // Not even the drive in common.
    if shared == 0 {
        return None;
    }
    let mut relative: Vec<&str> = vec![".."; folder.len() - shared];
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
        "rem Written by Omnigraph Studio for one agent conversation (spec 013).".to_owned(),
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

    /// A stand-in for a gateway's certificate, where no gateway runs.
    const CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----\nstand-in\n-----END CERTIFICATE-----\n";

    /// A gateway on `port` with `token`, as a home's tests write it.
    fn access(port: u16, token: &str) -> GatewayAccess<'_> {
        GatewayAccess {
            port,
            token,
            certificate: CERTIFICATE,
        }
    }

    /// A fake CLI in `dir`. Given `version`, it answers `version --json` with it; `config` is
    /// what it does otherwise. Every other command fails, as one would with no server behind the
    /// gateway.
    #[cfg(unix)]
    fn fake_cli(dir: &Path, name: &str, version: Option<&str>, config: &str) -> PathBuf {
        let path = dir.join(name);
        let answers = version
            .map(|json| {
                format!("[ \"$1 $2\" = \"version --json\" ] && {{ echo '{json}'; exit 0; }}\n")
            })
            .unwrap_or_default();
        std::fs::write(&path, format!("#!/bin/sh\n{answers}{config}\nexit 2\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// What `version --json` reports for a CLI of `version` that speaks `contract`, reads target
    /// protocol `target` and credential and access protocol 2, as Omnigraph 0.14.0 does.
    #[cfg(unix)]
    fn reports(version: &str, contract: &str, target: u64) -> String {
        format!(
            r#"{{"version":"{version}","http_contract":"{contract}","protocols":{{"target":{target},"credential":2,"access":2}}}}"#
        )
    }

    /// The first candidate that Studio's own finder takes (it speaks the graph's contract and
    /// reports target, credential and access protocol 2, as Omnigraph 0.14.0 does) is chosen,
    /// with no server behind the gateway. A build before the release (the edge builds
    /// `0.14.0-edge+50283ce` and `0.14.0-edge+888223b`, which report protocol 1), one that
    /// reports a target protocol Studio does not know, one that speaks another contract and one
    /// without protocols are passed over; with none left, the wrapper runs nothing and its
    /// refusal names each CLI found, why, and the release to install.
    #[cfg(unix)]
    #[test]
    fn the_first_cli_studio_drives_is_chosen_without_a_server() {
        let dir = std::env::temp_dir().join(format!("studio-cli-choice-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let fake = |name, version: Option<&str>, config| fake_cli(&dir, name, version, config);
        // 0.13.0 predates `version --json`.
        let old = fake(
            "omnigraph-0.13",
            None,
            r#"[ "$1" = --version ] && { echo "omnigraph 0.13.0"; exit 0; }"#,
        );
        // Speaks the contract but reports no protocols.
        let untargeted = fake(
            "omnigraph-0.14-dev",
            Some(r#"{"version":"0.14.0-dev","http_contract":"0.14"}"#),
            "",
        );
        // Edge builds before the release report protocol 1.
        let before = fake(
            "omnigraph-50283ce",
            Some(
                r#"{"version":"0.14.0-edge+50283ce","http_contract":"0.14","protocols":{"target":1,"credential":1}}"#,
            ),
            "",
        );
        let early = fake(
            "omnigraph-888223b",
            Some(
                r#"{"version":"0.14.0-edge+888223b","http_contract":"0.14","protocols":{"target":1,"credential":1,"access":1}}"#,
            ),
            "",
        );
        let current = fake("omnigraph-0.14.0", Some(&reports("0.14.0", "0.14", 2)), "");
        let later = fake("omnigraph-later", Some(&reports("0.14.1", "0.14", 2)), "");
        let unknown_protocol = fake(
            "omnigraph-target-3",
            Some(&reports("0.16.0", "0.14", 3)),
            "",
        );
        // A wrapper that hangs: a match before it is chosen without waiting for it.
        let hung = fake("omnigraph-hung", None, "sleep 30");
        let other = fake("omnigraph-0.15", Some(&reports("0.15.0", "0.15", 2)), "");
        // The conversation's folder, apart from the fake CLIs.
        let cwd = dir.join("conversation");
        std::fs::create_dir_all(&cwd).unwrap();
        let home = CliHome::write(&cwd, access(1, "t"), "knowledge").unwrap();
        let wrapper = || std::fs::read_to_string(home.command()).unwrap();
        let choose = |candidates: Vec<PathBuf>| smol::block_on(choose_cli(&home, candidates));
        let chosen = |candidates: Vec<PathBuf>| match choose(candidates) {
            CliChoice::Chosen { cli, .. } => cli,
            other => panic!("{other:?}"),
        };

        match choose(vec![
            old.clone(),
            untargeted.clone(),
            before.clone(),
            early.clone(),
            current.clone(),
        ]) {
            CliChoice::Chosen {
                cli,
                version,
                wrapper,
            } => {
                assert_eq!(cli, current);
                assert_eq!(version, "0.14.0");
                assert!(wrapper, "a shell script names any path");
            }
            other => panic!("{other:?}"),
        }
        assert!(wrapper().contains(&*current.to_string_lossy()));
        assert_eq!(chosen(vec![other.clone(), later.clone()]), later);
        assert_eq!(
            chosen(vec![unknown_protocol.clone(), current.clone()]),
            current
        );

        let started = std::time::Instant::now();
        assert_eq!(chosen(vec![current.clone(), hung.clone()]), current);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "waited for the hung CLI after the match: {:?}",
            started.elapsed()
        );

        let refused = vec![
            old.clone(),
            untargeted.clone(),
            before.clone(),
            early.clone(),
            unknown_protocol.clone(),
            other.clone(),
        ];
        let choice = choose(refused.clone());
        let why = choice.refusal().unwrap_or_default();
        match choice {
            CliChoice::NoMatch { found } => {
                let probe = |why: &str| Unfit::Probe(why.to_owned());
                assert_eq!(
                    found,
                    refused
                        .iter()
                        .cloned()
                        .zip([
                            probe(
                                "answers no `version --json` (older than Omnigraph 0.14, or not \
                                 its CLI)"
                            ),
                            probe("is 0.14.0-dev, which reports no target protocol"),
                            probe(
                                "is 0.14.0-edge+50283ce, whose target protocol 1 Studio does not \
                                 know"
                            ),
                            probe(
                                "is 0.14.0-edge+888223b, whose target protocol 1 Studio does not \
                                 know"
                            ),
                            probe("is 0.16.0, whose target protocol 3 Studio does not know"),
                            probe("is 0.15.0, which speaks HTTP contract 0.15, not Studio's 0.14"),
                        ])
                        .collect::<Vec<_>>()
                );
                for says in [
                    "speaks this graph's HTTP API (0.14) and reads Omnigraph's version-1 operator \
                     configuration",
                    "omnigraph-0.13 answers no `version --json`",
                    &format!(
                        "{} is 0.14.0-edge+50283ce, whose target protocol 1",
                        before.display()
                    ),
                    "0.16.0, whose target protocol 3",
                    "0.15.0, which speaks HTTP contract 0.15",
                    "Install Omnigraph 0.14.0 or later",
                    "scripts/install.sh",
                    "set `omnigraph_cli` in Studio's settings",
                ] {
                    assert!(why.contains(says), "{says}: {why}");
                }
            }
            other => panic!("{other:?}"),
        }
        // It names the CLIs it found but runs none of them.
        assert!(!wrapper().contains("exec "), "{}", wrapper());
        let run = std::process::Command::new(home.command())
            .arg("query")
            .output()
            .unwrap();
        assert!(!run.status.success());
        let said = String::from_utf8_lossy(&run.stderr);
        assert_eq!(said.trim_end(), why, "the wrapper says what the choice did");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The agent's choice uses Studio's own finder: a CLI is probed once (`version --json`),
    /// what it was found to be is remembered while its file is unchanged, and a later
    /// conversation asks it nothing.
    #[cfg(unix)]
    #[test]
    fn a_cli_is_probed_once_for_every_conversation() {
        let dir = std::env::temp_dir().join(format!("studio-cli-probed-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls");
        let cli = dir.join("omnigraph");
        let script = format!(
            "#!/bin/sh\n\
             echo \"$*\" >> '{log}'\n\
             [ \"$1 $2\" = \"version --json\" ] && {{ echo '{json}'; exit 0; }}\n\
             exit 2\n",
            log = log.display(),
            json = reports("0.14.0", "0.14", 2),
        );
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let calls = || {
            let calls = std::fs::read_to_string(&log).unwrap_or_default();
            std::fs::remove_file(&log).ok();
            calls.lines().map(str::to_owned).collect::<Vec<_>>()
        };
        // Each conversation in a folder of its own, apart from the fake CLI.
        let conversation = |name: &str| {
            let cwd = dir.join(name);
            std::fs::create_dir_all(&cwd).unwrap();
            CliHome::write(&cwd, access(1, "t"), "knowledge").unwrap()
        };

        let first = conversation("one");
        let chosen = smol::block_on(choose_cli(&first, vec![cli.clone()]));
        assert!(matches!(chosen, CliChoice::Chosen { .. }), "{chosen:?}");
        assert_eq!(calls(), ["version --json".to_owned()]);
        let second = conversation("two");
        let chosen = smol::block_on(choose_cli(&second, vec![cli.clone()]));
        assert!(matches!(chosen, CliChoice::Chosen { .. }), "{chosen:?}");
        assert!(calls().is_empty(), "probed once");
        drop((first, second));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The home holds Omnigraph's version-1 operator configuration as its own schema and
    /// parser require it: `version: 1` and no `target_config_version`; the gateway as the server
    /// target `studio` at `https://127.0.0.1:<port>`, the default target, with the graph; its
    /// token read from the home's `token` file (the session token, a bearer token as the CLI
    /// reads one) and only the home's `ca.pem` trusted, both by absolute path and private; no
    /// proxy; and none of what the CLI refuses (a null, a key it does not know). The schema
    /// refuses the former form and a token sent over plain HTTP, so the check is not empty.
    #[test]
    fn the_home_is_omnigraphs_version_1_operator_configuration() {
        let cwd = std::env::temp_dir().join(format!("studio-cli-format-{}", std::process::id()));
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::create_dir_all(&cwd).unwrap();
        let certificate = crate::gateway_tls::identity().unwrap().certificate;
        let token = crate::confined::random_hex(32).unwrap();
        let home = CliHome::write(
            &cwd,
            GatewayAccess {
                port: 49152,
                token: &token,
                certificate: &certificate,
            },
            "knowledge",
        )
        .unwrap();
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../fixtures/omnigraph-operator-config.schema.json"
        ))
        .unwrap();
        // Plain block mappings alone, each value a lower-case word or a JSON-quoted string: no
        // anchor, alias, tag, flow collection or block scalar.
        let plain = regex::Regex::new(r#"\A( *)[a-z_]+:( [a-z0-9]+| ".*")?\z"#).unwrap();
        let bearer = regex::Regex::new(r"\A[A-Za-z0-9._~+/-]+=*\z").unwrap();
        fn no_null(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::Null => false,
                serde_json::Value::Array(items) => items.iter().all(no_null),
                serde_json::Value::Object(fields) => fields.values().all(no_null),
                _ => true,
            }
        }
        for dir in [home.dir.clone()] {
            let text = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
            assert!(!text.contains("target_config_version"), "{text}");
            for line in text.lines().filter(|line| !line.starts_with('#')) {
                assert!(plain.is_match(line), "{line}");
            }
            let config: serde_json::Value = serde_norway::from_str(&text).unwrap();
            let errors = crate::json_schema::errors(&schema, &config);
            assert!(errors.is_empty(), "{errors:?}\n{text}");
            assert!(no_null(&config), "{text}");
            let path = |name: &str| {
                let path = std::path::absolute(dir.join(name)).unwrap();
                serde_json::Value::String(path.to_string_lossy().into_owned())
            };
            assert_eq!(
                config,
                serde_json::json!({
                    "version": 1,
                    "targets": {
                        "studio": {
                            "kind": "server",
                            "endpoint": "https://127.0.0.1:49152",
                            "auth": { "provider": "token", "source": { "file": path("token") } },
                            "transport": {
                                "tls": { "ca": { "mode": "replace", "file": path("ca.pem") } },
                                "proxy": { "mode": "direct" },
                            },
                            "default_graph": "knowledge",
                        },
                    },
                    "defaults": { "target": "studio" },
                })
            );
            let saved = std::fs::read_to_string(dir.join("token")).unwrap();
            assert_eq!(saved, token);
            assert!(
                bearer.is_match(&saved),
                "a bearer token as the CLI reads one"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("ca.pem")).unwrap(),
                certificate
            );
            #[cfg(unix)]
            for name in ["config.yaml", "token", "ca.pem"] {
                let mode = std::fs::metadata(dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{name}");
            }
        }
        for refused in [
            serde_json::json!({ "target_config_version": 1, "targets": {} }),
            serde_json::json!({
                "version": 1,
                "targets": { "studio": {
                    "kind": "server",
                    "endpoint": "http://127.0.0.1:49152",
                    "auth": { "provider": "token", "source": { "file": "/t" } },
                } },
            }),
            serde_json::json!({ "version": 1, "defaults": { "target": "studio", "server": "x" } }),
        ] {
            assert!(
                !crate::json_schema::errors(&schema, &refused).is_empty(),
                "{refused}"
            );
        }
        drop(home);
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_conversation_home_holds_its_config_token_and_wrapper() {
        let cwd = std::env::temp_dir().join(format!("studio-cli-home-{}", std::process::id()));
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::create_dir_all(&cwd).unwrap();
        let home = CliHome::write(&cwd, access(49152, "abc123"), "knowledge").unwrap();
        assert_eq!(home.dir, cwd.join(".studio"));
        assert_eq!(home.command(), cwd.join("omnigraph"));
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home.dir), 0o700);
        for name in ["config.yaml", TOKEN_FILE, CA_FILE] {
            assert_eq!(mode(&home.dir.join(name)), 0o600, "{name}");
        }
        let config = std::fs::read_to_string(home.dir.join("config.yaml")).unwrap();
        assert!(
            config.contains("    endpoint: \"https://127.0.0.1:49152\"\n")
                && config.contains("    default_graph: \"knowledge\"\n"),
            "{config}"
        );
        assert!(
            config.ends_with("defaults:\n  target: studio\n"),
            "{config}"
        );
        assert_eq!(
            std::fs::read_to_string(home.dir.join(TOKEN_FILE)).unwrap(),
            "abc123"
        );
        assert_eq!(
            std::fs::read_to_string(home.dir.join(CA_FILE)).unwrap(),
            CERTIFICATE
        );
        // Until a CLI is chosen the wrapper runs nothing and says so.
        let waiting = std::process::Command::new(home.command()).output().unwrap();
        assert!(!waiting.status.success());
        assert!(String::from_utf8_lossy(&waiting.stderr).contains("still choosing"));

        home.use_cli(Path::new("/opt/it's here/omnigraph")).unwrap();
        assert_eq!(mode(&home.command()), 0o700);
        let script = std::fs::read_to_string(home.command()).unwrap();
        assert!(
            script.contains("exec '/opt/it'\\''s here/omnigraph' \"$@\""),
            "{script}"
        );
        assert!(script.contains(&format!("OMNIGRAPH_HOME='{}'", home.dir.display())));

        // The wrapper runs with its own home, loopback outside any proxy, and no other Omnigraph
        // variable, one the CLI does not read today and one in another case included.
        home.use_cli(Path::new("/usr/bin/env")).unwrap();
        let output = std::process::Command::new(home.command())
            .env("OMNIGRAPH_TOKEN_PERSONAL", "secret")
            .env("OMNIGRAPH_BEARER_TOKEN", "secret")
            .env("OMNIGRAPH_FOO", "secret")
            .env("Omnigraph_Token_Work", "secret")
            .env("OMNIGRAPH_HOME", "/elsewhere")
            .env("NO_PROXY", "corp.example")
            .env("STUDIO_TEST_KEPT", "kept")
            .output()
            .unwrap();
        let env = String::from_utf8_lossy(&output.stdout);
        assert!(
            !env.contains("secret") && !env.contains("/elsewhere"),
            "{env}"
        );
        for line in env.lines().filter(|line| line.starts_with("OMNIGRAPH_")) {
            assert!(line.starts_with("OMNIGRAPH_HOME="), "{line}");
        }
        assert!(env.contains("STUDIO_TEST_KEPT=kept"), "{env}");
        assert!(
            env.contains("NO_PROXY=127.0.0.1,localhost,corp.example"),
            "{env}"
        );
        assert!(
            env.contains(&format!("OMNIGRAPH_HOME={}", home.dir.display())),
            "{env}"
        );

        let (token, certificate) = (home.dir.join(TOKEN_FILE), home.dir.join(CA_FILE));
        let wrapper = home.command();
        drop(home);
        assert!(
            !token.exists() && !certificate.exists() && !wrapper.exists(),
            "the token, the certificate and the wrapper go with the conversation"
        );
        assert!(
            !cwd.join(".studio").exists(),
            "and so does the home's folder"
        );
        std::fs::remove_dir_all(&cwd).ok();
    }

    /// `AGENTS.md` is the agent's one guide to the conversation: the CLI to run in its folder,
    /// its branch and base, and the rules; on Windows the batch file's notes, and that cmd cannot
    /// work in a network folder when the folder is one. Nothing points at a tool Studio no
    /// longer serves.
    #[test]
    fn agents_md_names_the_cli_the_branch_and_the_rules() {
        let text = agents_md("knowledge", "agent/20261028-101500", "main", false, false);
        for part in [
            "graph `knowledge`",
            "on your own branch `agent/20261028-101500`",
            "Run the graph's CLI as `./omnigraph`",
            "`./omnigraph branch create agent/20261028-101500 --from main`",
            "--branch agent/20261028-101500",
            "give no `--target` or `--graph`",
            "omnigraph-studio",
        ] {
            assert!(text.contains(part), "{part}\n{text}");
        }
        assert!(!text.contains("batch file") && !text.contains("MCP") && !text.contains("_run"));
        let windows = agents_md("knowledge", "agent/x", "main", true, false);
        assert!(windows.contains(".\\omnigraph.cmd") && windows.contains("batch file"));
        assert!(!windows.contains("network folder"));
        assert!(agents_md("knowledge", "agent/x", "main", true, true).contains("network folder"));

        let cwd = std::env::temp_dir().join(format!("studio-agents-md-{}", std::process::id()));
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::create_dir_all(&cwd).unwrap();
        write_agents_md(&cwd, "knowledge", "agent/x", "main").unwrap();
        let written = std::fs::read_to_string(cwd.join("AGENTS.md")).unwrap();
        assert_eq!(
            written,
            agents_md("knowledge", "agent/x", "main", cfg!(windows), false)
        );
        std::fs::remove_dir_all(&cwd).ok();
    }

    /// The shell script and the batch file are written from one account of the wrapper: the
    /// same variables removed, `.studio` beside it as the home, loopback first in `NO_PROXY`, and the
    /// CLI run with the agent's arguments as they came. Only the syntax differs.
    #[test]
    fn both_wrappers_are_written_from_one_account() {
        let posix = posix_script(&Wrapper::Run {
            home: Path::new("/data/agent/graphs/g/agent-x/.studio"),
            cli: Path::new("/opt/it's here/omnigraph"),
        });
        assert_eq!(
            posix,
            "#!/bin/sh\n\
             # Written by Omnigraph Studio for one agent conversation (spec 013): the omnigraph\n\
             # CLI, pointed at Studio's gateway for this conversation.\n\
             for name in $(env | sed -n 's/^\\([Oo][Mm][Nn][Ii][Gg][Rr][Aa][Pp][Hh]_[A-Za-z0-9_]*\\)=.*/\\1/p'); do unset \"$name\"; done\n\
             OMNIGRAPH_HOME='/data/agent/graphs/g/agent-x/.studio'\n\
             NO_PROXY=\"127.0.0.1,localhost${NO_PROXY:+,$NO_PROXY}\"\n\
             no_proxy=\"$NO_PROXY\"\n\
             export OMNIGRAPH_HOME NO_PROXY no_proxy\n\
             exec '/opt/it'\\''s here/omnigraph' \"$@\"\n"
        );
        let cmd = cmd_script(&Wrapper::Run {
            home: Path::new(
                r"C:\Users\ada\AppData\Roaming\Omnigraph Studio\agent\graphs\g\agent-x\.studio",
            ),
            cli: Path::new(r"C:\Users\ada\.local\bin\omnigraph.exe"),
        });
        let lines = [
            "@echo off",
            "rem Written by Omnigraph Studio for one agent conversation (spec 013): the omnigraph",
            "rem CLI, pointed at Studio's gateway for this conversation.",
            "setlocal EnableExtensions DisableDelayedExpansion",
            "for /f \"delims==\" %%N in ('set OMNIGRAPH_ 2^>nul') do set \"%%N=\"",
            "set \"OMNIGRAPH_HOME=%~dp0.studio\"",
            "if defined NO_PROXY (set \"NO_PROXY=127.0.0.1,localhost,%NO_PROXY%\") else set \
             \"NO_PROXY=127.0.0.1,localhost\"",
            "\"C:\\Users\\ada\\.local\\bin\\omnigraph.exe\" %*",
        ];
        assert_eq!(cmd, lines.map(|line| format!("{line}\r\n")).concat());
    }

    /// The batch file quotes every path it names, doubling `%`, the one character cmd reads
    /// inside double quotes, and holds nothing but ASCII, which every console code page reads
    /// alike. A CLI whose path is not ASCII is named from the batch file's own folder when that
    /// way is; otherwise the wrapper says so and runs nothing. Its refusals are escaped too.
    #[test]
    fn the_batch_file_names_hostile_paths_and_refuses_what_it_cannot_name() {
        let folder = r"C:\Users\Jürgen\AppData\Roaming\Omnigraph Studio\agent\graphs\g\agent-x\a";
        let home = format!(r"{folder}\.studio");
        let name = |cli: &str| cmd_program(Path::new(folder), Path::new(cli));
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
                home: Path::new(&home),
                cli: Path::new(cli),
            });
            assert!(refusal.contains("omnigraph_cli") && !refusal.contains("%*"));
            assert!(refusal.is_ascii(), "{refusal}");
        }

        let refusal = cmd_refusal("No CLI: \"C:\\Jürgen\\x\" 100% & <a|b> ^c (d)");
        assert_eq!(
            refusal,
            "@echo off\r\n\
             rem Written by Omnigraph Studio for one agent conversation (spec 013).\r\n\
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

    /// A CLI the batch file cannot name leaves the wrapper refusing and saying why; one it can
    /// name runs again, and a shell script names any.
    #[test]
    fn a_cli_the_batch_file_cannot_name_is_not_offered_to_the_shell() {
        let cwd = std::env::temp_dir().join(format!("studio-cli-unnamed-{}", std::process::id()));
        std::fs::remove_dir_all(&cwd).ok();
        std::fs::create_dir_all(&cwd).unwrap();
        let home = CliHome::write(&cwd, access(1, "t"), "knowledge").unwrap();
        let unnamed = Path::new(r"D:\Jürgen\omnigraph.exe");
        assert!(!home.use_cli_on(unnamed, true).unwrap());
        let wrapper = std::fs::read_to_string(home.command()).unwrap();
        assert!(
            wrapper.contains("omnigraph_cli") && !wrapper.contains("%*"),
            "{wrapper}"
        );
        assert!(
            home.use_cli_on(Path::new(r"C:\Tools\omnigraph.exe"), true)
                .unwrap()
        );
        assert!(
            std::fs::read_to_string(home.command())
                .unwrap()
                .contains("%*")
        );
        assert!(home.use_cli_on(unnamed, false).unwrap());
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
        let home = CliHome::write(&root, access(1, "t"), "knowledge").unwrap();
        home.refuse("No CLI: !y! ^c 100%").unwrap();
        let output = std::process::Command::new("cmd")
            .args(["/D", "/V:ON", "/C"])
            .arg(home.command())
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
            "foo": var("OMNIGRAPH_FOO"),
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
        let home = CliHome::write(&root, access(1, "t"), "knowledge").unwrap();
        assert!(home.command().ends_with("omnigraph.cmd"));
        let cli = std::env::current_exe().unwrap();
        assert!(home.use_cli(&cli).unwrap(), "the batch file names {cli:?}");
        let script = std::fs::read_to_string(home.command()).unwrap();
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
        let output = std::process::Command::new(home.command())
            .args(["--exact", STAND_IN_TEST, "--nocapture", "--", CLI])
            .args(given)
            .env(STAND_IN, "1")
            .env("OMNIGRAPH_TOKEN_PERSONAL", "secret")
            .env("OMNIGRAPH_BEARER_TOKEN", "secret")
            .env("omnigraph_foo", "secret")
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
        // `omnigraph_foo` is `OMNIGRAPH_FOO` to Windows.
        assert!(
            seen["token"].is_null() && seen["bearer"].is_null() && seen["foo"].is_null(),
            "{seen}"
        );
        assert_eq!(seen["kept"], "kept");
        assert_eq!(
            output.status.code(),
            Some(7),
            "the wrapper exits as the CLI does"
        );

        home.refuse("No CLI: \"x\" 100% & <a|b> ^c (d)").unwrap();
        let output = std::process::Command::new(home.command())
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
