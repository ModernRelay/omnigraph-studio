//! Where an agent's program is found (spec 011). Studio started from the Dock or Finder inherits
//! launchd's PATH, `/usr/bin:/bin:/usr/sbin:/sbin`, where neither `npx` nor `node` nor `codex`
//! is: Homebrew, mise, nvm, Volta and npm put them in folders the person's login shell adds.
//! So an agent runs with the PATH a terminal would give it: the login shell's, read once off
//! the main thread within [`SHELL_WITHIN`], then Studio's own, then the usual install folders
//! ([`usual_folders`]), each absolute folder once. A PATH in the agent's settings (`agent.env`)
//! is used as it is.
//!
//! A program named without a folder is looked for on that PATH before the agent starts, so a
//! missing one is named ([`missing_message`]) rather than reported as the system's "No such file
//! or directory". On Unix the agent then starts by the path found; Windows keeps its launcher
//! (`launch.rs`), which finds the program the same way.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// How long the login shell has to print its PATH. A shell whose startup files wait on the
/// network or a prompt is given up on, and the agent runs with Studio's PATH and the usual
/// folders.
const SHELL_WITHIN: Duration = Duration::from_secs(5);
/// Brackets the PATH in the shell's output, which an interactive shell's startup files may
/// print to as well.
const MARK: &str = "_omnigraph_studio_path_";

/// Starts reading the login shell's PATH on a thread of its own, so the first agent seldom
/// waits for it.
pub(crate) fn warm() {
    std::thread::Builder::new()
        .name("agent-path".into())
        .spawn(|| {
            agent_path();
        })
        .ok();
}

/// The PATH an agent runs with when its settings set none. Blocks while the login shell is read,
/// at most [`SHELL_WITHIN`], the first time only.
pub(crate) fn agent_path() -> OsString {
    static PATH: OnceLock<OsString> = OnceLock::new();
    PATH.get_or_init(|| {
        let shell = if cfg!(unix) {
            login_shell().and_then(|shell| shell_path(&shell, SHELL_WITHIN))
        } else {
            None
        };
        merged(
            shell.as_deref(),
            std::env::var_os("PATH").as_deref(),
            &usual_folders(std::env::home_dir().as_deref()),
        )
    })
    .clone()
}

/// Gives the agent's environment a PATH, unless its settings set one, and returns the program
/// to start: on Unix the one found on that PATH, else the program as named. `Err` holds the
/// program as named when it is on no folder of that PATH.
pub(crate) fn resolve(program: &str, env: &mut BTreeMap<String, String>) -> Result<String, String> {
    let path = match env.get("PATH") {
        Some(path) => OsString::from(path),
        None => {
            let path = agent_path();
            // A PATH that is not Unicode stays Studio's own: the agent's environment is text.
            if let Some(text) = path.to_str() {
                env.insert("PATH".to_owned(), text.to_owned());
            }
            path
        }
    };
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    #[cfg(windows)]
    {
        // The launcher finds the program again; a name with a folder is the standard library's.
        let extensions = std::env::var("PATHEXT").unwrap_or_default();
        let named_path = program.contains(['/', '\\', ':']);
        if named_path
            || crate::launch::find_program(OsStr::new(program), &dirs, &extensions, Path::is_file)
                .is_some()
        {
            Ok(program.to_owned())
        } else {
            Err(program.to_owned())
        }
    }
    #[cfg(not(windows))]
    {
        find(program, &dirs, is_executable)
            .map(|found| found.to_string_lossy().into_owned())
            .ok_or_else(|| program.to_owned())
    }
}

/// What the agent panel says when `program` cannot be found.
pub(crate) fn missing_message(program: &str) -> String {
    format!("Studio could not find `{program}`. Install it, or set its path in Settings → Agent.")
}

/// `program` as a shell finds it: a name with a folder as it is, when it can run; a bare name
/// in the first absolute folder of `dirs` that holds it, never in the working directory.
#[cfg(any(not(windows), test))]
fn find(program: &str, dirs: &[PathBuf], can_run: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let path = PathBuf::from(program);
        return can_run(&path).then_some(path);
    }
    dirs.iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| can_run(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The folders installers put agents and their runtimes in, after the shell's and Studio's own
/// PATH: Studio's own install folders for the CLI, mise's shims, Volta's and Bun's, and the
/// system's. nvm's folder names a Node version, so only the login shell finds it.
fn usual_folders(home: Option<&Path>) -> Vec<PathBuf> {
    if !cfg!(unix) {
        return Vec::new();
    }
    let mut folders = Vec::new();
    if let Some(home) = home {
        for dir in [
            ".local/bin",
            ".local/share/mise/shims",
            ".volta/bin",
            ".bun/bin",
        ] {
            folders.push(home.join(dir));
        }
    }
    for dir in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        folders.push(PathBuf::from(dir));
    }
    folders
}

/// The login shell's PATH, then `inherited`, then `usual`: each absolute folder once, in that
/// order. A relative or empty entry names a folder under the agent's working directory, where
/// the agent writes, and is dropped.
fn merged(shell: Option<&OsStr>, inherited: Option<&OsStr>, usual: &[PathBuf]) -> OsString {
    let mut folders: Vec<PathBuf> = Vec::new();
    let listed = [shell, inherited]
        .into_iter()
        .flatten()
        .flat_map(|path| std::env::split_paths(path).collect::<Vec<_>>());
    for folder in listed.chain(usual.iter().cloned()) {
        if folder.is_absolute()
            && !folders.contains(&folder)
            && std::env::join_paths([&folder]).is_ok()
        {
            folders.push(folder);
        }
    }
    std::env::join_paths(folders).unwrap_or_default()
}

/// The person's login shell: `SHELL` when it names one, else the platform's default.
fn login_shell() -> Option<PathBuf> {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|shell| shell.is_absolute())
        .or_else(|| {
            let default = if cfg!(target_os = "macos") {
                "/bin/zsh"
            } else {
                "/bin/sh"
            };
            Some(PathBuf::from(default))
        })
}

/// The PATH `shell` gives a login, interactive session, as a terminal would: startup files that
/// add Homebrew, mise or nvm run, and `printenv` prints the result between marks. `None` when
/// the shell fails, prints no marks, or has not finished within `within`, when it is ended.
/// Stdin is closed, so a startup file that asks a question reads its end.
fn shell_path(shell: &Path, within: Duration) -> Option<OsString> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};

    let script = format!("printf '%s' {MARK}; printenv PATH; printf '%s' {MARK}");
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, read) = std::sync::mpsc::channel();
    // Read on a thread so a shell that prints much cannot fill the pipe and stall; one that
    // leaves a process holding the pipe leaves only this thread waiting.
    std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).ok();
        sender.send(output).ok();
    });
    let deadline = std::time::Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
        }
    }
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    let output = read
        .recv_timeout(left.max(Duration::from_millis(100)))
        .ok()?;
    marked_path(&output)
}

/// The text between the first and the last [`MARK`] of `output`, trimmed; `None` when it is
/// empty or the marks are missing.
fn marked_path(output: &[u8]) -> Option<OsString> {
    let text = String::from_utf8_lossy(output);
    let start = text.find(MARK)? + MARK.len();
    let end = text.rfind(MARK)?;
    let path = text.get(start..end)?.trim();
    (!path.is_empty()).then(|| OsString::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(folders: &[&str]) -> OsString {
        std::env::join_paths(folders).unwrap()
    }

    /// The agent's PATH is the shell's, then Studio's, then the usual folders, each folder once;
    /// relative and empty entries are dropped. Started from the Dock, Studio's own PATH is
    /// launchd's, and the shell's brings Homebrew's and nvm's folders in front of it.
    #[cfg(unix)]
    #[test]
    fn the_agent_path_puts_the_shells_folders_first_and_lists_each_once() {
        let shell = joined(&[
            "/Users/a/.nvm/versions/node/v22/bin",
            "/opt/homebrew/bin",
            "bin",
            "/usr/bin",
        ]);
        let launchd = OsString::from("/usr/bin:/bin:/usr/sbin:/sbin:");
        let usual = usual_folders(Some(Path::new("/Users/a")));
        let path = merged(Some(&shell), Some(&launchd), &usual);
        let folders: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(
            folders[..5],
            [
                "/Users/a/.nvm/versions/node/v22/bin",
                "/opt/homebrew/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin"
            ]
            .map(PathBuf::from)
        );
        assert!(folders.contains(&PathBuf::from("/Users/a/.local/bin")));
        assert!(folders.contains(&PathBuf::from("/Users/a/.local/share/mise/shims")));
        assert!(folders.iter().all(|folder| folder.is_absolute()));
        let mut unique = folders.clone();
        unique.dedup();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), folders.len(), "{folders:?}");

        // No shell to read (it timed out): launchd's PATH still gains the install folders.
        let path = merged(None, Some(&launchd), &usual);
        let folders: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(folders[0], PathBuf::from("/usr/bin"));
        assert!(folders.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(folders.contains(&PathBuf::from("/usr/local/bin")));
    }

    /// Startup files may print before and after the PATH; only what the marks bracket counts.
    #[test]
    fn the_path_is_read_between_the_marks() {
        let output = format!("Welcome!\n{MARK}/opt/homebrew/bin:/usr/bin\n{MARK}bye\n");
        assert_eq!(
            marked_path(output.as_bytes()),
            Some(OsString::from("/opt/homebrew/bin:/usr/bin"))
        );
        assert_eq!(marked_path(b"no marks here"), None);
        assert_eq!(marked_path(format!("{MARK}\n{MARK}").as_bytes()), None);
    }

    #[cfg(unix)]
    fn script(name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    }

    /// A shell is run as a login, interactive one and its PATH read; one that never finishes is
    /// ended and given up on within the limit.
    #[cfg(unix)]
    #[test]
    fn the_login_shell_is_read_once_it_answers_and_given_up_on_when_it_hangs() {
        // A stand-in shell: says something, then runs the script it is given with the PATH a
        // login would set, if it was asked for a login, interactive session.
        let (_dir, shell) = script(
            "shell",
            "#!/bin/sh\necho 'startup noise'\n[ \"$1\" = -l ] && [ \"$2\" = -i ] && [ \"$3\" = -c ] || exit 3\nPATH=/opt/tools/bin:/usr/bin:/bin exec /bin/sh -c \"$4\"\n",
        );
        assert_eq!(
            shell_path(&shell, Duration::from_secs(30)),
            Some(OsString::from("/opt/tools/bin:/usr/bin:/bin"))
        );

        let (_dir, hanging) = script("shell", "#!/bin/sh\nexec sleep 60\n");
        let started = std::time::Instant::now();
        assert_eq!(shell_path(&hanging, Duration::from_millis(300)), None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );

        let (_dir, failing) = script("shell", "#!/bin/sh\nexit 1\n");
        assert_eq!(shell_path(&failing, Duration::from_secs(30)), None);
    }

    /// A bare name is found in the first absolute folder that holds it, never under the working
    /// directory; a path is taken as it is when it can run.
    #[cfg(unix)]
    #[test]
    fn a_program_is_found_on_the_path_or_named_as_missing() {
        let present = [
            PathBuf::from("/opt/homebrew/bin/npx"),
            PathBuf::from("/usr/local/bin/npx"),
            PathBuf::from("bin/codex"),
            PathBuf::from("/opt/agents/codex-acp"),
        ];
        let can_run = |path: &Path| present.iter().any(|file| file == path);
        let dirs = ["bin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from);
        assert_eq!(
            find("npx", &dirs, can_run),
            Some(PathBuf::from("/opt/homebrew/bin/npx"))
        );
        assert_eq!(find("codex", &dirs, can_run), None);
        assert_eq!(
            find("/opt/agents/codex-acp", &dirs, can_run),
            Some(PathBuf::from("/opt/agents/codex-acp"))
        );
        assert_eq!(find("/opt/agents/missing", &dirs, can_run), None);
        assert_eq!(find("", &dirs, can_run), None);
    }

    /// A PATH in the agent's settings is used as it is, for finding the program and for the
    /// agent; without one the agent is given the merged PATH. A program on neither is named in
    /// the message, which says what to do.
    #[cfg(unix)]
    #[test]
    fn the_settings_path_wins_and_a_missing_program_is_named() {
        let (dir, _agent) = script("my-agent", "#!/bin/sh\n");
        let folder = dir.path().to_string_lossy().into_owned();
        let mut env = BTreeMap::from([("PATH".to_owned(), folder.clone())]);
        assert_eq!(
            resolve("my-agent", &mut env),
            Ok(dir.path().join("my-agent").to_string_lossy().into_owned())
        );
        assert_eq!(env["PATH"], folder, "the settings' PATH is kept");

        let mut env = BTreeMap::from([("PATH".to_owned(), folder)]);
        assert_eq!(
            resolve("studio-test-no-such-agent", &mut env),
            Err("studio-test-no-such-agent".to_owned())
        );

        let mut env = BTreeMap::new();
        assert_eq!(
            resolve("sh", &mut env)
                .as_deref()
                .ok()
                .map(Path::new)
                .map(Path::is_absolute),
            Some(true)
        );
        assert!(
            env.contains_key("PATH"),
            "the agent is given the merged PATH"
        );

        assert_eq!(
            missing_message("codex"),
            "Studio could not find `codex`. Install it, or set its path in Settings → Agent."
        );
    }
}
