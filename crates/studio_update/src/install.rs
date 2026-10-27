//! Installing a downloaded update on macOS: where Studio may replace itself, the version inside
//! an update, the exchange of the running bundle with the update's, and what an interrupted
//! install leaves behind (spec 017, "Downloading and installing on macOS").

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail};

/// Why Studio cannot replace its own bundle where it runs; it offers the download instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// On a disk image or another volume (`/Volumes/…`), often read-only.
    DiskImage,
    /// Run in place from Downloads: macOS moved it to a random read-only path.
    Translocated,
    /// The person cannot write the bundle or this folder.
    NotWritable(PathBuf),
}

/// Whether Studio must not install over `bundle`: checked before the download, so a bundle
/// Studio cannot replace is offered as a download instead of being fetched in vain.
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

/// The start of the name of the folder an update is unpacked in, beside the bundle it replaces.
/// Hidden, and Studio's own, so cleaning up never touches another app's files.
const STAGING_PREFIX: &str = ".omnigraph-studio-update-";

/// Replaces `bundle` with the bundle in `archive`: unpacked into a folder beside it, written to
/// the disk, then exchanged with it in one rename, which is written to the disk too, so `bundle`
/// holds the old app or the whole new one whatever happens, a power loss included; the old one
/// is then deleted with that folder. Nothing outside the folder is written while unpacking, and
/// a failure before the exchange leaves `bundle` as it was.
pub fn replace_bundle(bundle: &Path, archive: &[u8]) -> Result<()> {
    let folder = bundle
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .context("the bundle has no folder")?;
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(folder)
        .with_context(|| format!("Studio can't write in {}", folder.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    tar.unpack(staging.path())
        .context("the update could not be unpacked")?;
    let update = only_bundle(staging.path())?;
    // Unpacking leaves the files in memory: made the bundle before they reach the disk, a
    // power loss would leave a bundle that does not launch, with the old one deleted.
    sync_tree(&update).context("the update could not be written to the disk")?;
    exchange(&update, bundle)
        .with_context(|| format!("Studio can't replace itself in {}", folder.display()))?;
    if let Err(error) = std::fs::File::open(folder).and_then(|folder| folder.sync_all()) {
        log::warn!("the bundle's exchange was not written to the disk: {error}");
    }
    // A new modification time tells Finder and Launch Services the bundle changed.
    if let Err(error) = std::fs::File::open(bundle)
        .and_then(|bundle| bundle.set_modified(std::time::SystemTime::now()))
    {
        log::warn!("the updated bundle's time was not set: {error}");
    }
    // `update` now holds the old app; deleting the folder deletes it. If that fails, the next
    // launch removes what is left ([`clean_leftovers`]).
    if let Err(error) = staging.close() {
        log::warn!("the old bundle was not deleted: {error}");
    }
    Ok(())
}

/// Writes every file and folder under `root` to the disk (`sync_all`, a full sync on macOS),
/// the folders last so their entries name what is there. A link is not followed.
fn sync_tree(root: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            std::fs::File::open(entry.path())?.sync_all()?;
        }
    }
    std::fs::File::open(root)?.sync_all()
}

/// The one app bundle an update unpacks to: a real directory named `*.app`, never a link.
fn only_bundle(staging: &Path) -> Result<PathBuf> {
    let entries = std::fs::read_dir(staging)?.collect::<std::io::Result<Vec<_>>>()?;
    match entries.as_slice() {
        [entry]
            if entry.file_type()?.is_dir()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "app") =>
        {
            Ok(entry.path())
        }
        _ => bail!("the update does not hold exactly one app bundle"),
    }
}

/// Swaps two paths in one rename (`renamex_np` with `RENAME_SWAP` on macOS, `renameat2` with
/// `RENAME_EXCHANGE` on Linux): both exist before and after, each under the other's name.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE).map_err(std::io::Error::from)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exchange(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "Studio replaces itself only on macOS",
    ))
}

/// How long a leftover is kept: an install under way is never that old.
const LEFTOVER_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Removes the update folders in `folder` (the bundle's) older than a day, left only when
/// Studio stopped during an install or could not delete the old bundle, and says how many.
pub fn clean_leftovers(folder: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGING_PREFIX)
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
        let folder = tempfile::tempdir().unwrap();
        for name in [
            ".omnigraph-studio-update-A1b2C3",
            ".omnigraph-studio-update-D4e5F6",
            "packager_current_appG7h8I9",
            "Omnigraph Studio.app",
        ] {
            std::fs::create_dir(folder.path().join(name)).unwrap();
        }
        let now = SystemTime::now();
        assert_eq!(
            clean_leftovers(folder.path(), now),
            0,
            "a day has not passed"
        );
        let tomorrow = now + Duration::from_secs(25 * 60 * 60);
        assert_eq!(clean_leftovers(folder.path(), tomorrow), 2);
        let mut left: Vec<String> = std::fs::read_dir(folder.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["Omnigraph Studio.app", "packager_current_appG7h8I9"],
            "another app's folders and the bundle stay"
        );
    }

    /// A folder holding a bundle whose executable says `build`, and that bundle.
    fn installed(build: &str) -> (tempfile::TempDir, PathBuf) {
        let folder = tempfile::tempdir().unwrap();
        let bundle = folder.path().join("Omnigraph Studio.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::write(bundle.join("Contents/MacOS/omnigraph-studio"), build).unwrap();
        std::fs::write(bundle.join("Contents/Old.txt"), "only in the old build").unwrap();
        (folder, bundle)
    }

    fn folder_names(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_update_takes_the_bundles_place_and_the_old_one_is_deleted() {
        let (folder, bundle) = installed("the old build");
        let update = archive(&[
            (
                "Omnigraph Studio.app/Contents/MacOS/omnigraph-studio",
                "the new build",
            ),
            ("Omnigraph Studio.app/Contents/Info.plist", &plist("0.3.0")),
        ]);
        replace_bundle(&bundle, &update).unwrap();
        assert_eq!(
            std::fs::read_to_string(bundle.join("Contents/MacOS/omnigraph-studio")).unwrap(),
            "the new build"
        );
        assert!(!bundle.join("Contents/Old.txt").exists());
        assert_eq!(folder_names(folder.path()), ["Omnigraph Studio.app"]);
    }

    /// Every file and folder of a tree is written to the disk, a link left as it is: bundles
    /// hold links (a framework's `Versions/Current`).
    #[cfg(unix)]
    #[test]
    fn a_tree_is_written_to_the_disk_without_following_its_links() {
        let root = tempfile::tempdir().unwrap();
        let inner = root
            .path()
            .join("Contents/Frameworks/Kit.framework/Versions/A");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("Kit"), "binary").unwrap();
        std::os::unix::fs::symlink("A", inner.parent().unwrap().join("Current")).unwrap();
        std::os::unix::fs::symlink("/nowhere", root.path().join("dangling")).unwrap();
        sync_tree(root.path()).unwrap();
    }

    #[test]
    fn an_update_that_does_not_unpack_leaves_the_bundle_as_it_was() {
        let (folder, bundle) = installed("the old build");
        assert!(replace_bundle(&bundle, b"not gzip").is_err());
        assert_eq!(
            std::fs::read_to_string(bundle.join("Contents/MacOS/omnigraph-studio")).unwrap(),
            "the old build"
        );
        assert_eq!(folder_names(folder.path()), ["Omnigraph Studio.app"]);
    }

    #[test]
    fn an_update_without_exactly_one_bundle_is_not_installed() {
        let (folder, bundle) = installed("the old build");
        for update in [
            archive(&[
                ("Omnigraph Studio.app/Contents/Info.plist", &plist("0.3.0")),
                ("Other.app/Contents/Info.plist", &plist("0.3.0")),
            ]),
            archive(&[("Omnigraph Studio/Contents/Info.plist", &plist("0.3.0"))]),
        ] {
            assert!(replace_bundle(&bundle, &update).is_err());
        }
        assert_eq!(
            std::fs::read_to_string(bundle.join("Contents/MacOS/omnigraph-studio")).unwrap(),
            "the old build"
        );
        assert_eq!(folder_names(folder.path()), ["Omnigraph Studio.app"]);
    }

    /// An entry naming `..` is never written, so an update cannot place a file beside the
    /// bundle, whatever it holds.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_entry_outside_the_update_is_not_written() {
        let (folder, bundle) = installed("the old build");
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let content = "the new build";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(
            &mut header,
            "Omnigraph Studio.app/Contents/MacOS/omnigraph-studio",
            content.as_bytes(),
        )
        .unwrap();
        // `append_data` refuses `..`; a raw header is what a hostile archive carries.
        let escape = "../escaped.txt";
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..escape.len()].copy_from_slice(escape.as_bytes());
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, content.as_bytes()).unwrap();
        let mut gz = tar.into_inner().unwrap();
        gz.flush().unwrap();
        replace_bundle(&bundle, &gz.finish().unwrap()).unwrap();
        assert_eq!(folder_names(folder.path()), ["Omnigraph Studio.app"]);
    }
}
