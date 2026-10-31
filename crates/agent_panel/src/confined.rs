//! Files Studio writes into an agent's working directory, which the agent writes too (spec
//! 013). A link the agent leaves there must not steer Studio, which runs outside the agent's
//! sandbox, to write or remove files elsewhere: every directory below the working directory is
//! opened by handle, never through a link, and every file is read, written or removed by its
//! name inside such a handle. A write goes to a new file renamed into place, which replaces a
//! link of the same name instead of following it. Unix opens each name relative to its
//! directory's handle (`openat`); Windows holds each directory's handle so that its path keeps
//! naming it (`pinned`).
//!
//! The working directory itself is Studio's own (`sessions::working_dir`) and is opened as it is.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// What an entry of a confined directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    /// A file, a link or anything else: removed by its name, never followed.
    Other,
}

#[cfg(unix)]
pub use unix::Confined;

#[cfg(windows)]
pub use pinned::Confined;

/// What a confined directory offers on both platforms, over each one's `open_dir`.
#[cfg(any(unix, windows))]
impl Confined {
    /// The directories along `path` (`a/b/c`), each opened as [`Confined::dir`] opens it.
    pub fn dirs(&self, path: &str) -> io::Result<Self> {
        self.walk(path, true)
    }

    /// The directories along `path`, which must exist.
    pub fn existing_dirs(&self, path: &str) -> io::Result<Self> {
        self.walk(path, false)
    }

    fn walk(&self, path: &str, create: bool) -> io::Result<Self> {
        let mut parts = path.split('/').filter(|part| !part.is_empty());
        let first = parts
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "an empty path"))?;
        parts.try_fold(self.open_dir(first, create)?, |dir, part| {
            dir.open_dir(part, create)
        })
    }
}

/// `len` random bytes as hex: a name no one else picks, or a token no one guesses.
pub(crate) fn random_hex(len: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `file` holds exactly `bytes`: its size first, so a large file is not read.
fn same_bytes(file: std::fs::File, bytes: &[u8]) -> bool {
    use std::io::Read as _;
    if !file
        .metadata()
        .is_ok_and(|meta| meta.len() == bytes.len() as u64)
    {
        return false;
    }
    let mut current = Vec::with_capacity(bytes.len());
    file.take(bytes.len() as u64 + 1)
        .read_to_end(&mut current)
        .is_ok()
        && current == bytes
}

/// A name for a file being written before it is renamed into place.
fn temporary(name: &str) -> io::Result<String> {
    Ok(format!(".{name}.studio-{}", random_hex(8)?))
}

#[cfg(unix)]
mod unix {
    use super::*;
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::io::Write as _;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt as _;

    /// A directory opened by handle.
    pub struct Confined {
        fd: OwnedFd,
    }

    const DIR: OFlags = OFlags::DIRECTORY
        .union(OFlags::RDONLY)
        .union(OFlags::CLOEXEC)
        .union(OFlags::NOFOLLOW);

    impl Confined {
        /// `path`, a directory Studio made, opened as it is.
        pub fn open(path: &Path) -> io::Result<Self> {
            let fd = rustix::fs::open(path, DIR.difference(OFlags::NOFOLLOW), Mode::empty())?;
            Ok(Self { fd })
        }

        /// The directory `name` inside this one, made when it is missing. A link, or anything
        /// that is not a directory, is refused.
        pub fn dir(&self, name: &str) -> io::Result<Self> {
            self.open_dir(name, true)
        }

        pub(super) fn open_dir(&self, name: &str, create: bool) -> io::Result<Self> {
            check_name(name)?;
            match rustix::fs::openat(&self.fd, name, DIR, Mode::empty()) {
                Ok(fd) => Ok(Self { fd }),
                Err(Errno::NOENT) if create => {
                    match rustix::fs::mkdirat(&self.fd, name, Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => return Err(error.into()),
                    }
                    let fd = rustix::fs::openat(&self.fd, name, DIR, Mode::empty())?;
                    Ok(Self { fd })
                }
                Err(Errno::LOOP | Errno::NOTDIR) => Err(refused(name)),
                Err(error) => Err(error.into()),
            }
        }

        /// The regular file `name`, opened for reading; a link, a pipe or anything else is
        /// refused. Opened without blocking, so a pipe left there does not hold Studio.
        pub fn file(&self, name: &str) -> io::Result<std::fs::File> {
            check_name(name)?;
            let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
            let fd = match rustix::fs::openat(&self.fd, name, flags, Mode::empty()) {
                Ok(fd) => fd,
                Err(Errno::LOOP) => return Err(refused(name)),
                Err(error) => return Err(error.into()),
            };
            if FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode) != FileType::RegularFile {
                return Err(refused(name));
            }
            Ok(std::fs::File::from(fd))
        }

        /// Whether the file `name` holds exactly `bytes`; a link is not read through.
        pub fn holds(&self, name: &str, bytes: &[u8]) -> bool {
            self.file(name).is_ok_and(|file| same_bytes(file, bytes))
        }

        /// Writes `bytes` as the file `name` with `mode`, through a new file renamed over it: a
        /// link of that name is replaced, never followed, and readers never see half a file.
        pub fn write(&self, name: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
            self.write_with(name, mode, |file| file.write_all(bytes))
        }

        /// [`Confined::write`] with what `fill` writes into the new file.
        pub fn write_with(
            &self,
            name: &str,
            mode: u32,
            fill: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
        ) -> io::Result<()> {
            check_name(name)?;
            let temporary = temporary(name)?;
            let flags =
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW;
            let mode = Mode::from_raw_mode(mode as _);
            let fd = rustix::fs::openat(&self.fd, temporary.as_str(), flags, mode)?;
            let written = (|| {
                // The mode as asked, whatever the umask.
                rustix::fs::fchmod(&fd, mode)?;
                fill(&mut std::fs::File::from(fd))?;
                rustix::fs::renameat(&self.fd, temporary.as_str(), &self.fd, name)?;
                Ok(())
            })();
            if written.is_err() {
                rustix::fs::unlinkat(&self.fd, temporary.as_str(), AtFlags::empty()).ok();
            }
            written
        }

        /// The entries, by name, without `.` and `..`.
        pub fn entries(&self) -> io::Result<Vec<(OsString, Kind)>> {
            let mut out = Vec::new();
            for entry in Dir::read_from(&self.fd)? {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let kind = match entry.file_type() {
                    FileType::Directory => Kind::Dir,
                    FileType::Unknown => {
                        // Some file systems leave the type out: asked without following.
                        let stat = rustix::fs::statat(
                            &self.fd,
                            entry.file_name(),
                            AtFlags::SYMLINK_NOFOLLOW,
                        )?;
                        match FileType::from_raw_mode(stat.st_mode as _) {
                            FileType::Directory => Kind::Dir,
                            _ => Kind::Other,
                        }
                    }
                    _ => Kind::Other,
                };
                out.push((std::ffi::OsStr::from_bytes(name).to_owned(), kind));
            }
            Ok(out)
        }

        /// Removes the file or link `name`; a link's target is never touched.
        pub fn remove(&self, name: &std::ffi::OsStr) -> io::Result<()> {
            check_os_name(name)?;
            rustix::fs::unlinkat(&self.fd, name, AtFlags::empty())?;
            Ok(())
        }

        /// Removes the directory `name` when it is empty; one holding anything stays.
        pub fn remove_dir(&self, name: &str) -> io::Result<()> {
            check_name(name)?;
            rustix::fs::unlinkat(&self.fd, name, AtFlags::REMOVEDIR)?;
            Ok(())
        }

        /// Sets this directory's own mode.
        pub fn set_mode(&self, mode: u32) -> io::Result<()> {
            rustix::fs::fchmod(&self.fd, Mode::from_raw_mode(mode as _))?;
            Ok(())
        }
    }
}

/// Windows opens no name relative to a directory's handle through the standard library, so
/// Studio holds the handle instead: every directory from the working directory down is opened
/// as a handle that shares reading and writing but not deleting, so nobody can rename or remove
/// it, or swap a link in for it, while Studio works below it, and the path Studio joins a name
/// to names the directory it checked. Each handle below the working directory is opened with
/// `FILE_FLAG_OPEN_REPARSE_POINT`, so a symbolic link or a junction opens as itself and is
/// refused as no directory, never followed. A file is opened the same way and refused unless it
/// is a file. A Windows agent may run in a sandbox of its own, as Codex's can, so a link is
/// refused there for the same reason as on Unix.
#[cfg(windows)]
mod pinned {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::PathBuf;
    use std::sync::Arc;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    /// A directory held open, with every directory above it up to the one Studio opened.
    pub struct Confined {
        path: PathBuf,
        held: Vec<Arc<File>>,
    }

    /// `path` held as a directory handle that keeps it from being renamed or removed. A link at
    /// its last part is followed when `follow` (the working directory, Studio's own) and opened
    /// as itself otherwise.
    fn hold(path: &Path, follow: bool) -> io::Result<File> {
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        // Reading the directory takes data access, which is what the sharing check holds
        // others' opens to: a handle with none would not keep a rename out.
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(flags)
            .open(path)
    }

    impl Confined {
        /// `path`, a directory Studio made, opened as it is.
        pub fn open(path: &Path) -> io::Result<Self> {
            let held = hold(path, true)?;
            if !held.metadata()?.is_dir() {
                return Err(refused(&path.to_string_lossy()));
            }
            Ok(Self {
                path: path.to_owned(),
                held: vec![Arc::new(held)],
            })
        }

        /// The directory `name` inside this one, made when it is missing. A link, or anything
        /// that is not a directory, is refused.
        pub fn dir(&self, name: &str) -> io::Result<Self> {
            self.open_dir(name, true)
        }

        pub(super) fn open_dir(&self, name: &str, create: bool) -> io::Result<Self> {
            check_name(name)?;
            let path = self.path.join(name);
            let held = match hold(&path, false) {
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    match std::fs::create_dir(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    hold(&path, false)?
                }
                held => held?,
            };
            // A link opened as itself is no directory to the standard library.
            if !held.metadata()?.is_dir() {
                return Err(refused(name));
            }
            let mut chain = self.held.clone();
            chain.push(Arc::new(held));
            Ok(Self { path, held: chain })
        }

        /// The regular file `name`, opened for reading; a link or anything else is refused.
        pub fn file(&self, name: &str) -> io::Result<File> {
            check_name(name)?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(self.path.join(name))?;
            if !file.metadata()?.is_file() {
                return Err(refused(name));
            }
            Ok(file)
        }

        /// Whether the file `name` holds exactly `bytes`; a link is not read through.
        pub fn holds(&self, name: &str, bytes: &[u8]) -> bool {
            self.file(name).is_ok_and(|file| same_bytes(file, bytes))
        }

        /// Writes `bytes` as the file `name`, through a new file renamed over it: a link of that
        /// name is replaced, never followed, and readers never see half a file.
        pub fn write(&self, name: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
            use std::io::Write as _;
            self.write_with(name, mode, |file| file.write_all(bytes))
        }

        /// [`Confined::write`] with what `fill` writes into the new file. Windows has no mode
        /// bits: the file takes the access its folder gives.
        pub fn write_with(
            &self,
            name: &str,
            _mode: u32,
            fill: impl FnOnce(&mut File) -> io::Result<()>,
        ) -> io::Result<()> {
            check_name(name)?;
            let temporary = self.path.join(temporary(name)?);
            // The new file is closed before it is renamed.
            let written = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .and_then(|mut file| fill(&mut file))
                .and_then(|()| std::fs::rename(&temporary, self.path.join(name)));
            if written.is_err() {
                std::fs::remove_file(&temporary).ok();
            }
            written
        }

        /// The entries, by name. A link or a junction is listed as no directory.
        pub fn entries(&self) -> io::Result<Vec<(OsString, Kind)>> {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(&self.path)? {
                let entry = entry?;
                let kind = if entry.file_type()?.is_dir() {
                    Kind::Dir
                } else {
                    Kind::Other
                };
                out.push((entry.file_name(), kind));
            }
            Ok(out)
        }

        /// Removes the file or link `name`; a link's target is never touched. A link to a
        /// folder, a junction included, is a directory to Windows, which removing a file
        /// refuses: it is removed as one, which removes the link itself, whatever it points at.
        pub fn remove(&self, name: &std::ffi::OsStr) -> io::Result<()> {
            use std::os::windows::fs::FileTypeExt as _;
            check_os_name(name)?;
            let path = self.path.join(name);
            if std::fs::symlink_metadata(&path)?
                .file_type()
                .is_symlink_dir()
            {
                std::fs::remove_dir(&path)
            } else {
                std::fs::remove_file(&path)
            }
        }

        /// Removes the directory `name` when it is empty; one holding anything stays. A link to
        /// a folder, a junction included, is removed as a link, whatever it points at.
        pub fn remove_dir(&self, name: &str) -> io::Result<()> {
            check_name(name)?;
            std::fs::remove_dir(self.path.join(name))
        }

        /// Windows has no mode bits: a folder takes the access its parent gives, which under the
        /// person's profile (Studio's data directory) is the person's, the system's and
        /// administrators' only.
        pub fn set_mode(&self, _mode: u32) -> io::Result<()> {
            Ok(())
        }
    }
}

/// Why a name was not used: it is a link, or not the kind of entry Studio expects there.
#[cfg(any(unix, windows))]
fn refused(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{name} is a link or not what Studio expects there; Studio does not use it"),
    )
}

/// [`check_name`] for a name an entry listing gave, which may not be text: it names one entry of
/// the folder, never the folder itself, its parent or a path. A `\` parts a path on Windows
/// alone; elsewhere it is a name's own.
fn check_os_name(name: &std::ffi::OsStr) -> io::Result<()> {
    let bytes = name.as_encoded_bytes();
    let parts = |byte: &u8| *byte == b'/' || *byte == 0 || (cfg!(windows) && *byte == b'\\');
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.iter().any(parts) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a file name"),
        ));
    }
    Ok(())
}

/// A name of one part: no separator, not `.` or `..`.
fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a file name"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("studio-confined-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// While Studio works in a folder of the agent's, the agent cannot swap the folder for a
    /// junction to elsewhere: the folder is held. A junction left in place, which any user may
    /// make, is refused at any depth, and nothing is written through it.
    #[cfg(windows)]
    #[test]
    fn a_held_folder_cannot_be_swapped_and_a_junction_is_not_followed() {
        let root = scratch("held");
        let outside = scratch("held-outside");
        std::fs::write(outside.join("keep.txt"), "mine").unwrap();
        let cwd = Confined::open(&root).unwrap();

        let held = cwd.dirs("a/b").unwrap();
        for folder in ["a", "a\\b"] {
            assert!(
                std::fs::rename(root.join(folder), root.join("moved")).is_err(),
                "{folder} was renamed while held"
            );
        }
        held.write("f.txt", b"one", 0o644).unwrap();
        held.write("f.txt", b"two", 0o644).unwrap();
        assert_eq!(std::fs::read(root.join("a/b/f.txt")).unwrap(), b"two");
        assert!(held.holds("f.txt", b"two") && !held.holds("f.txt", b"one"));
        assert!(
            held.entries()
                .unwrap()
                .iter()
                .all(|(name, _)| !name.to_string_lossy().contains(".studio-")),
            "no temporary file is left behind"
        );
        drop(held);
        std::fs::rename(root.join("a"), root.join("moved")).unwrap();

        let junction = |at: &std::path::Path| {
            let made = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(at)
                .arg(&outside)
                .output()
                .unwrap();
            assert!(made.status.success(), "{made:?}");
        };
        junction(&root.join("linked"));
        assert!(cwd.dir("linked").is_err());
        assert!(cwd.dirs("linked/x").is_err());
        assert!(cwd.file("linked").is_err());
        std::fs::create_dir(root.join("inner")).unwrap();
        junction(&root.join("inner").join("deeper"));
        assert!(cwd.dirs("inner/deeper").is_err());
        assert!(!outside.join("x").exists());
        let entries = cwd.entries().unwrap();
        assert!(
            entries.contains(&("linked".into(), Kind::Other)),
            "{entries:?}"
        );
        assert!(
            entries.contains(&("moved".into(), Kind::Dir)),
            "{entries:?}"
        );
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"mine");

        // A junction is removed as the link it is, and what it points at stays.
        cwd.remove("linked".as_ref()).unwrap();
        cwd.dir("inner").unwrap().remove("deeper".as_ref()).unwrap();
        for link in [root.join("linked"), root.join("inner").join("deeper")] {
            assert!(std::fs::symlink_metadata(&link).is_err(), "{link:?}");
        }
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"mine");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn writes_stay_inside_and_links_are_not_followed() {
        let root = scratch("links");
        let outside = scratch("links-outside");
        std::fs::write(outside.join("keep.txt"), "mine").unwrap();
        let cwd = Confined::open(&root).unwrap();

        // A directory made and written to.
        cwd.dirs("a/b")
            .unwrap()
            .write("f.txt", b"one", 0o644)
            .unwrap();
        assert_eq!(std::fs::read(root.join("a/b/f.txt")).unwrap(), b"one");
        assert!(cwd.dirs("a/b").unwrap().holds("f.txt", b"one"));
        assert!(!cwd.dirs("a/b").unwrap().holds("f.txt", b"on"));
        // A pipe is refused without waiting for a writer.
        assert!(
            std::process::Command::new("mkfifo")
                .arg(root.join("a/pipe"))
                .status()
                .unwrap()
                .success()
        );
        assert!(cwd.dir("a").unwrap().file("pipe").is_err());

        // A link to a directory outside is refused, at any depth.
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        assert!(cwd.dir("linked").is_err());
        assert!(cwd.dirs("linked/x").is_err());
        std::os::unix::fs::symlink(&outside, root.join("a/inner")).unwrap();
        assert!(cwd.dirs("a/inner").is_err());

        // A link to a file outside is replaced by the write, its target left as it was.
        std::os::unix::fs::symlink(outside.join("keep.txt"), root.join("a/note.txt")).unwrap();
        let a = cwd.dir("a").unwrap();
        assert!(a.file("note.txt").is_err(), "a link is not read through");
        assert!(!a.holds("note.txt", b"mine"));
        a.write("note.txt", b"studio", 0o600).unwrap();
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(root.join("a/note.txt")).unwrap(), b"studio");
        let meta = std::fs::symlink_metadata(root.join("a/note.txt")).unwrap();
        assert!(meta.is_file());
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        // Removing a link removes the link.
        let entries = a.entries().unwrap();
        assert!(
            entries.contains(&("inner".into(), Kind::Other)),
            "{entries:?}"
        );
        assert!(entries.contains(&("b".into(), Kind::Dir)), "{entries:?}");
        a.remove("inner".as_ref()).unwrap();
        assert!(outside.join("keep.txt").exists());
        assert!(!root.join("a/inner").exists());

        // No temporary file is left behind.
        assert!(
            a.entries()
                .unwrap()
                .iter()
                .all(|(name, _)| { !name.to_string_lossy().contains(".studio-") })
        );
        assert!(check_name("../x").is_err() && check_name("a/b").is_err());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
