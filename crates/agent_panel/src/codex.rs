//! Codex runs unsandboxed, as Claude does (spec 013). codex-acp starts every session, new or
//! loaded, in a mode whose sandbox has no network, so its shell cannot reach the gateway, and it
//! ignores the sandbox in Codex's own configuration. It reads its starting mode from
//! `INITIAL_AGENT_MODE`, which Studio sets to full access when it starts codex-acp, unless the
//! agent's settings set one. A mode chosen in the panel still applies to its conversation.

use std::collections::BTreeMap;

/// The variable codex-acp reads its starting mode from.
const INITIAL_MODE: &str = "INITIAL_AGENT_MODE";
/// codex-acp's mode without a sandbox.
const FULL_ACCESS: &str = "agent-full-access";

/// Starts codex-acp without a sandbox when `command` runs it, unless `env` sets a mode.
pub fn unsandbox(command: &[String], env: &mut BTreeMap<String, String>) {
    if runs_codex_acp(command) {
        env.entry(INITIAL_MODE.to_owned())
            .or_insert_with(|| FULL_ACCESS.to_owned());
    }
}

/// Whether one of the command's words is codex-acp: a package (`@agentclientprotocol/codex-acp`,
/// with or without a version) or a program of that name.
fn runs_codex_acp(command: &[String]) -> bool {
    command.iter().any(|word| {
        let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
        let name = name.split('@').next().unwrap_or(name);
        let name = name
            .strip_suffix(".cmd")
            .or_else(|| name.strip_suffix(".exe"))
            .unwrap_or(name);
        name == "codex-acp"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &[&str]) -> Vec<String> {
        command.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn codex_acp_is_known_by_its_package_or_program() {
        for command in [
            &["npx", "-y", "@agentclientprotocol/codex-acp@latest"][..],
            &["npx", "@zed-industries/codex-acp"],
            &["/opt/bin/codex-acp"],
            &[r"C:\tools\codex-acp.cmd"],
        ] {
            assert!(runs_codex_acp(&words(command)), "{command:?}");
        }
        for command in [
            &["npx", "-y", "@agentclientprotocol/claude-agent-acp@latest"][..],
            &["codex"],
            &["codex-acp-wrapper"],
        ] {
            assert!(!runs_codex_acp(&words(command)), "{command:?}");
        }
    }

    #[test]
    fn codex_starts_unsandboxed_unless_its_settings_say_otherwise() {
        let codex = words(&["npx", "-y", "@agentclientprotocol/codex-acp@latest"]);
        let mut env = BTreeMap::new();
        unsandbox(&codex, &mut env);
        assert_eq!(env.get(INITIAL_MODE).map(String::as_str), Some(FULL_ACCESS));

        let mut env = BTreeMap::from([(INITIAL_MODE.to_owned(), "read-only".to_owned())]);
        unsandbox(&codex, &mut env);
        assert_eq!(env.get(INITIAL_MODE).map(String::as_str), Some("read-only"));

        let mut env = BTreeMap::new();
        unsandbox(&words(&["claude-agent-acp"]), &mut env);
        assert!(env.is_empty());
    }
}
