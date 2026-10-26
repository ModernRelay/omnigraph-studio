//! The update manifest Studio's releases publish, `latest.json`, in Tauri's format (spec 017,
//! "What the releases serve"). Only this platform's entry is decoded, so an entry a later
//! release adds for another platform, in a format this Studio does not know, never stops it
//! learning of a newer version.

use std::collections::HashMap;

use anyhow::{Context as _, Result, bail};
use semver::Version;
use serde::Deserialize;

const RELEASES: &str = "https://github.com/ModernRelay/omnigraph-studio/releases";

/// Where every release build reads the newest release's manifest.
pub const FEED: &str =
    "https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/latest.json";

/// The most of a manifest Studio reads; today's is under 1 KiB.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// The platform Studio installs updates on in place, and the only one the manifest's
/// `app` entry is read for.
const MAC: &str = "macos-aarch64";

/// The newest release, as this platform sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: Version,
    /// The file this platform installs, when Studio installs updates here.
    pub update: Option<PlatformUpdate>,
}

/// A signed update file for one platform.
#[derive(Debug, Clone, PartialEq)]
pub struct PlatformUpdate {
    pub url: String,
    /// Base64 of the file's minisign `.sig`.
    pub signature: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    platforms: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct Entry {
    url: String,
    signature: String,
    format: String,
}

/// Reads a manifest for `platform` (`macos-aarch64`, `linux-x86_64`, …). Only a macOS bundle
/// (`format: app`) is installed in place today; other platforms learn the version alone. The
/// macOS file must be the one the release workflow uploads for that version, under the
/// release's own address: a manifest naming any other file is refused.
pub fn parse(bytes: &[u8], platform: &str) -> Result<Release> {
    let manifest: Manifest =
        serde_json::from_slice(bytes).context("the answer is not an update manifest")?;
    let version = Version::parse(&manifest.version).with_context(|| {
        format!(
            "the manifest's version {:?} is not semver",
            manifest.version
        )
    })?;
    let update = match manifest.platforms.get(platform) {
        Some(entry) if platform == MAC => {
            let entry: Entry = serde_json::from_value(entry.clone())
                .with_context(|| format!("the manifest's {platform} entry"))?;
            if entry.format != "app" {
                None
            } else if entry.url != update_url(&version) {
                bail!("the update for {platform} is not this release's file");
            } else {
                Some(PlatformUpdate {
                    url: entry.url,
                    signature: entry.signature,
                })
            }
        }
        _ => None,
    };
    Ok(Release { version, update })
}

/// The macOS update file the release workflow uploads for `version`.
pub fn update_url(version: &Version) -> String {
    format!("{RELEASES}/download/v{version}/Omnigraph-Studio-{version}-arm64.app.tar.gz")
}

/// Whether `release` is newer than the running version, by semver precedence: build metadata
/// counts for nothing.
pub fn is_newer(release: &Version, running: &Version) -> bool {
    release.cmp_precedence(running).is_gt()
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

    /// The manifest `script/release-files.py` writes, which its own test compares with.
    #[test]
    fn the_manifest_a_release_publishes_is_read() {
        let written = include_str!("../tests/fixtures/latest.json");
        let release = parse(written.as_bytes(), "macos-aarch64").unwrap();
        assert_eq!(release.version, Version::new(0, 3, 0));
        let update = release.update.expect("macOS is offered the update");
        assert_eq!(
            update.url,
            "https://github.com/ModernRelay/omnigraph-studio/releases/download/v0.3.0/Omnigraph-Studio-0.3.0-arm64.app.tar.gz"
        );
        assert_eq!(update.signature, "c2lnbmF0dXJl");
        assert!(
            parse(written.as_bytes(), "linux-x86_64")
                .unwrap()
                .update
                .is_none()
        );
    }

    #[test]
    fn a_platform_without_an_entry_only_learns_the_version() {
        let release = parse(MANIFEST.as_bytes(), "linux-x86_64").unwrap();
        assert_eq!(release.version, Version::new(0, 3, 0));
        assert!(release.update.is_none());
    }

    /// Only the file the release workflow uploads for that version is installed: not one
    /// served in the clear, from another host, or of another version.
    #[test]
    fn an_update_from_anywhere_but_its_release_is_refused() {
        for elsewhere in [
            MANIFEST.replace("https://github.com", "http://github.com"),
            MANIFEST.replace("https://github.com", "https://example.com"),
            MANIFEST.replace("download/v0.3.0", "download/v0.2.9"),
            MANIFEST.replace(".app.tar.gz", ".app.tar.gz?x=1"),
        ] {
            let error = parse(elsewhere.as_bytes(), "macos-aarch64").unwrap_err();
            assert!(
                error.to_string().contains("not this release's file"),
                "{error}"
            );
        }
    }

    /// An entry a later release adds for another platform, in a shape this Studio does not
    /// know, leaves the check working.
    #[test]
    fn another_platforms_entry_of_any_shape_is_not_read() {
        let later = MANIFEST.replace(
            r#""platforms": {"#,
            r#""platforms": {
              "linux-x86_64": {"format": "deb", "url": "not a url"},
              "windows-x86_64": "an installer, one day","#,
        );
        let release = parse(later.as_bytes(), "macos-aarch64").unwrap();
        assert!(release.update.is_some());
        let linux = parse(later.as_bytes(), "linux-x86_64").unwrap();
        assert_eq!(linux.version, Version::new(0, 3, 0));
        assert!(linux.update.is_none());
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
        assert!(
            !is_newer(&Version::parse("0.2.1+rebuilt").unwrap(), &running),
            "build metadata is not a newer version"
        );
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
