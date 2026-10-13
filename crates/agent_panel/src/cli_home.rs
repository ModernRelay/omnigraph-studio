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

/// How long one candidate CLI may take to answer the check.
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);

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

    /// What the agent is told about this conversation's CLI: in its MCP instructions and by the
    /// `omnigraph_cli` tool.
    pub fn instructions(&self, cli_ready: Option<bool>) -> String {
        let command = self.command.to_string_lossy();
        let flags = format!("--server studio --graph {}", self.graph);
        match cli_ready {
            Some(false) => format!(
                "Studio found no omnigraph CLI that speaks this graph's HTTP API, so the CLI is \
                 not available in this conversation. Use the omnigraph_* tools. Your branch is \
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
                 sent\"). See AGENTS.md for the rules.",
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
         Tell the person, and use the `omnigraph_*` tools meanwhile.\n"
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

/// Tries each candidate through the conversation's gateway with a cheap read, and leaves the
/// wrapper pointing at the first that answers. `None` when none does.
pub async fn choose_cli(home: &CliHome, candidates: Vec<PathBuf>) -> Option<PathBuf> {
    for cli in candidates {
        if home.use_cli(&cli).is_err() {
            continue;
        }
        let check = smol::process::Command::new(&home.command)
            .args([
                "snapshot",
                "--server",
                "studio",
                "--graph",
                &home.graph,
                "--json",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .fuse();
        let timeout = smol::Timer::after(CHECK_TIMEOUT).fuse();
        futures::pin_mut!(check, timeout);
        let answered = futures::select! {
            status = check => status.is_ok_and(|status| status.success()),
            _ = timeout => false,
        };
        if answered {
            return Some(cli);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn expand_home(path: &str) -> PathBuf {
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
