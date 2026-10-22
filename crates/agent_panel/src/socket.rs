//! The tool server's socket (spec 011). The agent starts `omnigraph-studio mcp-proxy <socket>`
//! as a stdio MCP server; the proxy connects here and copies bytes both ways. Studio answers
//! MCP itself and sends tool calls to the panel, which runs them with the omnigraph CLI.
//!
//! The socket lives in a short per-user runtime directory only the person can enter (`0700`),
//! so no other local user can reach it, and it is removed when the agent session ends. Unix
//! socket paths are limited to about 104 bytes on macOS, which Studio's data directory can
//! exceed, so the socket does not live there. Windows 10 and later have Unix sockets too
//! (`uds_windows`); there the directory is in the person's own temporary folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::channel::{mpsc, oneshot};
use futures::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, FutureExt,
    StreamExt as _,
};
use serde_json::Value;
use smol::io::BufReader;

use crate::mcp::{self, Incoming};

/// A tool call for the panel to run.
pub struct ToolRequest {
    pub name: String,
    pub arguments: Value,
    /// The result text, and whether it is an error.
    pub reply: oneshot::Sender<(String, bool)>,
    /// Answers when the agent cancels the call (`notifications/cancelled`); dropped, without an
    /// answer, once the call is answered.
    pub cancelled: oneshot::Receiver<()>,
}

/// A bound socket. Dropping it removes the socket file.
pub struct ToolSocket {
    pub path: PathBuf,
    listener: Option<sys::Listener>,
}

/// Unix sockets as each system offers them: an accepted connection's two halves, and a
/// blocking stream for the proxy.
#[cfg(unix)]
mod sys {
    use std::path::Path;

    pub use smol::net::unix::UnixListener as Listener;
    pub use std::os::unix::net::UnixStream as BlockingStream;

    pub fn bind(path: &Path) -> std::io::Result<Listener> {
        Listener::bind(path)
    }

    pub async fn accept(
        listener: &Listener,
    ) -> std::io::Result<(smol::net::unix::UnixStream, smol::net::unix::UnixStream)> {
        let (stream, _) = listener.accept().await?;
        Ok((stream.clone(), stream))
    }
}

#[cfg(windows)]
mod sys {
    use std::path::Path;
    use std::time::Duration;

    use smol::Unblock;
    pub use uds_windows::UnixStream as BlockingStream;

    /// `uds_windows`'s sockets are not ones the reactor can wait on, so the listener is polled
    /// (the agent connects once, at its start) and a connection's halves read and write on
    /// blocking threads.
    pub struct Listener(uds_windows::UnixListener);

    pub fn bind(path: &Path) -> std::io::Result<Listener> {
        let listener = uds_windows::UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Listener(listener))
    }

    pub async fn accept(
        listener: &Listener,
    ) -> std::io::Result<(Unblock<BlockingStream>, Unblock<BlockingStream>)> {
        loop {
            match listener.0.accept() {
                Ok((stream, _)) => {
                    // An accepted socket takes the listener's non-blocking mode.
                    stream.set_nonblocking(false)?;
                    let reader = stream.try_clone()?;
                    return Ok((Unblock::new(reader), Unblock::new(stream)));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    smol::Timer::after(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for ToolSocket {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// How often a running tool call reports progress.
const PROGRESS_EVERY: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(50)
} else {
    std::time::Duration::from_secs(60)
};

/// Longest socket path Studio binds: macOS allows 104 bytes including the terminator.
const MAX_SOCKET_PATH: usize = 100;

/// Where tool sockets live: the per-user temporary directory on macOS and Windows (already
/// private to the person), `$XDG_RUNTIME_DIR` on Linux, else a private directory under the
/// temporary directory.
pub fn socket_dir() -> PathBuf {
    if cfg!(any(target_os = "macos", windows)) {
        return std::env::temp_dir().join("omnigraph-studio");
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(runtime).join("omnigraph-studio");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    std::env::temp_dir().join(format!("omnigraph-studio-{user}"))
}

/// Creates `dir` with mode `0700`, or accepts it only if it is a real directory owned by this
/// user with mode `0700`. A shared temporary directory could hold a directory or symlink
/// someone else planted.
#[cfg(unix)]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(dir)?;
    // A file this process creates is owned by this process's user. The name is unique per
    // call: two sockets bound at once must not remove each other's probe.
    let probe = dir.join(format!(
        ".owner-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&probe, b"")?;
    let me = std::fs::metadata(&probe)?.uid();
    std::fs::remove_file(&probe).ok();
    if !metadata.is_dir() || metadata.uid() != me || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(std::io::Error::other(format!(
            "{} is not a private directory of this user",
            dir.display()
        )));
    }
    Ok(())
}

/// The person's temporary folder on Windows is under their profile, which only they can open;
/// the directory inherits that.
#[cfg(windows)]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if !std::fs::symlink_metadata(dir)?.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    Ok(())
}

impl ToolSocket {
    /// Binds a fresh socket in `dir`, which must be (or becomes) private to this user.
    pub fn bind(dir: &Path) -> std::io::Result<Self> {
        private_dir(dir)?;
        let path = dir.join(format!(
            "{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(std::io::Error::other(format!(
                "the socket path {} is longer than {MAX_SOCKET_PATH} bytes",
                path.display()
            )));
        }
        std::fs::remove_file(&path).ok();
        let listener = sys::bind(&path)?;
        Ok(Self {
            path,
            listener: Some(listener),
        })
    }

    /// Serves connections until the returned future is dropped. `instructions` are this
    /// conversation's, appended to the server's in `initialize` as they are when it arrives.
    pub fn serve(
        &mut self,
        tools: mpsc::UnboundedSender<ToolRequest>,
        instructions: Instructions,
    ) -> impl Future<Output = ()> + Send + 'static {
        let listener = self.listener.take();
        async move {
            let Some(listener) = listener else { return };
            serve_each(
                || sys::accept(&listener),
                |(reader, writer), standing: Standing| {
                    // Only the person can enter the socket's directory, so a peer that reached
                    // it proved itself by doing so.
                    standing.prove();
                    serve_connection(reader, writer, tools.clone(), instructions.clone())
                },
            )
            .await;
        }
    }
}

/// A conversation's instructions, which `initialize` appends to the server's. They say what the
/// conversation's CLI is and whether the agent's shell can run it, so they are known only once
/// Studio has chosen the CLI: the panel sends them then. Dropped unsent (the conversation
/// ended), there are none.
pub type Instructions = futures::future::Shared<oneshot::Receiver<String>>;

/// How long `initialize` waits for a conversation's instructions before it answers that Studio
/// is still choosing the CLI: a CLI that answers its version check is chosen well within it,
/// and an agent's MCP server should answer its start promptly.
const INSTRUCTIONS_WITHIN: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(200)
} else {
    std::time::Duration::from_secs(5)
};

/// The conversation's instructions as `initialize` gives them: what Studio chose, once it has;
/// while it is still choosing after [`INSTRUCTIONS_WITHIN`], that it is, so the agent is never
/// told of a CLI it may not get.
async fn instructions_now(instructions: Instructions) -> String {
    let chosen = instructions.fuse();
    let late = FutureExt::fuse(smol::Timer::after(INSTRUCTIONS_WITHIN));
    futures::pin_mut!(chosen, late);
    futures::select! {
        chosen = chosen => chosen.unwrap_or_default(),
        _ = late => mcp::STILL_CHOOSING.to_owned(),
    }
}

/// The most connections a listener of Studio's serves at once, so connections held open by
/// another process cannot use up Studio's file descriptors.
const MAX_CONNECTIONS: usize = 64;

/// How long a listener waits after a connection it could not take, before taking the next.
const ACCEPT_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// Whether a connection's peer proved itself to the listener (the gateway's token, checked
/// once a request's headers are in): its server says so, and a full listener lets go of
/// connections that have not first ([`serve_each`]).
#[derive(Clone, Default)]
pub(crate) struct Standing(Arc<AtomicBool>);

impl Standing {
    pub(crate) fn prove(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub(crate) fn proved(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Serves each connection `accept` takes on a task of its own, at most [`MAX_CONNECTIONS`] at
/// once, telling `serve` the connection's [`Standing`] for it to raise. A full listener closes
/// the oldest connection whose peer has not proved itself to take the next (its task is
/// dropped), so connections another local process holds open never keep out one that proves
/// itself; with every one proved, the next is closed unread. A connection it could not take
/// (out of descriptors, reset before it was taken) ends that connection, not the listener.
pub(crate) async fn serve_each<C, A, S>(
    mut accept: impl FnMut() -> A,
    serve: impl Fn(C, Standing) -> S,
) where
    A: Future<Output = std::io::Result<C>>,
    S: Future<Output = ()> + Send + 'static,
{
    // Oldest first.
    let mut connections: Vec<(smol::Task<()>, Standing)> = Vec::new();
    loop {
        let Ok(connection) = accept().await else {
            smol::Timer::after(ACCEPT_RETRY).await;
            continue;
        };
        connections.retain(|(task, _)| !task.is_finished());
        if connections.len() >= MAX_CONNECTIONS {
            match connections
                .iter()
                .position(|(_, standing)| !standing.proved())
            {
                Some(oldest) => drop(connections.remove(oldest)),
                None => {
                    drop(connection);
                    continue;
                }
            }
        }
        let standing = Standing::default();
        connections.push((smol::spawn(serve(connection, standing.clone())), standing));
    }
}

/// The most one message from the agent may hold; a longer one ends the connection.
const LINE_MAX: usize = if cfg!(test) { 1024 } else { 64 * 1024 * 1024 };

/// Reads the next line from `reader` into `line`, without its end: whether there was one. At
/// the end of the stream, on an error and past [`LINE_MAX`] there is none.
async fn next_line(reader: &mut (impl AsyncBufRead + Unpin), line: &mut Vec<u8>) -> bool {
    line.clear();
    loop {
        let Ok(buffer) = reader.fill_buf().await else {
            return false;
        };
        if buffer.is_empty() {
            return !line.is_empty();
        }
        let (taken, ended) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                line.extend_from_slice(&buffer[..end]);
                (end + 1, true)
            }
            None => {
                line.extend_from_slice(buffer);
                (buffer.len(), false)
            }
        };
        reader.consume_unpin(taken);
        if line.len() > LINE_MAX {
            return false;
        }
        if ended {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return true;
        }
    }
}

async fn serve_connection(
    reader: impl AsyncRead + Unpin + Send + 'static,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
    tools: mpsc::UnboundedSender<ToolRequest>,
    instructions: Instructions,
) {
    let (replies_tx, mut replies_rx) = mpsc::unbounded::<Value>();
    let write = smol::spawn(async move {
        while let Some(reply) = replies_rx.next().await {
            let mut line = serde_json::to_string(&reply).unwrap_or_default();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            writer.flush().await.ok();
        }
    });

    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut calls: Vec<smol::Task<()>> = Vec::new();
    // The calls under way, by their id as text, to tell them they were cancelled.
    let cancels: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, oneshot::Sender<()>>>,
    > = Default::default();
    while next_line(&mut reader, &mut line).await {
        let Ok(line) = std::str::from_utf8(&line) else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        match mcp::handle(line) {
            Incoming::Reply(reply) => {
                replies_tx.unbounded_send(reply).ok();
            }
            // Answered on its own, as a call is, once the instructions are known.
            Incoming::Initialize { id, version } => {
                let (replies_tx, instructions) = (replies_tx.clone(), instructions.clone());
                calls.retain(|call| !call.is_finished());
                calls.push(smol::spawn(async move {
                    let conversation = instructions_now(instructions).await;
                    replies_tx
                        .unbounded_send(mcp::initialize(id, version, &conversation))
                        .ok();
                }));
            }
            Incoming::Nothing => {}
            Incoming::Cancelled { id } => {
                if let Some(cancel) = cancels
                    .lock()
                    .ok()
                    .and_then(|mut c| c.remove(&id.to_string()))
                {
                    cancel.send(()).ok();
                }
            }
            Incoming::CallTool {
                id,
                name,
                arguments,
                progress,
            } => {
                let (reply, result) = oneshot::channel();
                let (cancel, cancelled) = oneshot::channel();
                if let Ok(mut cancels) = cancels.lock() {
                    cancels.insert(id.to_string(), cancel);
                }
                if tools
                    .unbounded_send(ToolRequest {
                        name,
                        arguments,
                        reply,
                        cancelled,
                    })
                    .is_err()
                {
                    if let Ok(mut cancels) = cancels.lock() {
                        cancels.remove(&id.to_string());
                    }
                    let reply =
                        mcp::tool_reply(id, "Studio closed the agent session.".into(), true);
                    replies_tx.unbounded_send(reply).ok();
                    continue;
                }
                calls.retain(|call| !call.is_finished());
                let replies_tx = replies_tx.clone();
                let cancels = cancels.clone();
                // Each call answers on its own, so a change waiting for approval does not hold
                // up the agent's reads. A long call reports progress, so the agent does not give
                // up on it.
                calls.push(smol::spawn(async move {
                    let mut result = result.fuse();
                    let mut ticks = 0;
                    let answer = loop {
                        let mut tick = FutureExt::fuse(smol::Timer::after(PROGRESS_EVERY));
                        futures::select! {
                            answer = result => break answer,
                            _ = tick => {
                                ticks += 1;
                                if let Some(token) = &progress {
                                    replies_tx.unbounded_send(mcp::progress(token, ticks)).ok();
                                }
                            }
                        }
                    };
                    // Answered: a cancellation after this stops nothing.
                    if let Ok(mut cancels) = cancels.lock() {
                        cancels.remove(&id.to_string());
                    }
                    let (text, is_error) = answer
                        .unwrap_or_else(|_| ("Studio closed the agent session.".into(), true));
                    replies_tx
                        .unbounded_send(mcp::tool_reply(id, text, is_error))
                        .ok();
                }));
            }
        }
    }
    drop(replies_tx);
    for call in calls {
        call.await;
    }
    write.await;
}

/// The `mcp-proxy` command: copies stdin to the socket and the socket to stdout until either
/// side closes. Holds no token and runs no logic.
pub fn proxy(socket: &Path) -> std::io::Result<()> {
    use std::io::{Read as _, Write as _};

    let stream = sys::BlockingStream::connect(socket)?;
    let mut to_socket = stream.try_clone()?;
    let mut from_socket = stream;
    let upstream = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_socket.write_all(&buffer[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        to_socket.shutdown(std::net::Shutdown::Write).ok();
    });
    let mut stdout = std::io::stdout().lock();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match from_socket.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                stdout.write_all(&buffer[..n])?;
                stdout.flush()?;
            }
        }
    }
    drop(upstream);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;

    /// A connection as the listener test offers it: it notes its id when it is closed, whether
    /// served or closed unread.
    struct Offered {
        id: usize,
        proves: bool,
        closed: Arc<Mutex<Vec<usize>>>,
    }

    impl Drop for Offered {
        fn drop(&mut self) {
            self.closed.lock().unwrap().push(self.id);
        }
    }

    /// A full listener closes the oldest connection whose peer has not proved itself to take
    /// the next, so connections another local process holds open, idle within their bound,
    /// never keep out one that proves itself (the agent's CLI with its token). With every one
    /// proved, the next is closed unread.
    #[test]
    fn a_full_listener_lets_go_of_the_oldest_unproved_connection() {
        smol::block_on(async {
            let (offer, offered) = smol::channel::unbounded::<Offered>();
            let closed = Arc::new(Mutex::new(Vec::new()));
            let served = Arc::new(Mutex::new(Vec::new()));
            let listening = {
                let served = served.clone();
                smol::spawn(serve_each(
                    move || {
                        let offered = offered.clone();
                        async move { offered.recv().await.map_err(std::io::Error::other) }
                    },
                    move |connection: Offered, standing: Standing| {
                        served.lock().unwrap().push(connection.id);
                        if connection.proves {
                            standing.prove();
                        }
                        async move {
                            let _held = connection;
                            futures::future::pending::<()>().await;
                        }
                    },
                ))
            };
            let mut next = 0;
            let mut connect = |proves: bool| {
                let connection = Offered {
                    id: next,
                    proves,
                    closed: closed.clone(),
                };
                next += 1;
                offer.try_send(connection).unwrap();
            };
            async fn until(done: impl Fn() -> bool) {
                let by = Instant::now() + Duration::from_secs(5);
                while !done() {
                    assert!(Instant::now() < by, "the listener never took it");
                    smol::Timer::after(Duration::from_millis(1)).await;
                }
            }
            let taken = |n: usize| served.lock().unwrap().len() == n;
            // A dropped task's future is dropped on the executor, so in no set order.
            let closed_now = || {
                let mut closed = closed.lock().unwrap().clone();
                closed.sort_unstable();
                closed
            };

            // The agent's CLI, then connections another process opens and holds.
            connect(true);
            for _ in 1..MAX_CONNECTIONS {
                connect(false);
            }
            until(|| taken(MAX_CONNECTIONS)).await;
            assert!(closed_now().is_empty());
            // Full: the next, proved or not, takes the oldest unproved one's place.
            connect(false);
            until(|| taken(MAX_CONNECTIONS + 1) && closed_now() == [1]).await;
            connect(true);
            until(|| taken(MAX_CONNECTIONS + 2) && closed_now() == [1, 2]).await;
            // Proved ones take every unproved one's place, the one that took the first's
            // included, and the CLI's stays.
            for _ in 3..=MAX_CONNECTIONS {
                connect(true);
            }
            let expected: Vec<usize> = (1..=MAX_CONNECTIONS).collect();
            until(|| taken(2 * MAX_CONNECTIONS) && closed_now() == expected).await;
            // With every one proved, the next is closed unread.
            connect(false);
            until(|| closed_now().len() == MAX_CONNECTIONS + 1).await;
            assert_eq!(closed_now().last(), Some(&(2 * MAX_CONNECTIONS)));
            assert!(taken(2 * MAX_CONNECTIONS), "closed unread");
            drop(listening);
        });
    }

    #[test]
    fn binds_in_a_private_directory_and_removes_the_socket_on_drop() {
        let dir = socket_dir();
        let socket = ToolSocket::bind(&dir).expect("bind the tool socket");
        let path = socket.path.clone();
        assert!(path.exists());
        assert!(path.as_os_str().len() <= MAX_SOCKET_PATH);
        drop(socket);
        assert!(!path.exists());
    }

    #[test]
    fn a_long_call_reports_growing_progress_until_it_answers() {
        smol::block_on(async {
            let mut socket = ToolSocket::bind(&socket_dir()).expect("bind the tool socket");
            let (tools_tx, mut tools_rx) = mpsc::unbounded();
            let (_chosen, instructions) = oneshot::channel();
            let _serve = smol::spawn(socket.serve(tools_tx, instructions.shared()));
            let path = socket.path.clone();
            let stream = smol::unblock(move || sys::BlockingStream::connect(path))
                .await
                .unwrap();
            let mut writer = smol::Unblock::new(stream.try_clone().unwrap());
            let stream = smol::Unblock::new(stream);
            let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"omnigraph_run","arguments":{"args":[]},"_meta":{"progressToken":7}}}"#;
            writer
                .write_all(format!("{call}\n").as_bytes())
                .await
                .unwrap();
            let request = tools_rx.next().await.unwrap();
            // The call holds past several ticks, then answers.
            smol::Timer::after(PROGRESS_EVERY * 3 + PROGRESS_EVERY / 2).await;
            request.reply.send(("done".into(), false)).ok();
            let mut lines = BufReader::new(stream).lines();
            let mut progress = Vec::new();
            let answer = loop {
                let line: Value =
                    serde_json::from_str(&lines.next().await.unwrap().unwrap()).unwrap();
                if line["method"] == "notifications/progress" {
                    assert_eq!(line["params"]["progressToken"], 7);
                    progress.push(line["params"]["progress"].as_u64().unwrap());
                } else {
                    break line;
                }
            };
            assert_eq!(answer["id"], 1);
            assert_eq!(answer["result"]["content"][0]["text"], "done");
            assert!(progress.len() >= 2, "{progress:?}");
            assert!(
                progress.windows(2).all(|pair| pair[0] < pair[1]),
                "{progress:?}"
            );
        });
    }

    /// `initialize` gives the conversation's instructions as they are when it arrives: those
    /// Studio sends once it has chosen the CLI, though they come after the agent asked; and,
    /// while Studio is still choosing past the wait, that it is, naming no CLI.
    #[test]
    fn initialize_answers_with_what_studio_chose_when_it_asks() {
        async fn initialize(
            send: impl FnOnce(oneshot::Sender<String>) -> Option<oneshot::Sender<String>>,
            after: Option<std::time::Duration>,
        ) -> String {
            let mut socket = ToolSocket::bind(&socket_dir()).expect("bind the tool socket");
            let (tools_tx, _tools_rx) = mpsc::unbounded();
            let (chosen, instructions) = oneshot::channel();
            let _serve = smol::spawn(socket.serve(tools_tx, instructions.shared()));
            let later = send(chosen);
            let path = socket.path.clone();
            let stream = smol::unblock(move || sys::BlockingStream::connect(path))
                .await
                .unwrap();
            let mut writer = smol::Unblock::new(stream.try_clone().unwrap());
            let mut lines = BufReader::new(smol::Unblock::new(stream)).lines();
            let call = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
            writer
                .write_all(format!("{call}\n").as_bytes())
                .await
                .unwrap();
            // Studio chooses after the agent asked, or is still choosing when it is answered.
            let choosing = match (later, after) {
                (Some(later), Some(after)) => {
                    smol::Timer::after(after).await;
                    later
                        .send("Your CLI is /home/.studio/omnigraph.".into())
                        .ok();
                    None
                }
                (later, _) => later,
            };
            let answer: Value =
                serde_json::from_str(&lines.next().await.unwrap().unwrap()).unwrap();
            drop(choosing);
            assert_eq!(answer["id"], 1);
            answer["result"]["instructions"]
                .as_str()
                .unwrap()
                .to_owned()
        }
        smol::block_on(async {
            let chosen = "Your CLI is /home/.studio/omnigraph.";
            let before = initialize(
                |sender| {
                    sender.send(chosen.into()).ok();
                    None
                },
                None,
            )
            .await;
            assert!(before.ends_with(chosen), "{before}");
            let after = initialize(Some, Some(INSTRUCTIONS_WITHIN / 4)).await;
            assert!(after.ends_with(chosen), "{after}");
            let choosing = initialize(Some, None).await;
            assert!(choosing.ends_with(mcp::STILL_CHOOSING), "{choosing}");
            assert!(!choosing.contains(".studio"), "{choosing}");
            // A conversation that ended before it chose has none of its own.
            let ended = initialize(|_| None, None).await;
            assert_eq!(ended, mcp::INSTRUCTIONS);
        });
    }

    /// Lines arrive in pieces and end with `\n` or `\r\n`; one past the cap ends the
    /// connection rather than growing without end.
    #[test]
    fn lines_are_read_whole_and_a_line_past_the_cap_ends_the_connection() {
        smol::block_on(async {
            let text = format!("one\r\ntwo\n\n{}\nlast", "x".repeat(LINE_MAX + 1));
            let mut reader =
                futures::io::BufReader::with_capacity(3, futures::io::Cursor::new(text));
            let mut line = Vec::new();
            let mut read = Vec::new();
            while next_line(&mut reader, &mut line).await {
                read.push(String::from_utf8(line.clone()).unwrap());
            }
            assert_eq!(read, ["one", "two", ""]);

            let mut reader = futures::io::Cursor::new("a\nend");
            let mut read = Vec::new();
            while next_line(&mut reader, &mut line).await {
                read.push(String::from_utf8(line.clone()).unwrap());
            }
            assert_eq!(read, ["a", "end"]);
        });
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_directory_others_can_enter() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("ogs-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ToolSocket::bind(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
