use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::tools::{Answer, Target};
use futures::channel::mpsc;
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use serde_json::{Value, json};
use smol::net::TcpStream;

use super::*;
use crate::http::{Call, MAX_BODY, MAX_IN_FLIGHT};
use graph_store::sign_in::MemoryKeychain;
use studio_settings::StudioSettings;

const TOKEN: &str = "token-of-the-test";

/// A server on a port of its own with one outside token, whose calls come out of the
/// receiver for the test to answer, or not.
fn server() -> (u16, mpsc::UnboundedReceiver<Call>, Arc<Shared>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let listener = smol::net::TcpListener::try_from(listener).unwrap();
    let (calls, incoming) = mpsc::unbounded();
    let shared = Arc::new(Shared {
        tokens: Mutex::new(HashMap::from([(
            TOKEN.to_owned(),
            TokenState::new(Target::Any, true),
        )])),
        tools_any: json!([{ "name": "studio_windows" }]),
        tools_bound: json!([{ "name": "studio_windows", "bound": true }]),
        calls,
    });
    smol::spawn(http::serve(listener, shared.clone())).detach();
    (port, incoming, shared)
}

/// Sends `raw` and reads the whole answer: its status, its head and its body.
async fn send(port: u16, raw: &[u8]) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(raw).await.unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.ok();
    let text = String::from_utf8_lossy(&answer).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head.to_ascii_lowercase(), body.to_owned())
}

fn post(body: &str, headers: &str) -> Vec<u8> {
    format!(
        "POST /mcp HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\n\
         accept: application/json, text/event-stream\r\n{headers}content-length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn authorized(body: &str) -> Vec<u8> {
    post(body, &format!("authorization: Bearer {TOKEN}\r\n"))
}

fn rpc(id: u64, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

/// A request without the token is refused before its body is read: the server answers though
/// the body never comes. A token that is not Studio's is refused the same way.
#[test]
fn a_request_without_the_token_gets_401_before_its_body_is_read() {
    smol::block_on(async {
        let (port, _calls, _) = server();
        let head = "POST /mcp HTTP/1.1\r\nhost: x\r\ncontent-length: 40000\r\n\r\n";
        let (status, headers, _) = smol::future::or(send(port, head.as_bytes()), async {
            smol::Timer::after(Duration::from_secs(5)).await;
            (0, String::new(), String::new())
        })
        .await;
        assert_eq!(status, 401, "answered without the body");
        assert!(headers.contains("www-authenticate: bearer"));
        let (status, _, _) = send(
            port,
            &post(
                &rpc(1, "ping", json!({})),
                "authorization: Bearer not-it\r\n",
            ),
        )
        .await;
        assert_eq!(status, 401);
    });
}

/// A web page's request carries an `Origin`; one is refused, even with the token. `null` and
/// none are what the agents send.
#[test]
fn a_request_from_a_web_page_gets_403() {
    smol::block_on(async {
        let (port, _calls, _) = server();
        let page = post(
            &rpc(1, "ping", json!({})),
            &format!("origin: http://rebound.example\r\nauthorization: Bearer {TOKEN}\r\n"),
        );
        assert_eq!(send(port, &page).await.0, 403);
        let null = post(
            &rpc(1, "ping", json!({})),
            &format!("origin: null\r\nauthorization: Bearer {TOKEN}\r\n"),
        );
        assert_eq!(send(port, &null).await.0, 200);
        assert_eq!(
            send(port, &authorized(&rpc(1, "ping", json!({})))).await.0,
            200
        );
    });
}

/// Claude Code opens `GET /mcp` for a server stream and carries on after a 405; the server
/// keeps no session, so DELETE gets the same, and no answer names a session.
#[test]
fn get_and_delete_get_405_with_allow_post() {
    smol::block_on(async {
        let (port, _calls, _) = server();
        for method in ["GET", "DELETE"] {
            let raw = format!(
                "{method} /mcp HTTP/1.1\r\nhost: x\r\naccept: text/event-stream\r\n\
                 authorization: Bearer {TOKEN}\r\n\r\n"
            );
            let (status, headers, _) = send(port, raw.as_bytes()).await;
            assert_eq!(status, 405, "{method}");
            assert!(headers.contains("allow: post"), "{headers}");
            assert!(!headers.contains("mcp-session-id"));
        }
        let elsewhere = format!(
            "POST /other HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {TOKEN}\r\n\
             content-length: 0\r\n\r\n"
        );
        assert_eq!(send(port, elsewhere.as_bytes()).await.0, 404);
    });
}

/// Claude Code's first message, `server/discover`, and any other method get -32601, and a
/// notification 202 with no body.
#[test]
fn other_methods_get_method_not_found_and_notifications_202() {
    smol::block_on(async {
        let (port, _calls, _) = server();
        for method in ["server/discover", "resources/list"] {
            let (status, _, body) = send(port, &authorized(&rpc(9, method, json!({})))).await;
            assert_eq!(status, 200);
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["error"]["code"], -32601, "{method}");
            assert_eq!(body["id"], 9);
        }
        let notification =
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        let (status, headers, body) = send(port, &authorized(&notification)).await;
        assert_eq!(status, 202);
        assert!(body.is_empty() && headers.contains("content-length: 0"));
    });
}

/// `initialize` echoes both clients' revisions and names the agent for the window's chip.
#[test]
fn initialize_answers_both_versions_and_names_the_agent() {
    smol::block_on(async {
        let (port, _calls, shared) = server();
        for (version, client) in [
            ("2025-06-18", "codex-mcp-client"),
            ("2025-11-25", "claude-code"),
        ] {
            let body = rpc(
                1,
                "initialize",
                json!({ "protocolVersion": version, "clientInfo": { "name": client } }),
            );
            let (status, headers, body) = send(port, &authorized(&body)).await;
            assert_eq!(status, 200);
            assert!(headers.contains("content-type: application/json"));
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["result"]["protocolVersion"], version);
            assert_eq!(body["result"]["serverInfo"]["name"], "omnigraph-studio");
        }
        let agent = shared.tokens.lock().unwrap()[TOKEN].agent.clone();
        assert_eq!(agent.as_deref(), Some("Claude Code"));
        let (_, _, body) = send(port, &authorized(&rpc(2, "tools/list", json!({})))).await;
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            body["result"]["tools"],
            json!([{ "name": "studio_windows" }])
        );
    });
}

/// A body past 64 KiB is refused unread, and an answer past it is not sent.
#[test]
fn a_body_or_an_answer_past_64_kib_is_refused() {
    smol::block_on(async {
        let (port, mut calls, _) = server();
        let large = format!(
            "POST /mcp HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {TOKEN}\r\n\
             content-length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(send(port, large.as_bytes()).await.0, 413);

        let call = rpc(
            3,
            "tools/call",
            json!({ "name": "studio_tabs", "arguments": {} }),
        );
        let answered = smol::spawn(async move { send(port, &authorized(&call)).await });
        let call = calls.next().await.unwrap();
        let mut answer = Answer::window_closed(1);
        answer.json["filler"] = json!("x".repeat(70 * 1024));
        call.reply.send(answer).ok();
        let (status, _, body) = answered.await;
        assert_eq!(status, 200);
        assert!(body.len() < 64 * 1024);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["id"], 3);
        assert_eq!(body["result"]["isError"], true);
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("larger than 64 KiB")
        );
    });
}

/// A token's fifth call while four wait is refused with `busy`; once one is answered, the next
/// goes through.
#[test]
fn a_fifth_call_in_flight_is_busy() {
    smol::block_on(async {
        let (port, mut calls, shared) = server();
        let call = |id: u64| {
            let body = rpc(
                id,
                "tools/call",
                json!({ "name": "studio_tabs", "arguments": {} }),
            );
            smol::spawn(async move { send(port, &authorized(&body)).await })
        };
        let waiting: Vec<_> = (1..=MAX_IN_FLIGHT as u64).map(call).collect();
        let mut held = Vec::new();
        for _ in 0..MAX_IN_FLIGHT {
            held.push(calls.next().await.unwrap());
        }
        let (_, _, body) = call(5).await;
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["result"]["isError"], true);
        let json: Value =
            serde_json::from_str(body["result"]["content"][1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(json["outcome"], "busy");

        // Each waiting call owed the notice when it was read; the main thread asks again just
        // before it runs one, and the first that a window shows it for marks it given.
        assert!(held.iter().all(|call| call.caller.announce));
        assert!(http::still_owed(&shared, &held[0]));
        http::announce_once(&shared, &held[0].token);
        assert!(!http::still_owed(&shared, &held[1]), "shown once");
        for call in held {
            call.reply.send(Answer::window_closed(7)).ok();
        }
        for answered in waiting {
            assert!(answered.await.2.contains("window_closed"));
        }
        let next = call(6);
        let sent = calls.next().await.unwrap();
        assert_eq!(sent.tool, "studio_tabs");
        assert!(!sent.caller.announce);
        sent.reply.send(Answer::window_closed(7)).ok();
        assert!(next.await.2.contains("window_closed"));
    });
}

/// The outside token is base64url of 32 random bytes, and the commands copied for Claude Code
/// and Codex read it from its file rather than show it.
#[test]
fn the_token_and_the_copied_commands() {
    let token = new_token().unwrap();
    assert_eq!(token.len(), 43);
    assert!(
        token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    );
    assert_ne!(token, new_token().unwrap());
    assert_eq!(base64url(b"\xfb\xff"), "-_8");
    assert_eq!(base64url(b"Man"), "TWFu");

    let url = "http://127.0.0.1:4100/mcp";
    let claude = claude_command(url);
    assert!(claude.starts_with(
        "claude mcp add --transport http omnigraph-studio http://127.0.0.1:4100/mcp --header \
         \"Authorization: Bearer $(cat '"
    ));
    assert!(claude.ends_with("token')\""));
    // Once run, the command's `$(cat …)` has put the token in Claude Code's own config: the
    // settings say where, and until when it works.
    assert!(claude_token_note(false).contains("~/.claude.json"));
    assert!(claude_token_note(false).contains("stops working when Studio starts again"));
    assert!(claude_token_note(true).contains("keeps working across restarts"));
    let codex = codex_config(url);
    assert!(codex.contains("[mcp_servers.omnigraph-studio]\nurl = \"http://127.0.0.1:4100/mcp\""));
    assert!(codex.contains("bearer_token_env_var = \"OMNIGRAPH_STUDIO_TOKEN\""));
    for tool in [
        "studio_windows",
        "graph_open_type",
        "workspace_activate_panel",
    ] {
        assert!(
            codex.contains(&format!("tools.{tool}.approval_mode = \"approve\"")),
            "{tool}"
        );
    }
}

/// The log names a call's tool only when it is one of Studio's: anything else an agent sends as
/// a name is its own text.
#[test]
fn the_log_names_only_studios_tools() {
    assert_eq!(http::logged_name("graph_open_type"), "graph_open_type");
    assert_eq!(http::logged_name("studio_windows"), "studio_windows");
    assert_eq!(
        http::logged_name("query people() { match { $p: Person } return { $p.email } }"),
        "unknown"
    );
    assert_eq!(http::logged_name("person-000042"), "unknown");
}

/// A call Studio stops before answering (the main thread let its reply go, as at quit) says so,
/// naming the window it meant, rather than that a window 0 is not open.
#[test]
fn a_call_studio_never_answered_is_cancelled() {
    smol::block_on(async {
        let (port, mut calls, _) = server();
        let body = rpc(
            8,
            "tools/call",
            json!({ "name": "graph_open_map", "arguments": { "window": 4294967297u64 } }),
        );
        let answered = smol::spawn(async move { send(port, &authorized(&body)).await });
        drop(calls.next().await.unwrap());
        let (_, _, body) = answered.await;
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["result"]["isError"], true);
        let json: Value =
            serde_json::from_str(body["result"]["content"][1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(json["outcome"], "cancelled");
        assert_eq!(json["window"], 4294967297u64);
    });
}

/// The outside token lasts the run, but its state goes with the switch: turned off, it is
/// refused and its file goes; turned on again, the same token is a new grant, whose first
/// call shows the notice again.
#[gpui::test]
fn the_outside_token_starts_afresh_when_the_switch_comes_back(cx: &mut gpui::TestAppContext) {
    let set = |on: bool, cx: &mut gpui::TestAppContext| {
        cx.update(|cx| {
            let mut settings = StudioSettings::get(cx).clone();
            settings.agent.outside_agents = on;
            cx.set_global(settings);
        });
    };
    cx.update(studio_settings::init_test);
    set(true, cx);
    cx.update(|cx| {
        let mut settings = StudioSettings::get(cx).clone();
        settings.agent.keep_address = false;
        cx.set_global(settings);
        init_with(Arc::new(MemoryKeychain::default()), cx);
    });
    let (token, shared) = cx.update(|cx| {
        let server = cx.global::<UiToolServer>();
        (server.outside_token.clone().unwrap(), server.shared.clone())
    });
    let state = |shared: &Shared| {
        shared
            .tokens
            .lock()
            .unwrap()
            .get(&token)
            .map(|state| state.announced)
    };
    http::announce_once(&shared, &token);
    assert_eq!(state(&shared), Some(true));
    set(false, cx);
    assert_eq!(state(&shared), None, "refused while off");
    set(true, cx);
    assert_eq!(state(&shared), Some(false), "the notice is owed again");
    cx.update(|cx| {
        assert_eq!(
            cx.global::<UiToolServer>().outside_token,
            Some(token.clone())
        );
    });
}

/// While the address is kept, the server asks for the port used last, then the one set and
/// the nine after it, passing over any another process holds, and only then takes one the
/// system chooses. Off, the system chooses.
#[test]
fn a_taken_port_is_passed_over_never_taken() {
    // Ten ports in a row the test holds itself, so no other process's port is relied on: each
    // step lets go of the one it expects the server to take.
    let (base, mut held) = (0..100)
        .find_map(|_| {
            let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?;
            let base = probe.local_addr().ok()?.port();
            drop(probe);
            if base > u16::MAX - PORT_TRIES {
                return None;
            }
            let held: Option<Vec<_>> = (0..PORT_TRIES)
                .map(|step| std::net::TcpListener::bind(("127.0.0.1", base + step)).ok())
                .collect();
            Some((base, held?))
        })
        .expect("ten free ports in a row");
    let mut free = |port: u16| {
        held[usize::from(port - base)] = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap()
    };
    // Free: it is taken.
    free(base);
    let first = bind(true, None, base).unwrap();
    assert_eq!(first.local_addr().unwrap().port(), base);
    // Held (by the first): the next free one.
    free(base + 1);
    let second = bind(true, None, base).unwrap();
    assert_eq!(second.local_addr().unwrap().port(), base + 1);
    // The port used last comes before the one set.
    free(base + 5);
    let last = bind(true, Some(base + 5), base).unwrap();
    assert_eq!(last.local_addr().unwrap().port(), base + 5);
    // All ten held: the system chooses, outside them; never one of another's.
    let chosen = bind(true, None, base).unwrap().local_addr().unwrap().port();
    assert!(!(base..base + PORT_TRIES).contains(&chosen), "{chosen}");
    // Not kept: the system chooses.
    let any = bind(false, None, base)
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert!(!(base..base + PORT_TRIES).contains(&any), "{any}");
    drop((first, second, last));
}

/// Until an agent is to be served nothing runs: no server, no port, no token, and the keychain
/// is not asked. A conversation's entry starts the server and still asks the keychain nothing;
/// letting agents outside Studio in is what reads the kept token.
#[gpui::test]
async fn nothing_starts_until_an_agent_is_served(cx: &mut gpui::TestAppContext) {
    use std::sync::atomic::Ordering;
    cx.update(studio_settings::init_test);
    let keychain = Arc::new(MemoryKeychain::default());
    let set = |change: fn(&mut studio_settings::AgentSettings), cx: &mut gpui::TestAppContext| {
        cx.update(|cx| {
            let mut settings = StudioSettings::get(cx).clone();
            change(&mut settings.agent);
            cx.set_global(settings);
        });
    };
    set(
        |agent| {
            agent.outside_agents = false;
            agent.keep_address = true;
            agent.ui_tools_port = 0;
        },
        cx,
    );
    let chain: Arc<dyn graph_store::sign_in::Keychain> = keychain.clone();
    cx.update(|cx| init_with(chain, cx));
    cx.run_until_parked();
    let asked = |keychain: &MemoryKeychain| {
        keychain.reads.load(Ordering::SeqCst) + keychain.writes.load(Ordering::SeqCst)
    };
    assert!(!cx.update(|cx| is_running(cx)), "no server");
    assert!(cx.update(|cx| UiToolServer::outside_url(cx)).is_none());
    assert_eq!(asked(&keychain), 0, "the keychain is not asked");

    // A setting that lets no agent in starts nothing.
    set(|agent| agent.raise_window = true, cx);
    set(|agent| agent.keep_address = false, cx);
    set(|agent| agent.keep_address = true, cx);
    cx.run_until_parked();
    assert!(!cx.update(|cx| is_running(cx)));
    assert_eq!(asked(&keychain), 0);

    // A conversation asks for its entry: the server starts for it.
    let entry = cx
        .update(|cx| UiToolServer::entry_for(7, cx))
        .expect("the server starts for a conversation");
    cx.run_until_parked();
    assert!(cx.update(|cx| is_running(cx)));
    assert!(entry.url.starts_with("http://127.0.0.1:"), "{}", entry.url);
    let (outside, shared) = cx.update(|cx| UiToolServer::outside(cx));
    assert!(outside.is_none(), "no token for agents outside Studio");
    assert_eq!(shared.tokens.lock().unwrap().len(), 1, "the conversation's");
    assert!(cx.update(|cx| UiToolServer::outside_url(cx)).is_none());
    assert_eq!(asked(&keychain), 0, "a conversation's token is not kept");
    drop(entry);
    assert!(shared.tokens.lock().unwrap().is_empty(), "given up");

    // Agents outside Studio are let in: now the kept token is read; none, so one is kept.
    set(|agent| agent.outside_agents = true, cx);
    cx.run_until_parked();
    assert_eq!(keychain.reads.load(Ordering::SeqCst), 1);
    let (outside, _) = cx.update(|cx| UiToolServer::outside(cx));
    let kept = keychain.items.lock().unwrap()["omnigraph-studio://ui-tools"]
        .1
        .clone();
    assert_eq!(Some(kept), outside.map(String::into_bytes));
    assert!(cx.update(|cx| UiToolServer::outside_url(cx)).is_some());
}

/// Kept, the token is read from the keychain, or made and kept there; New token replaces it
/// there and refuses the old one; turned off, the kept token goes and the run takes its own.
#[gpui::test]
async fn the_kept_token_lives_in_the_keychain(cx: &mut gpui::TestAppContext) {
    cx.update(studio_settings::init_test);
    let keychain = Arc::new(MemoryKeychain::default());
    keychain.items.lock().unwrap().insert(
        "omnigraph-studio://ui-tools".into(),
        ("studio".into(), b"kept-token".to_vec()),
    );
    let set = |change: fn(&mut studio_settings::AgentSettings), cx: &mut gpui::TestAppContext| {
        cx.update(|cx| {
            let mut settings = StudioSettings::get(cx).clone();
            change(&mut settings.agent);
            cx.set_global(settings);
        });
    };
    set(
        |agent| {
            agent.outside_agents = true;
            agent.keep_address = true;
            agent.ui_tools_port = 0;
        },
        cx,
    );
    let chain: Arc<dyn graph_store::sign_in::Keychain> = keychain.clone();
    cx.update(|cx| init_with(chain, cx));
    cx.run_until_parked();
    let (token, shared) = cx.update(|cx| UiToolServer::outside(cx));
    assert_eq!(
        token.as_deref(),
        Some("kept-token"),
        "read from the keychain"
    );
    assert!(shared.tokens.lock().unwrap().contains_key("kept-token"));

    cx.update(UiToolServer::replace_token);
    cx.run_until_parked();
    let (new, shared) = cx.update(|cx| UiToolServer::outside(cx));
    let new = new.unwrap();
    assert_ne!(new, "kept-token");
    assert!(
        !shared.tokens.lock().unwrap().contains_key("kept-token"),
        "refused"
    );
    assert!(shared.tokens.lock().unwrap().contains_key(&new));
    let kept = keychain.items.lock().unwrap()["omnigraph-studio://ui-tools"]
        .1
        .clone();
    assert_eq!(kept, new.as_bytes(), "kept in its place");

    set(|agent| agent.keep_address = false, cx);
    cx.run_until_parked();
    let (run, shared) = cx.update(|cx| UiToolServer::outside(cx));
    let run = run.unwrap();
    assert_ne!(run, new, "a token for the run");
    assert!(!shared.tokens.lock().unwrap().contains_key(&new));
    assert!(
        !keychain
            .items
            .lock()
            .unwrap()
            .contains_key("omnigraph-studio://ui-tools"),
        "no longer kept"
    );
}

/// Turned on while the address is kept, the outside agents' first token is kept too, so the
/// command copied now works after a restart; and the port in use is the one asked for next.
#[gpui::test]
async fn a_token_made_while_keeping_is_kept(cx: &mut gpui::TestAppContext) {
    cx.update(studio_settings::init_test);
    let keychain = Arc::new(MemoryKeychain::default());
    let set = |change: fn(&mut studio_settings::AgentSettings), cx: &mut gpui::TestAppContext| {
        cx.update(|cx| {
            let mut settings = StudioSettings::get(cx).clone();
            change(&mut settings.agent);
            cx.set_global(settings);
        });
    };
    set(
        |agent| {
            agent.outside_agents = false;
            agent.keep_address = false;
            agent.ui_tools_port = 0;
        },
        cx,
    );
    let chain: Arc<dyn graph_store::sign_in::Keychain> = keychain.clone();
    cx.update(|cx| init_with(chain, cx));
    cx.run_until_parked();
    set(|agent| agent.keep_address = true, cx);
    cx.run_until_parked();
    set(|agent| agent.outside_agents = true, cx);
    cx.run_until_parked();
    let (token, _) = cx.update(|cx| UiToolServer::outside(cx));
    let kept = keychain.items.lock().unwrap()["omnigraph-studio://ui-tools"]
        .1
        .clone();
    assert_eq!(
        Some(kept),
        token.map(String::into_bytes),
        "kept as it was made"
    );
}
