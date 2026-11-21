//! `postmaster.pid` handling: a dead pid is stale and its file is removed (logged); a
//! live pid is refused with a typed error; a corrupt or unparsable file is refused like
//! a real postmaster refuses a corrupt lock file.

use crate::error::SupervisorError;
use crate::layout::PgLayout;
use std::path::Path;

/// The classification the supervisor acts on before spawning postgres.
pub(crate) enum LockOutcome {
    /// No pid file (or an empty one that has been removed now): nothing held the port.
    Absent,
    /// The file named a dead process and has been removed; startup may proceed.
    CleanedStale(u32),
    /// The file names a live process: refuse, never kill something we did not start.
    Live(u32),
}

/// What [`read_pid_file`] found (before any cleanup).
pub(crate) enum LockState {
    /// No pid file present.
    Absent,
    /// The file exists but holds no pid (0 bytes): stale by postgres' own rules.
    Empty,
    /// A pid was read from the first line.
    Named(u32),
}

/// Reads `postmaster.pid` and classifies it. Only errors that mean "stop" bubble up.
pub(crate) async fn read_pid_file(path: &Path) -> Result<LockState, SupervisorError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(LockState::Absent),
        Err(source) => {
            return Err(SupervisorError::ReadFile {
                path: path.display().to_string(),
                source,
            });
        }
    };
    parse_first_line(path, &bytes)
}

/// Parses the pid from already-read bytes: first line, trimmed, non-empty, a `u32`.
pub(crate) fn parse_first_line(path: &Path, bytes: &[u8]) -> Result<LockState, SupervisorError> {
    let text = String::from_utf8_lossy(bytes);
    let first_line = text.lines().next().unwrap_or_default().trim().to_string();
    if first_line.is_empty() {
        // postgres itself treats an empty pid file as stale ("any contents are
        // invalid"): the next startup may delete it.
        return Ok(LockState::Empty);
    }
    match first_line.parse::<u32>() {
        Ok(pid) => Ok(LockState::Named(pid)),
        Err(_) => Err(SupervisorError::CorruptPidFile {
            path: path.display().to_string(),
            first_line,
        }),
    }
}

/// `true` when a process with this pid exists on this machine (`kill -0` semantics).
#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    crate::platform::process_exists(pid)
}

/// `true` when a process with this pid exists on this machine.
#[cfg(windows)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    // Without a process handle here we must not guess: on Windows, classifying a pid we
    // just read as dead risks deleting a lock of a running postmaster we did not start.
    // Conservative default: treat every pid as alive so the typed refusal fires.
    let _ = pid;
    true
}

/// Removes the pid file (stale-lock cleanup) and logs it.
pub(crate) async fn remove(path: &Path) -> Result<(), SupervisorError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        // Racing with a postmaster that exited concurrently is fine.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SupervisorError::RemoveStaleLock {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// The gate run before every start: dead pid -> clean and log; live pid -> refuse.
pub(crate) async fn clean_stale_or_fail(layout: &PgLayout) -> Result<LockOutcome, SupervisorError> {
    let path = layout.postmaster_pid_path();
    match read_pid_file(&path).await? {
        LockState::Absent => Ok(LockOutcome::Absent),
        LockState::Empty => {
            remove(&path).await?;
            tracing::info!(
                path = %path.display(),
                "removed empty postmaster.pid (no pid held)"
            );
            Ok(LockOutcome::CleanedStale(0))
        }
        LockState::Named(pid) => {
            if pid_alive(pid) {
                tracing::warn!(
                    pid,
                    path = %path.display(),
                    "postmaster.pid names a live process; refusing to start"
                );
                Ok(LockOutcome::Live(pid))
            } else {
                remove(&path).await?;
                tracing::info!(
                    pid,
                    path = %path.display(),
                    "removed stale postmaster.pid (pid {pid} is gone)"
                );
                Ok(LockOutcome::CleanedStale(pid))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Result-style unwrap that names the failure instead of a bare `unwrap`.
    fn expect<T, E: std::fmt::Display>(value: Result<T, E>, what: &str) -> T {
        match value {
            Ok(value) => value,
            Err(error) => panic!("{what}: {error}"),
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => panic!("test runtime could not be built: {error}"),
        };
        runtime.block_on(future)
    }

    #[test]
    fn empty_file_reads_as_empty() {
        let state = expect(
            parse_first_line(Path::new("/tmp/x"), b""),
            "empty pid file parses",
        );
        assert!(matches!(state, LockState::Empty));
    }

    #[test]
    fn garbage_is_corrupt() {
        let result = parse_first_line(Path::new("/tmp/x"), b"not-a-pid\n12345\n");
        assert!(matches!(
            result,
            Err(SupervisorError::CorruptPidFile { .. })
        ));
    }

    #[test]
    fn plain_pid_reads() {
        let state = expect(
            parse_first_line(Path::new("/tmp/x"), b"12345\n/data\n5433\n"),
            "pid file parses",
        );
        assert!(matches!(state, LockState::Named(12345)));
    }

    #[test]
    fn negative_pid_is_corrupt() {
        let result = parse_first_line(Path::new("/tmp/x"), b"-1\n");
        assert!(matches!(
            result,
            Err(SupervisorError::CorruptPidFile { .. })
        ));
    }

    #[test]
    fn missing_file_reads_absent() {
        let state = block_on(read_pid_file(Path::new(
            "/definitely/not/here/postmaster.pid",
        )));
        assert!(matches!(state, Ok(LockState::Absent)));
    }
}
