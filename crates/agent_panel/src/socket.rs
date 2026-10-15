//! The tool server's socket (spec 011). The agent starts `omnigraph-studio mcp-proxy <socket>`
//! as a stdio MCP server; the proxy connects here and copies bytes both ways. Studio answers
//! MCP itself and sends tool calls to the panel, which runs them on the person's session.
//!
//! The socket lives in a short per-user runtime directory only the person can enter (`0700`),
//! so no other local user can reach it, and it is removed when the agent session ends. Unix
//! socket paths are limited to about 104 bytes on macOS, which Studio's data directory can
//! exceed, so the socket does not live there. Windows 10 and later have Unix sockets too
//! (`uds_windows`); there the directory is in the person's own temporary folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::channel::{mpsc, oneshot};
use futures::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, FutureExt, StreamExt as _,
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

    /// Serves connections until the returned future is dropped.
    /// `instructions` are this conversation's, appended to the server's in `initialize`.
    pub fn serve(
        &mut self,
        tools: mpsc::UnboundedSender<ToolRequest>,
        instructions: String,
    ) -> impl Future<Output = ()> + Send + 'static {
        let listener = self.listener.take();
        let instructions: std::sync::Arc<str> = instructions.into();
        async move {
            let Some(listener) = listener else { return };
            let mut connections = Vec::new();
            while let Ok((reader, writer)) = sys::accept(&listener).await {
                connections.push(smol::spawn(serve_connection(
                    reader,
                    writer,
                    tools.clone(),
                    instructions.clone(),
                )));
            }
        }
    }
}

async fn serve_connection(
    reader: impl AsyncRead + Unpin + Send + 'static,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
    tools: mpsc::UnboundedSender<ToolRequest>,
    instructions: std::sync::Arc<str>,
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

    let mut lines = BufReader::new(reader).lines();
    let mut calls = Vec::new();
    while let Some(Ok(line)) = lines.next().await {
        if line.trim().is_empty() {
            continue;
        }
        match mcp::handle(&line, &instructions) {
            Incoming::Reply(reply) => {
                replies_tx.unbounded_send(reply).ok();
            }
            Incoming::Nothing => {}
            Incoming::CallTool {
                id,
                name,
                arguments,
                progress,
            } => {
                let (reply, result) = oneshot::channel();
                if tools
                    .unbounded_send(ToolRequest {
                        name,
                        arguments,
                        reply,
                    })
                    .is_err()
                {
                    let reply =
                        mcp::tool_reply(id, "Studio closed the agent session.".into(), true);
                    replies_tx.unbounded_send(reply).ok();
                    continue;
                }
                let replies_tx = replies_tx.clone();
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
    use super::*;

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
            let _serve = smol::spawn(socket.serve(tools_tx, String::new()));
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
