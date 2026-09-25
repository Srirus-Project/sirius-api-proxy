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

#[cfg(unix)]
struct Group {
    id: u32,
    armed: bool,
}
#[cfg(unix)]
impl Drop for Group {
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

/// Output limits apply separately to stdout/stderr. Stderr is discarded even on success.
/// Dropping the future cancels the process group on supported platforms.
/// Arguments are passed directly, never through a shell; no raw argv/output is logged.
pub async fn run(
    executable: &Path,
    directory: &Path,
    args: &[OsString],
    timeout: Duration,
    output_limit: usize,
) -> Result<Vec<u8>, Error> {
    if timeout.is_zero()
        || timeout > Duration::from_secs(600)
        || !(1024..=16 * 1024 * 1024).contains(&output_limit)
    {
        return Err(Error::Config);
    }
    #[cfg(not(unix))]
    {
        let _ = (executable, directory, args);
        Err(Error::Unsupported)
    }
    #[cfg(unix)]
    {
        use std::process::Stdio;
        use tokio::io::AsyncReadExt;
        let mut command = tokio::process::Command::new(executable);
        command
            .current_dir(directory)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn().map_err(|_| Error::Spawn)?;
        let mut group = Group {
            id: child.id().ok_or(Error::Spawn)?,
            armed: true,
        };
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
            let (stdout, _, status) = tokio::try_join!(
                read(stdout, output_limit),
                read(stderr, output_limit),
                async { child.wait().await.map_err(|_| Error::Io) },
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
