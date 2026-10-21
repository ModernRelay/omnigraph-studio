//! The agent's launcher (spec 013). The agent never inherits the person's graph credentials, and
//! the ACP crate can add variables to the agent's process but not remove them. On Unix the agent
//! starts through `env -u`; Windows has no `env`, so there Studio starts its own executable as
//! `omnigraph-studio launch-agent NAME… -- PROGRAM ARGS…`, which runs PROGRAM with ARGS and none
//! of the NAMEs in its environment, and exits as PROGRAM does.
//!
//! The arguments reach the launcher on the command line the standard library writes and reads
//! back, and the launcher starts PROGRAM as the ACP crate would, so a program path or argument
//! passes as it does when nothing is removed. `cmd /c set NAME=` could not promise that: `cmd`
//! expands `%` even inside quotes and does not read the quotes the standard library writes.
//!
//! Studio stops an agent by ending the process it started, which on Windows is the launcher, so
//! the launcher keeps the agent in a job that ends with it. For that reason Windows starts every
//! agent through it, with no names to remove as well.
//!
//! A program named without a folder is found as a shell finds it, on PATH, and never in the
//! working directory ([`find_program`]): the standard library looks only for `NAME.exe`, and the
//! agents Studio offers start with `npx`, which npm installs on Windows as `npx.cmd`.

use std::ffi::{OsStr, OsString};
#[cfg(any(windows, test))]
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The launcher's arguments, its own name first, for running `program` with `args` and none of
/// `removed` in its environment.
pub(crate) fn arguments(removed: &[String], program: &str, args: &[String]) -> Vec<String> {
    let mut arguments = vec![crate::LAUNCH_COMMAND.to_owned()];
    arguments.extend(removed.iter().cloned());
    arguments.push("--".to_owned());
    arguments.push(program.to_owned());
    arguments.extend(args.iter().cloned());
    arguments
}

/// Runs the launcher with the arguments after its name, `NAME… -- PROGRAM ARGS…`, and returns
/// the exit code: PROGRAM's, 2 when no program is named, or 1 when it cannot start.
pub fn launch_agent(args: impl IntoIterator<Item = OsString>) -> i32 {
    let mut command = match command(args) {
        Ok(command) => command,
        Err(NoCommand::Usage) => {
            eprintln!(
                "usage: omnigraph-studio {} NAME… -- PROGRAM [ARGS…]",
                crate::LAUNCH_COMMAND
            );
            return 2;
        }
        #[cfg(windows)]
        Err(NoCommand::NotFound(program)) => {
            eprintln!(
                "omnigraph-studio {}: could not find {} on PATH",
                crate::LAUNCH_COMMAND,
                program.to_string_lossy()
            );
            return 1;
        }
    };
    #[cfg(windows)]
    if let Err(error) = job::end_with_this_process() {
        eprintln!(
            "omnigraph-studio {}: the agent may outlive this launcher: {error}",
            crate::LAUNCH_COMMAND
        );
    }
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!(
                "omnigraph-studio {}: could not start {}: {error}",
                crate::LAUNCH_COMMAND,
                command.get_program().to_string_lossy()
            );
            1
        }
    }
}

/// Why the launcher has no command to run.
enum NoCommand {
    /// No program follows the `--`.
    Usage,
    /// A program named without a folder is on no absolute PATH entry.
    #[cfg(windows)]
    NotFound(OsString),
}

/// The agent's command: the program after the first `--`, its arguments, and every name before
/// that `--` removed from its environment.
fn command(args: impl IntoIterator<Item = OsString>) -> Result<Command, NoCommand> {
    let mut args = args.into_iter();
    let removed: Vec<OsString> = args
        .by_ref()
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .collect();
    let program = args.next().ok_or(NoCommand::Usage)?;
    #[cfg(windows)]
    let program = {
        let dirs = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default();
        let extensions = std::env::var("PATHEXT").unwrap_or_default();
        find_program(&program, &dirs, &extensions, Path::is_file)
            .ok_or(NoCommand::NotFound(program))?
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for name in removed {
        command.env_remove(name);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // As the ACP crate starts an agent: a release build of Studio has no console, so a
        // console program would open a window of its own.
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    Ok(command)
}

/// The extensions of a program the standard library can start on Windows: a `.bat` or `.cmd`
/// runs through `cmd`, with its arguments escaped for it.
#[cfg(any(windows, test))]
const STARTABLE: &[&str] = &[".com", ".exe", ".bat", ".cmd"];

/// The program `name` stands for, found as a shell finds a command but never in the working
/// directory. A name with no folder is looked for in each absolute folder of `dirs` (PATH, in
/// its order): as it is when it has an extension, else with each extension of `pathext`
/// (PATHEXT, `.COM;.EXE;.BAT;.CMD` when unset) the standard library can start. An empty or
/// relative entry (`a;;b`, a trailing `;`, `bin`) names a folder under the working directory and
/// is passed over: the agent writes there, and the launcher runs outside the agent's sandbox.
/// The standard library's own search reads such entries too, so a name in no absolute folder is
/// `None`, not left to it. A name with a folder is the standard library's, as given.
#[cfg(any(windows, test))]
fn find_program(
    name: &OsStr,
    dirs: &[PathBuf],
    pathext: &str,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let as_given = PathBuf::from(name);
    let bytes = name.as_encoded_bytes();
    if bytes.is_empty() || bytes.iter().any(|byte| matches!(byte, b'/' | b'\\' | b':')) {
        return Some(as_given);
    }
    let extensions: Vec<&str> = if as_given.extension().is_some() {
        vec![""]
    } else if pathext.trim().is_empty() {
        STARTABLE.to_vec()
    } else {
        pathext
            .split(';')
            .filter(|extension| STARTABLE.contains(&extension.to_ascii_lowercase().as_str()))
            .collect()
    };
    dirs.iter().filter(|dir| dir.is_absolute()).find_map(|dir| {
        extensions
            .iter()
            .map(|extension| {
                let mut file = name.to_owned();
                file.push(extension);
                dir.join(file)
            })
            .find(|candidate| is_file(candidate))
    })
}

#[cfg(windows)]
mod job {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// Puts this process in a new job that ends every process in it when its last handle
    /// closes. The handle is never closed, so the job ends with this process however it ends,
    /// and with it the agent and everything the agent started, which join the job as they
    /// start.
    pub(super) fn end_with_this_process() -> std::io::Result<()> {
        // SAFETY: the job attributes and name may be null; `limits` is a plain C struct, zeroed
        // as the API expects, and outlives the call that reads it, which is told its size.
        // `GetCurrentProcess` returns a pseudo-handle that needs no closing.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
                || AssignProcessToJobObject(job, GetCurrentProcess()) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty or relative entry of PATH (`a;;b`, a trailing `;`, `bin`) names a folder under
    /// the working directory, which is never searched: the agent writes there. A name found on
    /// no absolute entry is found nowhere, though the working directory holds it.
    #[test]
    fn a_relative_path_entry_is_never_searched() {
        let npm = std::env::temp_dir().join("npm");
        let looked = std::cell::RefCell::new(Vec::new());
        let present = [
            PathBuf::from("npx.cmd"),
            PathBuf::from("bin").join("npx.cmd"),
            PathBuf::from("agent.exe"),
            npm.join("npx.cmd"),
        ];
        let is_file = |path: &Path| {
            looked.borrow_mut().push(path.to_owned());
            present.iter().any(|file| file == path)
        };
        let dirs = [PathBuf::new(), PathBuf::from("bin"), npm.clone()];
        let find = |name: &str| find_program(OsStr::new(name), &dirs, ".cmd;.exe", is_file);
        assert_eq!(find("npx"), Some(npm.join("npx.cmd")));
        assert_eq!(find("npx.cmd"), Some(npm.join("npx.cmd")));
        assert_eq!(find("agent"), None);
        assert_eq!(find("agent.exe"), None);
        assert!(
            looked.borrow().iter().all(|path| path.is_absolute()),
            "{:?}",
            looked.borrow()
        );
    }

    /// `npx` is `npx.cmd` on Windows: the launcher finds it by PATHEXT in PATH's order, as a
    /// shell would, and a name with an extension as it is. A path is left as given.
    #[test]
    fn a_bare_program_name_is_found_by_its_extensions_in_path_order() {
        let root = std::env::temp_dir();
        let (npm, tools) = (root.join("npm"), root.join("tools"));
        let present = [
            npm.join("npx.cmd"),
            tools.join("npx.cmd"),
            tools.join("agent.js"),
            tools.join("uvx.exe"),
        ];
        // Windows's file names match in any case.
        let is_file = |path: &Path| {
            let path = path.to_string_lossy().to_lowercase();
            present
                .iter()
                .any(|file| file.to_string_lossy().to_lowercase() == path)
        };
        let dirs = [npm.clone(), tools.clone()];
        let find =
            |name: &str, pathext: &str| find_program(OsStr::new(name), &dirs, pathext, is_file);
        let pathext = ".COM;.EXE;.BAT;.CMD;.VBS;.JS";
        assert_eq!(find("npx", pathext), Some(npm.join("npx.CMD")));
        assert_eq!(find("uvx", pathext), Some(tools.join("uvx.EXE")));
        // PATHEXT unset: the four extensions Windows starts.
        assert_eq!(find("uvx", ""), Some(tools.join("uvx.exe")));
        // A script the standard library cannot start is not chosen by its bare name.
        assert_eq!(find("agent", pathext), None);
        assert_eq!(find("agent.js", pathext), Some(tools.join("agent.js")));
        assert_eq!(find("npx.cmd", pathext), Some(npm.join("npx.cmd")));
        assert_eq!(find("missing", pathext), None);
        for given in ["C:\\npm\\npx", "bin/npx", ".\\npx", ""] {
            assert_eq!(
                find(given, pathext),
                Some(PathBuf::from(given)),
                "{given:?}"
            );
        }
    }
}
