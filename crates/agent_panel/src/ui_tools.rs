//! A conversation's way to Studio's UI tools (spec 032): the entry it is handed, the name it
//! gives Studio's server, and how the thread knows a call to that server by its title.
//!
//! The tools and the server that serves them live in `ui_tool_server`, a peer crate the panel
//! does not depend on. `studio` sets [`UiToolEndpoint`] from it; without that global a
//! conversation has no UI tools and nothing else changes.

use std::rc::Rc;

use gpui::{App, Global};

/// Where a conversation gets its entry to Studio's UI tools (spec 032, The panel's agent).
/// `studio` sets it, so the panel reaches the tool server without depending on it. Asking for
/// an entry is what starts the server: none runs for a person who starts no conversation.
#[derive(Clone)]
pub struct UiToolEndpoint(Rc<MakeEntry>);

/// Makes a conversation's entry for its window, by the window's id.
type MakeEntry = dyn Fn(u64, &mut App) -> Option<UiToolEntry>;

impl Global for UiToolEndpoint {}

impl UiToolEndpoint {
    pub fn new(entry: impl Fn(u64, &mut App) -> Option<UiToolEntry> + 'static) -> Self {
        Self(Rc::new(entry))
    }

    /// A new entry for a conversation in the window `window`, with a token of its own bound to
    /// that window; `None` when Studio serves no UI tools.
    pub fn entry_for(window: u64, cx: &mut App) -> Option<UiToolEntry> {
        let endpoint = cx.try_global::<Self>()?.clone();
        (endpoint.0)(window, cx)
    }
}

/// A conversation's way to the tool server: its address, a token bound to its window, and
/// which names are the server's tools. The token stops working when the entry is dropped, as
/// the conversation ends.
pub struct UiToolEntry {
    pub url: String,
    pub token: String,
    /// Whether a name is one of the server's tools: a call to anything else asks the person.
    pub is_tool: fn(&str) -> bool,
    revoke: Option<Box<dyn FnOnce()>>,
}

impl UiToolEntry {
    pub fn new(
        url: String,
        token: String,
        is_tool: fn(&str) -> bool,
        revoke: impl FnOnce() + 'static,
    ) -> Self {
        Self {
            url,
            token,
            is_tool,
            revoke: Some(Box::new(revoke)),
        }
    }
}

impl std::fmt::Debug for UiToolEntry {
    /// The token is never written out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiToolEntry")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Drop for UiToolEntry {
    fn drop(&mut self) {
        if let Some(revoke) = self.revoke.take() {
            revoke();
        }
    }
}

/// What every conversation's MCP server name starts with. The rest is chosen at random per
/// conversation ([`server_name`]), so a server of the person's own config, which the agent also
/// loads, cannot take its name.
pub const SERVER_PREFIX: &str = "studio_";

/// A conversation's server name from six random bytes: [`SERVER_PREFIX`] and six lowercase
/// letters or digits.
pub fn server_name(random: [u8; 6]) -> String {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let suffix: String = random
        .iter()
        .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
        .collect();
    format!("{SERVER_PREFIX}{suffix}")
}

/// Studio's server as a conversation's thread knows it, once the host has said it was passed:
/// the name it was passed under, and which names are its tools.
#[derive(Clone, Copy)]
pub(crate) struct StudioServer<'a> {
    pub name: &'a str,
    pub is_tool: fn(&str) -> bool,
}

/// The tool a call's title names when it is a call to the server named `server`: the adapters
/// title one `mcp__<server>__<tool>` (Claude) or `mcp.<server>.<tool>` (Codex).
pub(crate) fn studio_call<'a>(title: &'a str, server: StudioServer<'_>) -> Option<&'a str> {
    let tool = title
        .strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(server.name))
        .and_then(|rest| rest.strip_prefix("__"))
        .or_else(|| {
            title
                .strip_prefix("mcp.")
                .and_then(|rest| rest.strip_prefix(server.name))
                .and_then(|rest| rest.strip_prefix('.'))
        })?;
    let tool = tool.split_whitespace().next().unwrap_or(tool);
    (server.is_tool)(tool).then_some(tool)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in for the tool server's list of names.
    pub(crate) fn is_tool(name: &str) -> bool {
        matches!(
            name,
            "graph_open_type" | "graph_open_map" | "studio_windows"
        )
    }

    #[test]
    fn a_studio_call_is_known_by_either_title_and_its_own_name_only() {
        let name = server_name([0, 1, 25, 26, 35, 36]);
        assert_eq!(name, "studio_abz09a");
        let name = server_name([7, 4, 11, 11, 14, 35]);
        assert_eq!(name, "studio_hello9");
        let server = StudioServer {
            name: &name,
            is_tool,
        };
        assert_eq!(
            studio_call("mcp__studio_hello9__graph_open_type", server),
            Some("graph_open_type")
        );
        assert_eq!(
            studio_call("mcp.studio_hello9.studio_windows", server),
            Some("studio_windows")
        );
        assert_eq!(
            studio_call("mcp__studio_hello9__run_query", server),
            None,
            "not a tool"
        );
        // A server of the person's own config named `studio`, or another conversation's.
        assert_eq!(studio_call("mcp__studio__graph_open_type", server), None);
        assert_eq!(studio_call("mcp.studio.graph_open_type", server), None);
        assert_eq!(
            studio_call("mcp__studio_hello99__graph_open_type", server),
            None
        );
    }
}
