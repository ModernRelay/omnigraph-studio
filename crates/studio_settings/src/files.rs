//! Writing Studio's files so a crash or a full disk never leaves one empty or half written: the
//! new contents go to a temporary file beside the target, which is synced and then renamed over
//! it. A rename within one directory replaces the file in one step, so a reader sees the old
//! file or the new one.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Who may read a written file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Only the person (`0600`): Studio's own state.
    Private,
    /// As the file was, or as a new file is by default: a file the person chose.
    Keep,
}

/// One write waiting its turn, or a mark that everything asked for before it is done.
enum Job {
    Write(PathBuf, Vec<u8>, Access),
    Done(std::sync::mpsc::Sender<()>),
}

/// The thread Studio's own state files are written on, one at a time in the order asked: a
/// sync to disk can take tens of milliseconds, too long for the main thread, and a later save of
/// a file must never land before an earlier one.
fn writer() -> &'static std::sync::mpsc::Sender<Job> {
    static WRITER: std::sync::OnceLock<std::sync::mpsc::Sender<Job>> = std::sync::OnceLock::new();
    WRITER.get_or_init(|| {
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("studio-writes".into())
            .spawn(move || {
                for job in queue {
                    match job {
                        Job::Write(path, contents, access) => {
                            write_atomic(&path, &contents, access).ok();
                        }
                        Job::Done(done) => {
                            done.send(()).ok();
                        }
                    }
                }
            })
            .expect("the write thread starts");
        jobs
    })
}

/// [`write_atomic`] on the write thread, after every write asked for before it. What fails is
/// left as it was, as a failed [`write_atomic`] leaves it.
pub fn write_atomic_in_turn(path: PathBuf, contents: Vec<u8>, access: Access) {
    writer().send(Job::Write(path, contents, access)).ok();
}

/// Waits, a few seconds at most, until every write asked for so far is done: at quit, so the
/// last state is on disk.
pub fn writes_done() {
    let (done, wait) = std::sync::mpsc::channel();
    if writer().send(Job::Done(done)).is_ok() {
        wait.recv_timeout(std::time::Duration::from_secs(5)).ok();
    }
}

/// Replaces `path` with `contents` in one step. A link is written through to its target, so the
/// link stays. A failed write leaves the old file as it was and no temporary file behind.
pub fn write_atomic(path: &Path, contents: &[u8], access: Access) -> std::io::Result<()> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = target
        .file_name()
        .ok_or_else(|| std::io::Error::other("the path names no file"))?;
    let temp = dir.join(format!(
        ".{}.{}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let existing = std::fs::metadata(&target).ok();
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if access == Access::Private {
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        }
        let mut file = options.open(&temp)?;
        if access == Access::Keep
            && let Some(existing) = &existing
        {
            file.set_permissions(existing.permissions())?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temp, &target)
    })();
    if written.is_err() {
        std::fs::remove_file(&temp).ok();
    }
    written
}

/// Moves a file Studio could not read out of the way, to `<name>.unreadable-<seconds>` beside
/// it, so the next save does not overwrite what the person may want back. Returns where it went.
pub fn set_aside(path: &Path) -> Option<PathBuf> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let name = path.file_name()?.to_string_lossy();
    let aside = path.with_file_name(format!("{name}.unreadable-{seconds}"));
    std::fs::rename(path, &aside).ok()?;
    Some(aside)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("studio-files-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn replaces_the_whole_file_and_leaves_nothing_behind() {
        let dir = scratch("replace");
        let path = dir.join("state.json");
        std::fs::write(&path, "a much longer old text than the new one").unwrap();
        write_atomic(&path, b"new", Access::Private).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(leftovers(&dir).is_empty());
        // A missing directory is made.
        write_atomic(&dir.join("deeper/state.json"), b"x", Access::Private).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_the_owners_and_kept_files_keep_their_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("modes");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let private = dir.join("private.json");
        std::fs::write(&private, "old").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&private, b"new", Access::Private).unwrap();
        assert_eq!(mode(&private), 0o600);

        let chosen = dir.join("query.gq");
        std::fs::write(&chosen, "old").unwrap();
        std::fs::set_permissions(&chosen, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&chosen, b"new", Access::Keep).unwrap();
        assert_eq!(mode(&chosen), 0o640);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_written_through_and_stays_a_link() {
        let dir = scratch("link");
        let real = dir.join("real.json");
        std::fs::write(&real, "old").unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"new", Access::Keep).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_write_keeps_the_old_file() {
        let dir = scratch("fail");
        // The target is a directory, so the rename fails after the temporary file is written.
        let path = dir.join("taken");
        std::fs::create_dir_all(path.join("inside")).unwrap();
        assert!(write_atomic(&path, b"new", Access::Private).is_err());
        assert!(path.join("inside").is_dir());
        assert!(leftovers(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_file_is_set_aside_beside_itself() {
        let dir = scratch("aside");
        let path = dir.join("session.json");
        std::fs::write(&path, "{ not json").unwrap();
        let aside = set_aside(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(aside.parent(), Some(dir.as_path()));
        assert!(
            aside
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("session.json.unreadable-")
        );
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{ not json");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writes_in_turn_land_in_the_order_asked() {
        let dir = scratch("in-turn");
        let path = dir.join("state.json");
        for n in 0..50 {
            write_atomic_in_turn(path.clone(), format!("{n}").into_bytes(), Access::Private);
        }
        writes_done();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "49", "the last asked, last");
        std::fs::remove_dir_all(&dir).ok();
    }
}
