//! The loopback redirect the browser returns to (RFC 8252 §7.3): plain HTTP on `127.0.0.1` and
//! a port of its own, which takes one sign-in's answer and closes.

use std::net::Ipv4Addr;
use std::time::Duration;

use futures::stream::FuturesUnordered;
use futures::{AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _};
use smol::net::{TcpListener, TcpStream};
use url::Url;

use crate::shown;

/// How long one request to the listener may take to arrive in full.
const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// The most of one request's head the listener reads.
const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// The most connections answered at once; more wait to be accepted until one is answered.
const MAX_ANSWERING: usize = 8;

/// Why a sign-in ended without a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackError {
    /// The sign-in service answered with an error, in its words: the person declined, or the
    /// service refused.
    Denied(String),
    /// The listener stopped working.
    Failed(String),
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallbackError::Denied(why) | CallbackError::Failed(why) => f.write_str(why),
        }
    }
}

pub struct Callback {
    listener: TcpListener,
    redirect_uri: String,
}

enum Answered {
    Code(String),
    Denied(String),
    /// Another request: answered, and the listener waits on.
    Other,
}

/// What the listener heard next: a connection, or the answer to one.
enum Heard {
    Connection(std::io::Result<TcpStream>),
    Answered(Answered),
}

impl Callback {
    /// Listens on `127.0.0.1` (an IPv4 literal, never `localhost`, RFC 8252 §8.3) on a port the
    /// system chooses.
    pub fn bind() -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener: TcpListener::try_from(listener)?,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        })
    }

    /// `http://127.0.0.1:<port>/callback`, the `redirect_uri` the sign-in names.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits for the browser to return with the sign-in's answer, `state` matching, and gives its
    /// code. Every request is answered with a short page; any other request is answered 404, or
    /// 400 for a callback that is not this sign-in's, and the listener waits on. Connections are
    /// answered at once, up to [`MAX_ANSWERING`], so one that sends nothing (a browser's
    /// speculative preconnect) does not hold back the one that brings the answer. The caller
    /// bounds the wait ([`crate::SIGN_IN_WAIT`]) and cancels it by dropping it; the listener
    /// closes when this returns. `host` is the server the page says Studio signed in to.
    pub async fn code(
        self,
        state: &str,
        issuer: &str,
        host: &str,
    ) -> Result<String, CallbackError> {
        let mut answering = FuturesUnordered::new();
        loop {
            let accepting = answering.len() < MAX_ANSWERING;
            let heard = smol::future::or(
                async {
                    if !accepting {
                        futures::future::pending::<()>().await;
                    }
                    Heard::Connection(self.listener.accept().await.map(|(stream, _)| stream))
                },
                async {
                    match answering.next().await {
                        Some(answered) => Heard::Answered(answered),
                        None => futures::future::pending().await,
                    }
                },
            )
            .await;
            match heard {
                Heard::Connection(stream) => {
                    let stream = stream.map_err(|error| {
                        CallbackError::Failed(format!("the sign-in listener failed: {error}"))
                    })?;
                    answering.push(answer(stream, state, issuer, host));
                }
                Heard::Answered(Answered::Code(code)) => return Ok(code),
                Heard::Answered(Answered::Denied(why)) => return Err(CallbackError::Denied(why)),
                Heard::Answered(Answered::Other) => {}
            }
        }
    }
}

async fn answer(mut stream: TcpStream, state: &str, issuer: &str, host: &str) -> Answered {
    let head = smol::future::or(async { Some(read_head(&mut stream).await) }, async {
        smol::Timer::after(REQUEST_WAIT).await;
        None
    })
    .await;
    let Some(Some(head)) = head else {
        return Answered::Other;
    };
    let (status, page, answered) = judge(&head, state, issuer, host);
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Omnigraph Studio</title>\
         <meta name=\"viewport\" content=\"width=device-width\"></head><body style=\"font: 15px \
         -apple-system, system-ui, sans-serif; margin: 4em auto; max-width: 32em; padding: 0 1em\
         \"><p>{}</p></body></html>",
        escaped(&page)
    );
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.ok();
    stream.flush().await.ok();
    stream.close().await.ok();
    answered
}

/// One request's head, up to its blank line; `None` when it is longer than
/// [`MAX_REQUEST_BYTES`] or the connection ends first.
async fn read_head(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    while !head.windows(4).any(|end| end == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 || head.len() + read > MAX_REQUEST_BYTES {
            return None;
        }
        head.extend_from_slice(&buffer[..read]);
    }
    Some(head)
}

/// What a request is: its status line, the page's sentence, and what it means for the sign-in.
fn judge(head: &[u8], state: &str, issuer: &str, host: &str) -> (&'static str, String, Answered) {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    let not_found = || {
        (
            "404 Not Found",
            "Nothing is here.".to_owned(),
            Answered::Other,
        )
    };
    let (Ok(httparse::Status::Complete(_)), Some("GET"), Some(path)) =
        (request.parse(head), request.method, request.path)
    else {
        return not_found();
    };
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{path}")) else {
        return not_found();
    };
    if url.path() != "/callback" {
        return not_found();
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if param("state").as_deref() != Some(state) {
        return (
            "400 Bad Request",
            "This is not the sign-in Omnigraph Studio is waiting for. Start it again from Studio."
                .to_owned(),
            Answered::Other,
        );
    }
    // RFC 9207: an answer that names another issuer is another service's.
    if param("iss").is_some_and(|iss| iss != issuer) {
        let why = "The answer came from another sign-in service than the one Studio asked.";
        return (
            "400 Bad Request",
            why.to_owned(),
            Answered::Denied(why.to_owned()),
        );
    }
    if let Some(error) = param("error") {
        let why = shown(&param("error_description").unwrap_or(error));
        return (
            "200 OK",
            format!("Omnigraph Studio was not signed in to {host}: {why}"),
            Answered::Denied(why),
        );
    }
    match param("code") {
        Some(code) if !code.is_empty() => (
            "200 OK",
            format!("Signed in to {host}. You can close this tab."),
            Answered::Code(code),
        ),
        _ => (
            "400 Bad Request",
            "The sign-in answered without a code.".to_owned(),
            Answered::Denied("The sign-in service answered without a code.".to_owned()),
        ),
    }
}

fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(port: u16, path: &str) -> String {
        use std::io::{Read as _, Write as _};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n").unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    }

    fn port_of(callback: &Callback) -> u16 {
        callback
            .redirect_uri()
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches("/callback")
            .parse()
            .unwrap()
    }

    #[test]
    fn the_listener_takes_the_answer_whose_state_matches() {
        let callback = Callback::bind().unwrap();
        let port = port_of(&callback);
        let browser = std::thread::spawn(move || {
            let other = get(port, "/favicon.ico");
            let forged = get(port, "/callback?code=forged&state=other");
            let signed_in = get(
                port,
                "/callback?code=the-code&state=s1&iss=https%3A%2F%2Fid.example",
            );
            (other, forged, signed_in)
        });
        let code = smol::block_on(callback.code("s1", "https://id.example", "graphs.example"));
        let (other, forged, signed_in) = browser.join().unwrap();
        assert_eq!(code, Ok("the-code".to_owned()));
        assert!(other.starts_with("HTTP/1.1 404"), "{other}");
        assert!(forged.starts_with("HTTP/1.1 400"), "{forged}");
        assert!(signed_in.contains("Signed in to graphs.example. You can close this tab."));
        // Answered, the listener is gone.
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    /// A connection that sends nothing, as a browser's speculative preconnect, does not hold
    /// back the one that brings the answer: each is answered on its own. Before, the listener
    /// read one connection at a time, so the answer waited up to ten seconds behind it.
    #[test]
    fn a_silent_connection_does_not_hold_back_the_answer() {
        let callback = Callback::bind().unwrap();
        let port = port_of(&callback);
        let silent = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let browser = std::thread::spawn(move || {
            get(
                port,
                "/callback?code=the-code&state=s1&iss=https%3A%2F%2Fid.example",
            )
        });
        let started = std::time::Instant::now();
        let code = smol::block_on(callback.code("s1", "https://id.example", "graphs.example"));
        assert_eq!(code, Ok("the-code".to_owned()));
        assert!(
            started.elapsed() < REQUEST_WAIT / 2,
            "answered after {:?}",
            started.elapsed()
        );
        browser.join().unwrap();
        drop(silent);
    }

    #[test]
    fn an_error_or_another_issuer_ends_the_sign_in() {
        let callback = Callback::bind().unwrap();
        let port = port_of(&callback);
        let browser = std::thread::spawn(move || {
            get(
                port,
                "/callback?state=s1&error=access_denied&error_description=%3Cb%3ENo%3C%2Fb%3E",
            )
        });
        let ended = smol::block_on(callback.code("s1", "https://id.example", "graphs.example"));
        let page = browser.join().unwrap();
        assert_eq!(ended, Err(CallbackError::Denied("<b>No</b>".to_owned())));
        assert!(page.contains("&lt;b&gt;No&lt;/b&gt;"), "{page}");

        let callback = Callback::bind().unwrap();
        let port = port_of(&callback);
        let browser = std::thread::spawn(move || {
            get(
                port,
                "/callback?state=s1&code=c&iss=https%3A%2F%2Fevil.example",
            )
        });
        let ended = smol::block_on(callback.code("s1", "https://id.example", "graphs.example"));
        browser.join().unwrap();
        assert!(matches!(ended, Err(CallbackError::Denied(_))));
    }
}
