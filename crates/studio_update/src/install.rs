//! Installing a downloaded update on macOS: where Studio may replace itself, the version inside
//! an update, and what an interrupted install leaves behind (spec 017, "Downloading and
//! installing on macOS").

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail};

/// Why Studio cannot replace its own bundle where it runs; it offers the download instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// On a disk image or another volume (`/Volumes/…`): the swap's renames cannot cross to it.
    DiskImage,
    /// Run in place from Downloads: macOS moved it to a random read-only path.
    Translocated,
    /// The person cannot write the bundle or this folder.
    NotWritable(PathBuf),
}

/// Whether Studio must not install over `bundle`. Where it may, the crate's install renames
/// the bundle and never needs the administrator prompt it would otherwise run.
pub fn blocked(bundle: &Path) -> Option<Blocked> {
    if bundle.starts_with("/Volumes") {
        return Some(Blocked::DiskImage);
    }
    if bundle
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation")
    {
        return Some(Blocked::Translocated);
    }
    let folder = bundle.parent().unwrap_or(bundle);
    if !writable(bundle) || !writable(folder) {
        return Some(Blocked::NotWritable(folder.to_path_buf()));
    }
    None
}

#[cfg(unix)]
fn writable(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_ok()
}

#[cfg(not(unix))]
fn writable(_: &Path) -> bool {
    true
}

/// The most of an `Info.plist` read.
const MAX_PLIST_BYTES: u64 = 1024 * 1024;

/// The `CFBundleShortVersionString` of the bundle in an update archive, read in memory before
/// anything is unpacked: the top bundle's `Contents/Info.plist`, never a nested bundle's.
pub fn bundle_version(archive: &[u8]) -> Result<String> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().context("the update is not a tar archive")? {
        let entry = entry.context("the update is not a gzip tar archive")?;
        let path = entry.path()?.into_owned();
        let parts: Vec<&std::ffi::OsStr> = path
            .components()
            .filter_map(|part| match part {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        let own_plist = matches!(parts.as_slice(), [bundle, contents, plist]
            if bundle.to_string_lossy().ends_with(".app")
                && *contents == "Contents"
                && *plist == "Info.plist");
        if own_plist {
            let mut text = String::new();
            entry
                .take(MAX_PLIST_BYTES)
                .read_to_string(&mut text)
                .context("the update's Info.plist is not text")?;
            return plist_string(&text, "CFBundleShortVersionString")
                .context("the update's Info.plist names no version");
        }
    }
    bail!("the update holds no bundle's Contents/Info.plist")
}

/// A string value of an XML property list's top dictionary, as `script/bundle-mac` writes it.
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let (_, after) = plist.split_once(&format!("<key>{key}</key>"))?;
    let value = after.trim_start().strip_prefix("<string>")?;
    let (value, _) = value.split_once("</string>")?;
    Some(value.trim().to_owned()).filter(|value| !value.is_empty())
}

/// The temporary directories `cargo-packager-updater` installs through, left behind only when
/// Studio stopped during an install.
const LEFTOVER_PREFIXES: [&str; 2] = ["packager_current_app", "packager_updated_app"];

/// How long a leftover is kept: an install under way is never that old.
const LEFTOVER_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Removes the updater's directories in `temp` older than a day, and says how many.
pub fn clean_leftovers(temp: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            LEFTOVER_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| {
                    now.duration_since(modified).unwrap_or_default() >= LEFTOVER_AGE
                })
        })
        .filter(|entry| std::fs::remove_dir_all(entry.path()).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn plist(version: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>Omnigraph Studio</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
</dict>
</plist>
"#
        )
    }

    /// An update archive as `script/bundle-mac` writes it: the bundle is its top directory.
    fn archive(files: &[(&str, &str)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (path, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, content.as_bytes())
                .unwrap();
        }
        let mut gz = tar.into_inner().unwrap();
        gz.flush().unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn the_version_comes_from_the_bundles_info_plist() {
        let bytes = archive(&[
            (
                "Omnigraph Studio.app/Contents/MacOS/omnigraph-studio",
                "binary",
            ),
            ("Omnigraph Studio.app/Contents/Info.plist", &plist("0.3.0")),
        ]);
        assert_eq!(bundle_version(&bytes).unwrap(), "0.3.0");
    }

    #[test]
    fn an_archive_without_a_bundle_version_is_refused() {
        let no_plist = archive(&[("Omnigraph Studio.app/Contents/MacOS/omnigraph-studio", "x")]);
        assert!(bundle_version(&no_plist).is_err());
        let no_key = archive(&[(
            "Omnigraph Studio.app/Contents/Info.plist",
            "<plist><dict><key>CFBundleName</key><string>x</string></dict></plist>",
        )]);
        assert!(bundle_version(&no_key).is_err());
        assert!(bundle_version(b"not gzip").is_err());
    }

    #[test]
    fn a_nested_bundles_plist_is_not_the_apps() {
        let bytes = archive(&[(
            "Omnigraph Studio.app/Contents/Frameworks/Helper.app/Contents/Info.plist",
            &plist("9.9.9"),
        )]);
        assert!(bundle_version(&bytes).is_err());
    }

    #[test]
    fn a_disk_image_or_a_translocated_bundle_is_not_replaced() {
        assert_eq!(
            blocked(Path::new("/Volumes/Omnigraph Studio/Omnigraph Studio.app")),
            Some(Blocked::DiskImage)
        );
        assert_eq!(
            blocked(Path::new(
                "/private/var/folders/x1/T/AppTranslocation/0A1B/d/Omnigraph Studio.app"
            )),
            Some(Blocked::Translocated)
        );
    }

    #[test]
    fn a_bundle_in_a_folder_the_person_owns_is_replaced() {
        let folder = tempfile::tempdir().unwrap();
        let bundle = folder.path().join("Omnigraph Studio.app");
        std::fs::create_dir(&bundle).unwrap();
        assert_eq!(blocked(&bundle), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_bundle_in_a_folder_the_person_cannot_write_is_not_replaced() {
        use std::os::unix::fs::PermissionsExt as _;
        let folder = tempfile::tempdir().unwrap();
        let bundle = folder.path().join("Omnigraph Studio.app");
        std::fs::create_dir(&bundle).unwrap();
        std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = blocked(&bundle);
        std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            result,
            Some(Blocked::NotWritable(folder.path().to_path_buf()))
        );
    }

    #[test]
    fn leftovers_older_than_a_day_are_removed() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "packager_current_appA1b2C3",
            "packager_updated_appD4e5F6",
            "something_else",
        ] {
            std::fs::create_dir(temp.path().join(name)).unwrap();
        }
        let now = SystemTime::now();
        assert_eq!(clean_leftovers(temp.path(), now), 0, "a day has not passed");
        let tomorrow = now + Duration::from_secs(25 * 60 * 60);
        assert_eq!(clean_leftovers(temp.path(), tomorrow), 2);
        let left: Vec<String> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["something_else"]);
    }
}
