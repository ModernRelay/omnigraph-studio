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
//! the launcher keeps the agent in a job that ends with it.

use std::ffi::{OsStr, OsString};
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
    let Some(mut command) = command(args) else {
        eprintln!(
            "usage: omnigraph-studio {} NAME… -- PROGRAM [ARGS…]",
            crate::LAUNCH_COMMAND
        );
        return 2;
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

/// The agent's command: the program after the first `--`, its arguments, and every name before
/// that `--` removed from its environment. `None` when no program follows.
fn command(args: impl IntoIterator<Item = OsString>) -> Option<Command> {
    let mut args = args.into_iter();
    let removed: Vec<OsString> = args
        .by_ref()
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .collect();
    let mut command = Command::new(args.next()?);
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
    Some(command)
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
