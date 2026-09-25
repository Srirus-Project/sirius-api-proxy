//! Bounded Git subprocess execution. Never attach raw command output to errors.
use std::{ffi::OsString, path::Path, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Git execution policy")]
    Config,
    #[error("Git publication process containment is unavailable on this platform")]
    Unsupported,
    #[error("Git process could not be started")]
    Spawn,
    #[error("Git process I/O failed")]
    Io,
    #[error("Git process output exceeded the configured limit")]
    OutputLimit,
    #[error("Git command timed out")]
    Timeout,
    #[error("Git command failed")]
    Failed,
}

/// Owns the Git process and every helper it starts. Dropping an armed tree kills them all.
#[cfg(unix)]
struct Tree {
    id: u32,
    armed: bool,
}
#[cfg(unix)]
impl Tree {
    fn contain(child: &tokio::process::Child) -> Result<Self, Error> {
        Ok(Self {
            id: child.id().ok_or(Error::Spawn)?,
            armed: true,
        })
    }
}
#[cfg(unix)]
impl Drop for Tree {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: the child was placed in its own process group before exec.
            // Negative PID targets only that group's Git/helper descendants.
            unsafe {
                libc::kill(-(self.id as i32), libc::SIGKILL);
            }
        }
    }
}

/// A Job Object replaces the Unix process group. The child starts suspended and runs
/// only after assignment, so no helper can escape before containment. Kill-on-close
/// also covers this process exiting abnormally while Git is running.
#[cfg(windows)]
struct Tree {
    job: windows_sys::Win32::Foundation::HANDLE,
    armed: bool,
}
// SAFETY: the handle is owned exclusively and only used through thread-safe kernel calls.
#[cfg(windows)]
unsafe impl Send for Tree {}
#[cfg(windows)]
impl Tree {
    fn contain(child: &tokio::process::Child) -> Result<Self, Error> {
        use windows_sys::Win32::System::JobObjects::*;
        let process = child.raw_handle().ok_or(Error::Spawn)?;
        let pid = child.id().ok_or(Error::Spawn)?;
        // SAFETY: plain kernel object calls on handles this function owns or borrows
        // from the live child; failure paths close the job, which kills the child.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(Error::Spawn);
            }
            let tree = Self { job, armed: true };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if tree.limit(&limits) == 0 || AssignProcessToJobObject(job, process as _) == 0 {
                return Err(Error::Spawn);
            }
            resume(pid)?;
            Ok(tree)
        }
    }
    unsafe fn limit(
        &self,
        limits: &windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    ) -> i32 {
        use windows_sys::Win32::System::JobObjects::*;
        SetInformationJobObject(
            self.job,
            JobObjectExtendedLimitInformation,
            limits as *const _ as *const _,
            std::mem::size_of_val(limits) as u32,
        )
    }
}
/// Resume the single initial thread of a process created with CREATE_SUSPENDED.
#[cfg(windows)]
fn resume(pid: u32) -> Result<(), Error> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{Diagnostics::ToolHelp::*, Threading::*},
    };
    // SAFETY: the snapshot and thread handles are closed before returning.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(Error::Spawn);
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    if ResumeThread(thread) != u32::MAX {
                        resumed += 1;
                    }
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if resumed == 0 {
            Err(Error::Spawn)
        } else {
            Ok(())
        }
    }
}
#[cfg(windows)]
impl Drop for Tree {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
        // SAFETY: the job handle is owned and closed exactly once here.
        unsafe {
            if self.armed {
                TerminateJobObject(self.job, 1);
            } else {
                // Match Unix success semantics: a helper that outlives a successful
                // command (for example a signing agent) is not killed by closing the job.
                let limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                self.limit(&limits);
            }
            CloseHandle(self.job);
        }
    }
}

/// Output limits apply separately to stdout/stderr. Stderr is discarded even on success.
/// Dropping the future cancels the process tree (Unix process group, Windows Job Object).
/// Arguments are passed directly, never through a shell; no raw argv/output is logged.
pub async fn run(
    executable: &Path,
    directory: &Path,
    args: &[OsString],
    timeout: Duration,
    output_limit: usize,
) -> Result<Vec<u8>, Error> {
    run_with_input(executable, directory, args, timeout, output_limit, &[]).await
}

pub async fn run_with_input(
    executable: &Path,
    directory: &Path,
    args: &[OsString],
    timeout: Duration,
    output_limit: usize,
    input: &[u8],
) -> Result<Vec<u8>, Error> {
    if input.len() > 4 * 1024 * 1024
        || timeout.is_zero()
        || timeout > Duration::from_secs(600)
        || !(1024..=16 * 1024 * 1024).contains(&output_limit)
    {
        return Err(Error::Config);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (executable, directory, args, input);
        Err(Error::Unsupported)
    }
    #[cfg(any(unix, windows))]
    {
        use std::process::Stdio;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut command = tokio::process::Command::new(executable);
        // Do not inherit repository redirects, injected Git config or trace destinations.
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        // Explicit Git policy controls routing; ambient bypass lists must not bypass it.
        for name in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
        ] {
            command.env_remove(name);
        }
        command
            .current_dir(directory)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ASKPASS", "/usr/bin/false")
            .process_group(0);
        // Without an askpass program and with terminal prompts disabled, Git fails
        // instead of asking; SSH_ASKPASS is Git's remaining fallback.
        #[cfg(windows)]
        command
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env_remove("SSH_ASKPASS")
            .creation_flags(
                windows_sys::Win32::System::Threading::CREATE_SUSPENDED
                    | windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
            );
        let mut child = command.spawn().map_err(|_| Error::Spawn)?;
        let mut group = match Tree::contain(&child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                return Err(error);
            }
        };
        let mut stdin = child.stdin.take().ok_or(Error::Io)?;
        let stdout = child.stdout.take().ok_or(Error::Io)?;
        let stderr = child.stderr.take().ok_or(Error::Io)?;
        async fn read(
            stream: impl tokio::io::AsyncRead + Unpin,
            limit: usize,
        ) -> Result<Vec<u8>, Error> {
            let mut bytes = Vec::new();
            stream
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| Error::Io)?;
            if bytes.len() > limit {
                Err(Error::OutputLimit)
            } else {
                Ok(bytes)
            }
        }
        let operation = async {
            let (stdout, _, status, _) = tokio::try_join!(
                read(stdout, output_limit),
                read(stderr, output_limit),
                async { child.wait().await.map_err(|_| Error::Io) },
                async {
                    stdin.write_all(input).await.map_err(|_| Error::Io)?;
                    drop(stdin);
                    Ok(())
                },
            )?;
            if !status.success() {
                return Err(Error::Failed);
            }
            Ok(stdout)
        };
        let result = tokio::time::timeout(timeout, operation)
            .await
            .map_err(|_| Error::Timeout)
            .and_then(|v| v);
        if result.is_ok() {
            group.armed = false;
        }
        // An error kills helper descendants too, including a helper still holding a pipe.
        drop(group);
        if result.is_err() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        result
    }
}
