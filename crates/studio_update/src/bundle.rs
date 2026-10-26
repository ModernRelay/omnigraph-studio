//! Studio's installer for a macOS bundle (spec 017, "Downloading and installing on macOS"): the
//! update is downloaded through Studio's HTTP client, at most [`MAX_UPDATE_BYTES`], and kept
//! only when its minisign signature verifies against the update key; it is then unpacked beside
//! the running bundle and exchanged with it in one rename ([`install::replace_bundle`]).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use base64::Engine as _;
use futures::AsyncReadExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use http_client::http::header::CONTENT_LENGTH;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy, Request};
use minisign_verify::{PublicKey, Signature};

use crate::{InstallError, Installer, Job, install};

/// How long a download may take in all, headers and body.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The most of an update Studio downloads; a macOS update is about 50 MB today.
pub const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;

/// The redirects a download follows: GitHub sends a release file's address on to its file host.
const MAX_REDIRECTS: u32 = 10;

/// Downloads through Studio's HTTP client and installs by exchanging the bundle with the update.
pub struct BundleInstaller {
    http: Arc<dyn HttpClient>,
}

impl BundleInstaller {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self { http }
    }
}

impl Installer for BundleInstaller {
    fn download(
        &self,
        job: &Job,
        progress: Box<dyn Fn(u8) + Send>,
    ) -> BoxFuture<'static, Result<Vec<u8>, InstallError>> {
        let http = self.http.clone();
        let job = job.clone();
        Box::pin(async move {
            let bytes = fetch(http, &job.update.url, MAX_UPDATE_BYTES, progress)
                .await
                .map_err(|error| InstallError::Failed(format!("{error:#}")))?;
            verify(&bytes, &job.update.signature, &job.public_key)
                .map_err(InstallError::Refused)?;
            Ok(bytes)
        })
    }

    fn install(&self, job: &Job, bytes: Vec<u8>) -> BoxFuture<'static, Result<(), InstallError>> {
        let bundle = job.bundle.clone();
        on_thread(move || {
            install::replace_bundle(&bundle, &bytes)
                .map_err(|error| InstallError::Failed(format!("{error:#}")))
        })
    }
}

/// The whole file at `url`, refused past `limit` bytes whether the host says its length or
/// not; `progress` hears each whole percent of a length it says.
async fn fetch(
    http: Arc<dyn HttpClient>,
    url: &str,
    limit: u64,
    progress: Box<dyn Fn(u8) + Send>,
) -> Result<Vec<u8>> {
    let request = Request::get(url)
        .follow_redirects(RedirectPolicy::FollowLimit(MAX_REDIRECTS))
        .timeout(DOWNLOAD_TIMEOUT)
        .body(AsyncBody::default())?;
    let mut response = http.send(request).await?;
    if !response.status().is_success() {
        bail!("the release host answered {}", response.status());
    }
    let total = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(total) = total.filter(|total| *total > limit) {
        bail!("the update is {total} bytes, more than the {limit} Studio accepts");
    }
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut said = None;
    loop {
        let read = response.body_mut().read(&mut chunk).await?;
        if read == 0 {
            return Ok(bytes);
        }
        if (bytes.len() + read) as u64 > limit {
            bail!("the update is more than the {limit} bytes Studio accepts");
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(total) = total.filter(|total| *total > 0) {
            let percent = (bytes.len() as u64 * 100 / total).min(100) as u8;
            if said.replace(percent) != Some(percent) {
                progress(percent);
            }
        }
    }
}

/// Whether `signature` (base64 of a minisign `.sig` file) signs `bytes` under `key` (base64 of
/// a minisign `.pub` file): anything that does not decode or verify is refused.
pub fn verify(bytes: &[u8], signature: &str, key: &str) -> Result<(), String> {
    let key = decoded(key)
        .and_then(|text| PublicKey::decode(&text).map_err(|error| error.to_string()))
        .map_err(|error| format!("the update key: {error}"))?;
    let signature = decoded(signature)
        .and_then(|text| Signature::decode(&text).map_err(|error| error.to_string()))
        .map_err(|error| format!("the signature: {error}"))?;
    // `true` also accepts a signature over the file itself rather than over its hash, which
    // older minisign signers write.
    key.verify(bytes, &signature, true)
        .map_err(|error| format!("the signature does not verify: {error}"))
}

fn decoded(base64: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64.trim())
        .map_err(|error| format!("not base64: {error}"))?;
    String::from_utf8(bytes).map_err(|_| "not text".to_owned())
}

/// Runs blocking file work on a thread of its own, so no executor thread waits on the disk.
fn on_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, InstallError> + Send + 'static,
) -> BoxFuture<'static, Result<T, InstallError>> {
    let (sender, receiver) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("studio-update".into())
        .spawn(move || {
            sender.send(work()).ok();
        });
    Box::pin(async move {
        spawned.map_err(|error| InstallError::Failed(error.to_string()))?;
        receiver
            .await
            .unwrap_or_else(|_| Err(InstallError::Failed("the update stopped".into())))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use http_client::{FakeHttpClient, Response};
    use std::sync::Mutex;

    const UPDATE: &[u8] = include_bytes!("../tests/fixtures/update.app.tar.gz");
    const SIGNATURE: &str = include_str!("../tests/fixtures/update.app.tar.gz.minisig");
    const PUBLIC_KEY: &str = include_str!("../tests/fixtures/test.pub");

    fn base64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    /// A host answering every request with `status` and `body`, saying its length when `said`.
    fn host(status: u16, body: Vec<u8>, said: bool) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |_| {
            let mut response = Response::builder().status(status);
            if said {
                response = response.header(CONTENT_LENGTH, body.len());
            }
            let response = response.body(AsyncBody::from(body.clone())).unwrap();
            async move { Ok(response) }
        })
    }

    fn heard() -> (Arc<Mutex<Vec<u8>>>, Box<dyn Fn(u8) + Send>) {
        let percents = Arc::new(Mutex::new(Vec::new()));
        let sink = percents.clone();
        (
            percents,
            Box::new(move |percent| sink.lock().unwrap().push(percent)),
        )
    }

    #[test]
    fn a_download_returns_the_whole_file_and_hears_each_whole_percent() {
        let body = vec![7u8; 300 * 1024];
        let (percents, progress) = heard();
        let bytes = block_on(fetch(
            host(200, body.clone(), true),
            "https://x/u",
            1 << 20,
            progress,
        ))
        .unwrap();
        assert_eq!(bytes, body);
        let percents = percents.lock().unwrap();
        assert_eq!(percents.last(), Some(&100));
        assert!(
            percents.windows(2).all(|pair| pair[0] < pair[1]),
            "{percents:?}"
        );
    }

    #[test]
    fn a_file_said_to_be_too_large_is_refused_before_its_body_is_read() {
        let (percents, progress) = heard();
        let error = block_on(fetch(
            host(200, vec![0; 2048], true),
            "https://x/u",
            1024,
            progress,
        ))
        .unwrap_err();
        assert!(error.to_string().contains("2048 bytes"), "{error}");
        assert!(percents.lock().unwrap().is_empty());
    }

    #[test]
    fn a_file_that_grows_past_the_limit_unsaid_is_stopped() {
        let (_, progress) = heard();
        let error = block_on(fetch(
            host(200, vec![0; 2048], false),
            "https://x/u",
            1024,
            progress,
        ))
        .unwrap_err();
        assert!(
            error.to_string().contains("more than the 1024 bytes"),
            "{error}"
        );
    }

    #[test]
    fn a_host_that_answers_an_error_fails_the_download() {
        let (_, progress) = heard();
        let error = block_on(fetch(
            host(404, Vec::new(), true),
            "https://x/u",
            1024,
            progress,
        ))
        .unwrap_err();
        assert!(error.to_string().contains("404"), "{error}");
    }

    #[test]
    fn only_the_file_the_key_signed_verifies() {
        let (signature, key) = (base64(SIGNATURE), base64(PUBLIC_KEY));
        assert_eq!(verify(UPDATE, &signature, &key), Ok(()));
        let mut tampered = UPDATE.to_vec();
        tampered[0] ^= 0xff;
        assert!(verify(&tampered, &signature, &key).is_err());
        assert!(verify(UPDATE, "not base64!", &key).is_err());
        assert!(verify(UPDATE, &signature, &base64("not a key")).is_err());
    }
}
