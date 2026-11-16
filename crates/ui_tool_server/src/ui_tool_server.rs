//! Studio's UI tool server (spec 032): MCP over Streamable HTTP on `127.0.0.1`, serving the UI
//! tools of `studio_workspace::ui_tools` to the panel's agents and, when the person turns it on,
//! to agents outside Studio. It makes no request to an Omnigraph server: a tool acts on a
//! window, and the views it opens read as they do when the person opens them.
//!
//! Each conversation gets a token of its own, bound to its window, while it runs
//! ([`UiToolServer::entry_for`]). The outside-agent switch gives out one more token for the
//! run, written with the server's address to `<data dir>/ui-tools/endpoint.json` (mode 0600)
//! while it is on, and deleted when it is turned off and at quit.
//!
//! Never logged: a token, a tool's input, node ids or tab titles. The log names the tool, the
//! window and the outcome.
//!
//! Layer 5. `studio` starts it and sets `UiToolEndpoint`, so the agent panel reaches it
//! without depending on it.

pub mod http;
pub mod mcp;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, Global, Task};
use serde_json::{Value, json};
use studio_settings::{Access, StudioSettings};
use studio_workspace::ui_tools::{self, Target, UiToolEntry};

use crate::http::{Shared, TokenState};

/// The name an outside agent's config gives Studio's server.
pub const OUTSIDE_NAME: &str = "omnigraph-studio";

/// The environment variable Codex reads the outside token from (`bearer_token_env_var`).
pub const TOKEN_VARIABLE: &str = "OMNIGRAPH_STUDIO_TOKEN";

/// The running server.
pub struct UiToolServer {
    port: u16,
    shared: Arc<Shared>,
    /// The outside agents' token for this run, made when the switch is first turned on.
    outside_token: Option<String>,
    _serve: Task<()>,
    _calls: Task<()>,
}

impl Global for UiToolServer {}

/// Starts the server and follows the outside-agent switch. A server that cannot bind leaves
/// Studio without UI tools, and says so in the log.
pub fn init(cx: &mut App) {
    match start(cx) {
        Ok(server) => {
            cx.set_global(server);
            follow_switch(cx);
            cx.observe_global::<StudioSettings>(follow_switch).detach();
            cx.on_app_quit(|cx| {
                // The token dies with the run, so its file goes with it.
                if cx.has_global::<UiToolServer>() {
                    remove_endpoint();
                }
                async {}
            })
            .detach();
        }
        Err(error) => log::warn!("Studio's UI tool server could not start: {error}"),
    }
}

fn start(cx: &mut App) -> std::io::Result<UiToolServer> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let listener = smol::net::TcpListener::try_from(listener)?;
    let (calls, mut incoming) = mpsc::unbounded::<http::Call>();
    let shared = Arc::new(Shared {
        tokens: Mutex::default(),
        tools_any: Value::Array(ui_tools::definitions(Target::Any, cx)),
        tools_bound: Value::Array(ui_tools::definitions(Target::Bound(0), cx)),
        calls,
    });
    let serve = cx.background_spawn(http::serve(listener, shared.clone()));
    // Calls run one at a time on the main thread, each in its window's two updates.
    let calls = cx.spawn(async move |cx| {
        while let Some(call) = incoming.next().await {
            let answer = ui_tools::call(cx, call.target, call.caller, &call.tool, call.input).await;
            call.reply.send(answer).ok();
        }
    });
    Ok(UiToolServer {
        port,
        shared,
        outside_token: None,
        _serve: serve,
        _calls: calls,
    })
}

impl UiToolServer {
    /// The server's MCP address.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// A conversation's entry: a new token bound to its window, given up when the entry is
    /// dropped. `None` when the server is not running or no token could be made.
    pub fn entry_for(window: u64, cx: &mut App) -> Option<UiToolEntry> {
        let server = cx.try_global::<Self>()?;
        let token = new_token().ok()?;
        let shared = server.shared.clone();
        shared
            .tokens
            .lock()
            .ok()?
            .insert(token.clone(), TokenState::new(Target::Bound(window), false));
        let revoked = token.clone();
        Some(UiToolEntry::new(server.url(), token, move || {
            if let Ok(mut tokens) = shared.tokens.lock() {
                tokens.remove(&revoked);
            }
        }))
    }

    /// Whether agents outside Studio may call the tools now.
    pub fn outside_on(cx: &App) -> bool {
        StudioSettings::get(cx).agent.outside_agents && cx.has_global::<Self>()
    }

    /// The address an outside agent connects to, while the switch is on.
    pub fn outside_url(cx: &App) -> Option<String> {
        Self::outside_on(cx)
            .then(|| cx.try_global::<Self>().map(Self::url))
            .flatten()
    }
}

/// Turns the outside agents' token and its file on or off as the setting says.
fn follow_switch(cx: &mut App) {
    let on = StudioSettings::get(cx).agent.outside_agents;
    let Some(server) = cx.try_global::<UiToolServer>() else {
        return;
    };
    let shared = server.shared.clone();
    let url = server.url();
    let held = server.outside_token.clone();
    match (on, held) {
        (true, Some(token)) => {
            let present = shared
                .tokens
                .lock()
                .is_ok_and(|tokens| tokens.contains_key(&token));
            if !present {
                accept_outside(&shared, &token);
                write_endpoint(&url, &token);
            }
        }
        (true, None) => {
            let Ok(token) = new_token() else {
                log::warn!("Studio could not make a token for agents outside Studio");
                return;
            };
            accept_outside(&shared, &token);
            write_endpoint(&url, &token);
            cx.global_mut::<UiToolServer>().outside_token = Some(token);
        }
        (false, Some(token)) => {
            let removed = shared
                .tokens
                .lock()
                .ok()
                .and_then(|mut tokens| tokens.remove(&token))
                .is_some();
            if removed {
                remove_endpoint();
            }
        }
        // Off from the start: a file an earlier run left goes.
        (false, None) => remove_endpoint(),
    }
}

fn accept_outside(shared: &Shared, token: &str) {
    if let Ok(mut tokens) = shared.tokens.lock() {
        // The toast that says an outside agent is here is shown once a run, so a token that
        // comes back keeps what it was told.
        tokens
            .entry(token.to_owned())
            .or_insert_with(|| TokenState::new(Target::Any, true));
    }
}

/// Where the outside agents' address and token are kept while the switch is on.
pub fn endpoint_dir() -> PathBuf {
    studio_settings::data_dir().join("ui-tools")
}

/// `endpoint.json`: `url` and `token`.
pub fn endpoint_file() -> PathBuf {
    endpoint_dir().join("endpoint.json")
}

/// The token alone, which the copied commands read with `cat`, so they never show it.
pub fn token_file() -> PathBuf {
    endpoint_dir().join("token")
}

fn write_endpoint(url: &str, token: &str) {
    let dir = endpoint_dir();
    let written = std::fs::create_dir_all(&dir)
        .and_then(|()| private_dir(&dir))
        .and_then(|()| {
            let endpoint = serde_json::to_vec_pretty(&json!({ "url": url, "token": token }))
                .unwrap_or_default();
            studio_settings::write_atomic(&endpoint_file(), &endpoint, Access::Private)
        })
        .and_then(|()| {
            studio_settings::write_atomic(&token_file(), token.as_bytes(), Access::Private)
        });
    if let Err(error) = written {
        log::warn!("Studio could not write the UI tools' endpoint file: {error}");
    }
}

#[cfg(unix)]
fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn private_dir(_dir: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

fn remove_endpoint() {
    for file in [endpoint_file(), token_file()] {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("Studio could not delete {}: {error}", file.display()),
        }
    }
}

/// A token: 32 random bytes, base64url without padding.
pub fn new_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(base64url(&bytes))
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let take = chunk.len() + 1;
        for index in 0..take {
            out.push(ALPHABET[((n >> (18 - 6 * index)) & 63) as usize] as char);
        }
    }
    out
}

/// A path as a POSIX shell reads it, in single quotes.
fn quoted(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// The command that adds Studio to Claude Code. It reads the token from its file, so copying it
/// never shows the token.
pub fn claude_command(url: &str) -> String {
    format!(
        "claude mcp add --transport http {OUTSIDE_NAME} {url} --header \"Authorization: Bearer \
         $(cat {})\"",
        quoted(&token_file())
    )
}

/// Codex's `config.toml` entry for Studio: the address, the variable the token is read from,
/// and an approval for each tool, since Codex otherwise asks before every call (spec 032).
pub fn codex_config(url: &str) -> String {
    let mut config = format!(
        "# Before starting Codex: export {TOKEN_VARIABLE}=\"$(cat {})\"\n\
         [mcp_servers.{OUTSIDE_NAME}]\n\
         url = \"{url}\"\n\
         bearer_token_env_var = \"{TOKEN_VARIABLE}\"\n",
        quoted(&token_file())
    );
    for tool in ui_tools::READS
        .iter()
        .copied()
        .chain(ui_tools::ALLOWED.iter().map(|tool| tool.name))
    {
        config.push_str(&format!("tools.{tool}.approval_mode = \"approve\"\n"));
    }
    config
}

#[cfg(test)]
mod tests;
