//! The update manifest Studio's releases publish, `latest.json`: Tauri's format, read with
//! `cargo-packager-updater`'s own type (spec 017, "What the releases serve").

use anyhow::{Context as _, Result, bail};
use cargo_packager_updater::{RemoteRelease, RemoteReleaseData, UpdateFormat};
use semver::Version;

const RELEASES: &str = "https://github.com/ModernRelay/omnigraph-studio/releases";

/// Where every release build reads the newest release's manifest.
pub const FEED: &str =
    "https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/latest.json";

/// The most of a manifest Studio reads; today's is under 1 KiB.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// The newest release, as this platform sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: Version,
    pub notes: Option<String>,
    /// The file this platform installs, when Studio installs updates here.
    pub update: Option<PlatformUpdate>,
}

/// A signed update file for one platform.
#[derive(Debug, Clone, PartialEq)]
pub struct PlatformUpdate {
    pub url: String,
    /// The content of the file's minisign `.sig`.
    pub signature: String,
}

/// Reads a manifest for `platform` (`macos-aarch64`, `linux-x86_64`, …). Only a macOS bundle
/// (`format: app`) is installed in place today; other platforms learn the version alone.
pub fn parse(bytes: &[u8], platform: &str) -> Result<Release> {
    let release: RemoteRelease =
        serde_json::from_slice(bytes).context("the answer is not an update manifest")?;
    let entry = match &release.data {
        RemoteReleaseData::Static { platforms } => platforms.get(platform),
        RemoteReleaseData::Dynamic(entry) => Some(entry),
    };
    let update = match entry {
        Some(entry) if matches!(entry.format, UpdateFormat::App) => {
            if entry.url.scheme() != "https" {
                bail!("the update for {platform} is not served over HTTPS");
            }
            Some(PlatformUpdate {
                url: entry.url.to_string(),
                signature: entry.signature.clone(),
            })
        }
        _ => None,
    };
    Ok(Release {
        version: release.version,
        notes: release.notes,
        update,
    })
}

/// Whether `release` is newer than the running version, by semver precedence.
pub fn is_newer(release: &Version, running: &Version) -> bool {
    release > running
}

/// The version-free download for a platform, which always serves the newest release; the
/// releases page for a platform Studio does not build.
pub fn download_url(platform: &str) -> String {
    let file = match platform {
        "macos-aarch64" => "Omnigraph-Studio-arm64.dmg",
        "linux-x86_64" => "omnigraph-studio-linux-x86_64.tar.gz",
        "linux-aarch64" => "omnigraph-studio-linux-aarch64.tar.gz",
        "windows-x86_64" => "omnigraph-studio-windows-x86_64.zip",
        _ => return format!("{RELEASES}/latest"),
    };
    format!("{RELEASES}/latest/download/{file}")
}

/// A release's page, with its notes.
pub fn release_page(version: &Version) -> String {
    format!("{RELEASES}/tag/v{version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
      "version": "0.3.0",
      "notes": "Faster tables.",
      "pub_date": "2026-11-02T10:00:00Z",
      "platforms": {
        "macos-aarch64": {
          "url": "https://github.com/ModernRelay/omnigraph-studio/releases/download/v0.3.0/Omnigraph-Studio-0.3.0-arm64.app.tar.gz",
          "signature": "c2lnbmF0dXJl",
          "format": "app"
        }
      }
    }"#;

    #[test]
    fn a_platform_with_an_entry_gets_its_update() {
        let release = parse(MANIFEST.as_bytes(), "macos-aarch64").unwrap();
        assert_eq!(release.version, Version::new(0, 3, 0));
        let update = release.update.expect("an update for macOS");
        assert!(
            update
                .url
                .ends_with("/v0.3.0/Omnigraph-Studio-0.3.0-arm64.app.tar.gz")
        );
        assert_eq!(update.signature, "c2lnbmF0dXJl");
    }

    #[test]
    fn a_platform_without_an_entry_only_learns_the_version() {
        let release = parse(MANIFEST.as_bytes(), "linux-x86_64").unwrap();
        assert_eq!(release.version, Version::new(0, 3, 0));
        assert!(release.update.is_none());
    }

    #[test]
    fn an_update_not_served_over_https_is_refused() {
        let plain = MANIFEST.replace("https://github.com", "http://github.com");
        let error = parse(plain.as_bytes(), "macos-aarch64").unwrap_err();
        assert!(error.to_string().contains("HTTPS"), "{error}");
    }

    #[test]
    fn an_update_of_another_format_is_not_installed_here() {
        let nsis = MANIFEST.replace(r#""format": "app""#, r#""format": "nsis""#);
        assert!(
            parse(nsis.as_bytes(), "macos-aarch64")
                .unwrap()
                .update
                .is_none()
        );
    }

    #[test]
    fn a_manifest_that_is_not_one_is_refused() {
        assert!(parse(b"<html>Not Found</html>", "macos-aarch64").is_err());
        assert!(parse(br#"{"version": "soon"}"#, "macos-aarch64").is_err());
    }

    #[test]
    fn only_a_greater_version_is_newer() {
        let running = Version::new(0, 2, 1);
        assert!(is_newer(&Version::new(0, 3, 0), &running));
        assert!(is_newer(&Version::new(0, 2, 2), &running));
        assert!(!is_newer(&Version::new(0, 2, 1), &running));
        assert!(!is_newer(&Version::new(0, 2, 0), &running));
        assert!(!is_newer(
            &Version::parse("0.2.2-beta.1").unwrap(),
            &Version::new(0, 2, 2)
        ));
    }

    #[test]
    fn each_platform_downloads_its_version_free_file() {
        let base = "https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/";
        assert_eq!(
            download_url("macos-aarch64"),
            format!("{base}Omnigraph-Studio-arm64.dmg")
        );
        assert_eq!(
            download_url("linux-x86_64"),
            format!("{base}omnigraph-studio-linux-x86_64.tar.gz")
        );
        assert_eq!(
            download_url("linux-aarch64"),
            format!("{base}omnigraph-studio-linux-aarch64.tar.gz")
        );
        assert_eq!(
            download_url("windows-x86_64"),
            format!("{base}omnigraph-studio-windows-x86_64.zip")
        );
        assert_eq!(
            download_url("macos-x86_64"),
            "https://github.com/ModernRelay/omnigraph-studio/releases/latest"
        );
    }

    #[test]
    fn release_notes_are_the_releases_page() {
        assert_eq!(
            release_page(&Version::new(0, 3, 0)),
            "https://github.com/ModernRelay/omnigraph-studio/releases/tag/v0.3.0"
        );
    }
}
