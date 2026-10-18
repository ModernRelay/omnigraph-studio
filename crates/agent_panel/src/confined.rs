//! Files Studio writes into an agent's working directory, which the agent writes too (spec
//! 013). A link the agent leaves there must not steer Studio, which runs outside the agent's
//! sandbox, to write or remove files elsewhere: every directory below the working directory is
//! opened by handle, never through a link, and every file is read, written or removed by its
//! name inside such a handle. A write goes to a new file renamed into place, which replaces a
//! link of the same name instead of following it.
//!
//! The working directory itself is Studio's own (`sessions::graph_cwd`) and is opened as it is.

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

#[cfg(not(unix))]
pub use paths::Confined;

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

        fn open_dir(&self, name: &str, create: bool) -> io::Result<Self> {
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
            rustix::fs::unlinkat(&self.fd, name, AtFlags::empty())?;
            Ok(())
        }

        /// Sets this directory's own mode.
        pub fn set_mode(&self, mode: u32) -> io::Result<()> {
            rustix::fs::fchmod(&self.fd, Mode::from_raw_mode(mode as _))?;
            Ok(())
        }
    }

    fn refused(name: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{name} is a link or not what Studio expects there; Studio does not use it"),
        )
    }
}

/// Without directory handles (Windows): each part is checked before it is used. An agent
/// there runs with the person's own rights and no sandbox, so a link changes nothing it could
/// not do itself.
#[cfg(not(unix))]
mod paths {
    use super::*;
    use std::path::PathBuf;

    pub struct Confined {
        path: PathBuf,
    }

    impl Confined {
        pub fn open(path: &Path) -> io::Result<Self> {
            Ok(Self {
                path: path.to_owned(),
            })
        }

        pub fn dir(&self, name: &str) -> io::Result<Self> {
            self.open_dir(name, true)
        }

        fn open_dir(&self, name: &str, create: bool) -> io::Result<Self> {
            check_name(name)?;
            let path = self.path.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("{name} is a link or not a directory"),
                    ));
                }
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    std::fs::create_dir(&path)?;
                }
                Err(error) => return Err(error),
            }
            Ok(Self { path })
        }

        pub fn dirs(&self, path: &str) -> io::Result<Self> {
            self.walk(path, true)
        }

        pub fn existing_dirs(&self, path: &str) -> io::Result<Self> {
            self.walk(path, false)
        }

        fn walk(&self, path: &str, create: bool) -> io::Result<Self> {
            path.split('/')
                .filter(|part| !part.is_empty())
                .try_fold(Self::open(&self.path)?, |dir, part| {
                    dir.open_dir(part, create)
                })
        }

        pub fn file(&self, name: &str) -> io::Result<std::fs::File> {
            check_name(name)?;
            let path = self.path.join(name);
            if !std::fs::symlink_metadata(&path)?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{name} is not a file"),
                ));
            }
            std::fs::File::open(path)
        }

        pub fn holds(&self, name: &str, bytes: &[u8]) -> bool {
            self.file(name).is_ok_and(|file| same_bytes(file, bytes))
        }

        pub fn write(&self, name: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
            use std::io::Write as _;
            self.write_with(name, mode, |file| file.write_all(bytes))
        }

        pub fn write_with(
            &self,
            name: &str,
            _mode: u32,
            fill: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
        ) -> io::Result<()> {
            check_name(name)?;
            let temporary = self.path.join(temporary(name)?);
            let written = std::fs::File::create(&temporary)
                .and_then(|mut file| fill(&mut file))
                .and_then(|()| std::fs::rename(&temporary, self.path.join(name)));
            if written.is_err() {
                std::fs::remove_file(&temporary).ok();
            }
            written
        }

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

        pub fn remove(&self, name: &std::ffi::OsStr) -> io::Result<()> {
            std::fs::remove_file(self.path.join(name))
        }

        /// The directory inherits the person's own profile permissions.
        pub fn set_mode(&self, _mode: u32) -> io::Result<()> {
            Ok(())
        }
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("studio-confined-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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
