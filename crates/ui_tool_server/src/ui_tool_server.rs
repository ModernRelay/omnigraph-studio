//! Studio's UI tool server (spec 032): MCP over Streamable HTTP on `127.0.0.1`, serving the UI
//! tools of `studio_workspace::ui_tools` to the panel's agents and, when the person turns it on,
//! to agents outside Studio. It makes no request to an Omnigraph server: a tool acts on a
//! window, and the views it opens read as they do when the person opens them.
//!
//! Each conversation gets a token of its own, bound to its window, while it runs
//! ([`UiToolServer::entry_for`]). The outside-agent switch gives out one more token, written
//! with the server's address to `<data dir>/ui-tools/endpoint.json` (mode 0600, in a 0700
//! folder) while it is on and deleted when it is turned off. While Studio keeps its address
//! (on by default), the server asks for the same port each start ([`bind`],
//! `DEFAULT_UI_TOOLS_PORT`) and the token is kept in the system keychain, so a copied command
//! keeps working; otherwise both are made per run and the file goes at quit.
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
use graph_store::sign_in::{Keychain, PlatformKeychain};
use serde_json::{Value, json};
use studio_settings::{Access, StudioSettings};
use studio_workspace::ui_tools::{self, Target, UiToolEntry};

use crate::http::{Shared, TokenState};

/// The name an outside agent's config gives Studio's server.
pub const OUTSIDE_NAME: &str = "omnigraph-studio";

/// The environment variable Codex reads the outside token from (`bearer_token_env_var`).
pub const TOKEN_VARIABLE: &str = "OMNIGRAPH_STUDIO_TOKEN";

/// How many ports from the one asked for are tried before the system chooses one.
pub const PORT_TRIES: u16 = 10;

/// The keychain item the kept token is held under.
const KEYCHAIN_KEY: &str = "omnigraph-studio://ui-tools";
const KEYCHAIN_USER: &str = "studio";

/// The running server.
pub struct UiToolServer {
    port: u16,
    shared: Arc<Shared>,
    keychain: Arc<dyn Keychain>,
    /// The outside agents' token: kept in the keychain while the address is kept, else made
    /// for the run when the switch is first turned on.
    outside_token: Option<String>,
    /// The kept token is being read from the keychain; the switch waits for it.
    reading: bool,
    /// Whether the address is kept, as last followed.
    keeping: bool,
    /// The keychain's last change of the kept token, which the next one waits for ([`keep`]).
    keeping_writes: Option<Task<()>>,
    /// What the settings row says about the address or token, when something did not go as
    /// asked ("Port 24187 was taken…").
    note: Option<String>,
    _serve: Task<()>,
    _calls: Task<()>,
}

impl Global for UiToolServer {}

/// Starts the server, with the system keychain for a kept token, and follows the settings. A
/// server that cannot bind leaves Studio without UI tools, and says so in the log.
pub fn init(cx: &mut App) {
    let keychain = PlatformKeychain::start(cx);
    init_with(keychain, cx);
}

/// [`init`] with the keychain given: a test's keeps its items in memory.
pub fn init_with(keychain: Arc<dyn Keychain>, cx: &mut App) {
    match start(keychain, cx) {
        Ok(server) => {
            let keeping = server.keeping;
            cx.set_global(server);
            if keeping {
                read_kept_token(cx);
            }
            follow_settings(cx);
            cx.observe_global::<StudioSettings>(follow_settings)
                .detach();
            cx.on_app_quit(|cx| {
                // A token made for the run dies with it, so its file goes with it; a kept one's
                // stays for the copied commands. After the writes asked for before it.
                if let Some(server) = cx.try_global::<UiToolServer>()
                    && !server.keeping
                {
                    remove_endpoint();
                    studio_settings::writes_done();
                }
                // A change of the kept token still out lands before Studio goes, as far as
                // the quit waits: dropped with the server, it would be lost.
                let pending = cx
                    .try_global::<UiToolServer>()
                    .is_some()
                    .then(|| cx.global_mut::<UiToolServer>().keeping_writes.take())
                    .flatten();
                async move {
                    if let Some(pending) = pending {
                        pending.await;
                    }
                }
            })
            .detach();
        }
        Err(error) => log::warn!("Studio's UI tool server could not start: {error}"),
    }
}

/// Binds to `127.0.0.1`. While the address is kept, the ports asked for come first, in order:
/// the one used last, then `preferred` and the [`PORT_TRIES`] - 1 after it; a port another
/// process holds is passed over, never taken from it. Then a port the system chooses.
pub fn bind(
    keep: bool,
    last: Option<u16>,
    preferred: u16,
) -> std::io::Result<std::net::TcpListener> {
    if keep {
        // A port of 0 asks the system for one.
        let range = if preferred == 0 { 0 } else { PORT_TRIES };
        let asked = last
            .into_iter()
            .chain((0..range).filter_map(|step| preferred.checked_add(step)))
            .filter(|port| *port != 0);
        for port in asked {
            if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                return Ok(listener);
            }
        }
    }
    std::net::TcpListener::bind(("127.0.0.1", 0))
}

/// The port the server took last, while it kept its address.
fn last_port_file() -> PathBuf {
    endpoint_dir().join("port")
}

fn start(keychain: Arc<dyn Keychain>, cx: &mut App) -> std::io::Result<UiToolServer> {
    let settings = &StudioSettings::get(cx).agent;
    let (keep, preferred) = (settings.keep_address, settings.ui_tools_port);
    let last = keep
        .then(|| std::fs::read_to_string(last_port_file()).ok())
        .flatten()
        .and_then(|text| text.trim().parse::<u16>().ok());
    let listener = bind(keep, last, preferred)?;
    let port = listener.local_addr()?.port();
    let asked = (keep && port != preferred && Some(port) != last).then_some(preferred);
    if keep {
        studio_settings::write_atomic_in_turn(
            last_port_file(),
            port.to_string().into_bytes(),
            Access::Private,
        );
    }
    let listener = smol::net::TcpListener::try_from(listener)?;
    let (calls, mut incoming) = mpsc::unbounded::<http::Call>();
    let shared = Arc::new(Shared {
        tokens: Mutex::default(),
        tools_any: Value::Array(ui_tools::definitions(Target::Any, cx)),
        tools_bound: Value::Array(ui_tools::definitions(Target::Bound(0), cx)),
        calls,
    });
    let serve = cx.background_spawn(http::serve(listener, shared.clone()));
    // Calls run one at a time on the main thread, each in its window's two updates, so the
    // outside notice is checked and marked here, once.
    let calls = {
        let shared = shared.clone();
        cx.spawn(async move |cx| {
            while let Some(mut call) = incoming.next().await {
                call.caller.announce = http::still_owed(&shared, &call);
                let answer =
                    ui_tools::call(cx, call.target, call.caller, &call.tool, call.input).await;
                if answer.announced {
                    http::announce_once(&shared, &call.token);
                }
                call.reply.send(answer).ok();
            }
        })
    };
    Ok(UiToolServer {
        port,
        shared,
        keychain,
        outside_token: None,
        reading: false,
        keeping: keep,
        keeping_writes: None,
        note: asked.map(|asked| {
            format!(
                "Port {asked} was taken, so Studio uses {port} this time: copy the command again."
            )
        }),
        _serve: serve,
        _calls: calls,
    })
}

/// Reads the kept token, or makes and keeps one; the switch waits for it.
fn read_kept_token(cx: &mut App) {
    let server = cx.global_mut::<UiToolServer>();
    server.reading = true;
    let keychain = server.keychain.clone();
    let read = keychain.read(KEYCHAIN_KEY.to_owned());
    cx.spawn(async move |cx| {
        let kept = read.await;
        let (token, note) = match kept {
            Ok(Some((_, secret))) => match String::from_utf8(secret) {
                Ok(token) if !token.is_empty() => (Some(token), None),
                _ => (None, None),
            },
            Ok(None) => (None, None),
            Err(error) => {
                log::warn!("the keychain did not answer for the UI tools' token: {error}");
                (
                    None,
                    Some("The system keychain did not answer, so the token lasts this run."),
                )
            }
        };
        cx.update(|cx| {
            if !cx.has_global::<UiToolServer>() {
                return;
            }
            let server = cx.global_mut::<UiToolServer>();
            server.reading = false;
            if let Some(note) = note {
                server.note = Some(note.to_owned());
            }
            // New token pressed while the keychain was read: the new one stands, and is the
            // one kept. Else the kept one, or one made now and kept, unless the keychain
            // cannot be reached.
            let mut made = None;
            if server.outside_token.is_none() {
                let kept = token.is_some();
                server.outside_token = token.or_else(|| new_token().ok());
                if !kept && note.is_none() {
                    made = server.outside_token.clone();
                }
            }
            if made.is_some() {
                keep(cx, made);
            }
            follow_settings(cx);
        });
    })
    .detach();
}

impl UiToolServer {
    /// The server's MCP address.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// The port the server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// What the settings row says about the address or token, when anything.
    pub fn note(cx: &App) -> Option<String> {
        cx.try_global::<Self>()?.note.clone()
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

    /// Replaces the outside agents' token: every agent holding the old one is refused from
    /// now on, and the files and the kept token hold the new one.
    pub fn replace_token(cx: &mut App) {
        let Some(server) = cx.try_global::<Self>() else {
            return;
        };
        let Ok(token) = new_token() else {
            log::warn!("Studio could not make a token for agents outside Studio");
            return;
        };
        if let Some(old) = &server.outside_token
            && let Ok(mut tokens) = server.shared.tokens.lock()
        {
            tokens.remove(old);
        }
        if server.keeping {
            keep(cx, Some(token.clone()));
        }
        cx.global_mut::<Self>().outside_token = Some(token);
        follow_settings(cx);
    }

    /// For tests: the outside token and the tokens' states.
    #[cfg(test)]
    fn outside(cx: &App) -> (Option<String>, Arc<Shared>) {
        let server = cx.global::<Self>();
        (server.outside_token.clone(), server.shared.clone())
    }
}

/// Keeps `token` in the keychain, or deletes the kept one (`None`), after every change of it
/// asked for before: two writes out at once could land in either order and leave a token kept
/// that was replaced.
fn keep(cx: &mut App, token: Option<String>) {
    let server = cx.global_mut::<UiToolServer>();
    let keychain = server.keychain.clone();
    let before = server.keeping_writes.take();
    let write = cx.background_spawn(async move {
        if let Some(before) = before {
            before.await;
        }
        let done = match token {
            Some(token) => {
                keychain
                    .write(
                        KEYCHAIN_KEY.to_owned(),
                        KEYCHAIN_USER.to_owned(),
                        token.into_bytes(),
                    )
                    .await
            }
            None => keychain.delete(KEYCHAIN_KEY.to_owned()).await,
        };
        if let Err(error) = done {
            log::warn!("the keychain did not keep the UI tools' token: {error}");
        }
    });
    cx.global_mut::<UiToolServer>().keeping_writes = Some(write);
}

/// Follows the settings: the outside agents' token and its file on or off, and whether the
/// token is kept. A port asked for applies at the next start.
fn follow_settings(cx: &mut App) {
    let settings = StudioSettings::get(cx).agent.clone();
    let Some(server) = cx.try_global::<UiToolServer>() else {
        return;
    };
    if server.reading {
        return;
    }
    if settings.keep_address != server.keeping {
        if settings.keep_address {
            // Kept from now: the port in use is the one the next start asks for first.
            studio_settings::write_atomic_in_turn(
                last_port_file(),
                server.port.to_string().into_bytes(),
                Access::Private,
            );
            // Kept from now: the token held goes in the keychain.
            if let Some(token) = server.outside_token.clone() {
                keep(cx, Some(token));
            }
            cx.global_mut::<UiToolServer>().keeping = true;
        } else {
            // No longer kept: the kept token goes, and the run takes one of its own.
            keep(cx, None);
            cx.global_mut::<UiToolServer>().keeping = false;
            if cx.global::<UiToolServer>().outside_token.is_some() {
                UiToolServer::replace_token(cx);
                return;
            }
        }
    }
    let server = cx.global::<UiToolServer>();
    let on = settings.outside_agents;
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
            // While the address is kept, a token made now is kept too.
            if server.keeping {
                keep(cx, Some(token.clone()));
            }
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
        // Turning the switch off removes the token's state, so turned on again it starts
        // afresh: a new grant, whose first call shows the notice again.
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

/// Writes the endpoint's files on Studio's write thread, after every write asked for before
/// it: a sync to disk is too slow for the main thread.
fn write_endpoint(url: &str, token: &str) {
    let endpoint =
        serde_json::to_vec_pretty(&json!({ "url": url, "token": token })).unwrap_or_default();
    let token = token.as_bytes().to_vec();
    studio_settings::run_in_turn(move || {
        let dir = endpoint_dir();
        let written = std::fs::create_dir_all(&dir)
            .and_then(|()| private_dir(&dir))
            .and_then(|()| {
                studio_settings::write_atomic(&endpoint_file(), &endpoint, Access::Private)
            })
            .and_then(|()| studio_settings::write_atomic(&token_file(), &token, Access::Private));
        if let Err(error) = written {
            log::warn!("Studio could not write the UI tools' endpoint file: {error}");
        }
    });
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

/// Deletes the endpoint's files on the write thread, after a write of them asked for before.
fn remove_endpoint() {
    studio_settings::run_in_turn(remove_endpoint_now);
}

fn remove_endpoint_now() {
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

/// What the settings say under the Claude Code command: `$(cat …)` expands when the person runs
/// it, so Claude Code keeps the token itself, until the token stops working. While Studio keeps
/// its address, the command keeps working across restarts.
pub fn claude_token_note(keeping: bool) -> &'static str {
    if keeping {
        "Claude Code keeps the token in ~/.claude.json once the command runs. It keeps working \
         across restarts, until New token replaces it or this is turned off."
    } else {
        "Claude Code keeps the token in ~/.claude.json once the command runs; it stops working \
         when Studio starts again or this is turned off."
    }
}

/// Whether Studio keeps its address and token between starts now.
pub fn keeping(cx: &App) -> bool {
    cx.try_global::<UiToolServer>()
        .is_some_and(|server| server.keeping)
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
