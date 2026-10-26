//! The release installer, through Studio's HTTP client against a signed update served on
//! loopback: a genuine update downloads and replaces the bundle, and any other is refused
//! before anything is written (spec 017).

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use semver::Version;
use studio_update::manifest::PlatformUpdate;
use studio_update::{BundleInstaller, InstallError, Installer as _, Job};

const UPDATE: &[u8] = include_bytes!("fixtures/update.app.tar.gz");
const SIGNATURE: &str = include_str!("fixtures/update.app.tar.gz.minisig");
const PUBLIC_KEY: &str = include_str!("fixtures/test.pub");

/// The installer a release uses, with the HTTP client Studio sends through.
fn installer() -> BundleInstaller {
    BundleInstaller::new(Arc::new(
        reqwest_client::ReqwestClient::user_agent("Omnigraph-Studio/test").unwrap(),
    ))
}

fn base64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// Serves `body` to every request on a loopback port, and gives its URL.
fn serve(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/update.app.tar.gz",
        listener.local_addr().unwrap()
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).ok();
            stream.write_all(&body).ok();
        }
    });
    url
}

fn job(url: String, bundle: std::path::PathBuf) -> Job {
    Job {
        version: Version::new(0, 3, 0),
        running: Version::new(0, 2, 1),
        update: PlatformUpdate {
            url,
            signature: base64(SIGNATURE),
        },
        bundle,
        public_key: base64(PUBLIC_KEY),
    }
}

#[test]
fn a_signed_update_downloads_whole_and_reports_its_progress() {
    let folder = tempfile::tempdir().unwrap();
    let job = job(
        serve(UPDATE.to_vec()),
        folder.path().join("Omnigraph Studio.app"),
    );
    let percents = Arc::new(Mutex::new(Vec::new()));
    let heard = percents.clone();
    let bytes = futures::executor::block_on(installer().download(
        &job,
        Box::new(move |percent| heard.lock().unwrap().push(percent)),
    ))
    .unwrap();
    assert_eq!(bytes, UPDATE);
    assert_eq!(percents.lock().unwrap().last(), Some(&100));
}

#[test]
fn an_update_its_signature_does_not_cover_is_refused() {
    let mut tampered = UPDATE.to_vec();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    let folder = tempfile::tempdir().unwrap();
    let job = job(serve(tampered), folder.path().join("Omnigraph Studio.app"));
    let result = futures::executor::block_on(installer().download(&job, Box::new(|_| {})));
    assert!(
        matches!(result, Err(InstallError::Refused(_))),
        "{result:?}"
    );
}

#[test]
fn a_download_that_cannot_reach_its_host_fails_without_being_refused() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/update.app.tar.gz", closed.local_addr().unwrap());
    drop(closed);
    let folder = tempfile::tempdir().unwrap();
    let job = job(url, folder.path().join("Omnigraph Studio.app"));
    let result = futures::executor::block_on(installer().download(&job, Box::new(|_| {})));
    assert!(matches!(result, Err(InstallError::Failed(_))), "{result:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn installing_replaces_the_bundle_with_the_updates() {
    let folder = tempfile::tempdir().unwrap();
    let bundle = folder.path().join("Omnigraph Studio.app");
    std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
    std::fs::write(
        bundle.join("Contents/MacOS/omnigraph-studio"),
        "the old build\n",
    )
    .unwrap();
    std::fs::write(bundle.join("Contents/Old.txt"), "gone after the update").unwrap();
    let job = job(serve(UPDATE.to_vec()), bundle.clone());
    futures::executor::block_on(installer().install(&job, UPDATE.to_vec())).unwrap();
    assert_eq!(
        std::fs::read_to_string(bundle.join("Contents/MacOS/omnigraph-studio")).unwrap(),
        "the new build\n"
    );
    assert!(bundle.join("Contents/Info.plist").is_file());
    assert!(!bundle.join("Contents/Old.txt").exists());
}

/// A bundle archived as `script/bundle-mac` archives it and installed by Studio still
/// passes `codesign --verify --strict`: the signature lives in the bundle's own files, and the
/// install keeps them, their modes and the executable intact.
#[cfg(target_os = "macos")]
#[test]
fn an_installed_bundle_keeps_its_signature() {
    use std::process::Command;
    let source = tempfile::tempdir().unwrap();
    let app = source.path().join("Omnigraph Studio.app");
    std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    std::fs::copy("/usr/bin/true", app.join("Contents/MacOS/omnigraph-studio")).unwrap();
    std::fs::write(
        app.join("Contents/Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>omnigraph-studio</string>
	<key>CFBundleIdentifier</key>
	<string>com.modernrelay.omnigraph-studio.test</string>
	<key>CFBundleShortVersionString</key>
	<string>0.3.0</string>
</dict>
</plist>
"#,
    )
    .unwrap();
    let signed = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&app)
        .status()
        .unwrap();
    assert!(signed.success());
    let archive = source.path().join("update.app.tar.gz");
    let archived = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .args(["--no-mac-metadata", "-czf"])
        .arg(&archive)
        .arg("-C")
        .arg(source.path())
        .arg("Omnigraph Studio.app")
        .status()
        .unwrap();
    assert!(archived.success());

    let target = tempfile::tempdir().unwrap();
    let bundle = target.path().join("Omnigraph Studio.app");
    std::fs::create_dir_all(bundle.join("Contents")).unwrap();
    let job = job(serve(Vec::new()), bundle.clone());
    let bytes = std::fs::read(&archive).unwrap();
    futures::executor::block_on(installer().install(&job, bytes)).unwrap();
    let verified = Command::new("codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
}
