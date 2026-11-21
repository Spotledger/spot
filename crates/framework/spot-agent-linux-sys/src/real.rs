//! The real adapters: [`StdCommandRunner`] spawns argv vectors with `std::process`, and
//! [`StdFileSystem`] is `std::fs` with atomic writes.
//!
//! This is the only module in the crate that touches the operating system directly. It
//! clears the child environment and sets only the spec's allow-listed pairs, reads the two
//! output streams on their own threads (so a chatty tool cannot deadlock on a full pipe),
//! enforces the timeout by killing the child, and caps the kept output. It reads a
//! monotonic clock (`Instant`) for the timeout only — that is an adapter concern; the
//! modules themselves never read a clock.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{COMMAND_TIMEOUT, SysError, SysResult};
use crate::sys::{
    CommandOutput, CommandRunner, CommandSpec, FileLock, FileSystem, Sleeper, tool_missing,
};

/// The default `PATH` when the spec does not set one: the administrative directories, so a
/// bare tool name resolves on every supported distribution.
pub const DEFAULT_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Runs commands with `std::process::Command`, argv-only.
#[derive(Debug, Default)]
pub struct StdCommandRunner;

impl StdCommandRunner {
    /// A new runner.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for StdCommandRunner {
    fn run(&self, spec: &CommandSpec) -> SysResult<CommandOutput> {
        let mut command = Command::new(spec.program());
        command.args(spec.arguments());
        command.env_clear();
        for (key, value) in spec.env_pairs() {
            command.env(key, value);
        }
        if !spec.env_pairs().iter().any(|(key, _)| key == "PATH") {
            command.env("PATH", DEFAULT_PATH);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                tool_missing(spec.program())
            } else {
                SysError::io(format!("cannot start {}: {error}", spec.program()))
            }
        })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_reader = std::thread::spawn(move || read_all(stdout));
        let err_reader = std::thread::spawn(move || read_all(stderr));
        let deadline = Instant::now() + Duration::from_secs(spec.timeout());
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(SysError::io(error.to_string())),
            }
        };
        let out_bytes = out_reader
            .join()
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let err_bytes = err_reader
            .join()
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let Some(status) = status else {
            return Err(SysError::new(
                COMMAND_TIMEOUT,
                format!(
                    "{} did not finish within {}s",
                    spec.argv_key(),
                    spec.timeout()
                ),
            ));
        };
        let (stdout, out_cut) = cap(out_bytes, spec.cap());
        let (stderr, err_cut) = cap(err_bytes, spec.cap());
        Ok(
            CommandOutput::new(status.code().unwrap_or(-1), stdout, stderr)
                .truncated(out_cut || err_cut),
        )
    }
}

/// Reads a pipe to the end, or returns empty bytes when there is no pipe.
fn read_all(mut pipe: Option<impl Read>) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(reader) = pipe.as_mut() {
        reader.read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

/// Renders bytes as UTF-8 and truncates to `cap` bytes at a character boundary, reporting
/// whether anything was cut.
fn cap(bytes: Vec<u8>, cap: usize) -> (String, bool) {
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

/// The real filesystem: `std::fs`, with writes through a temporary file and a rename.
#[derive(Debug, Default)]
pub struct StdFileSystem;

impl StdFileSystem {
    /// A new filesystem adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for StdFileSystem {
    fn read_to_string(&self, path: &Path) -> SysResult<String> {
        std::fs::read_to_string(path)
            .map_err(|error| SysError::io(format!("cannot read {}: {error}", path.display())))
    }

    fn exists(&self, path: &Path) -> SysResult<bool> {
        path.try_exists()
            .map_err(|error| SysError::io(format!("cannot inspect {}: {error}", path.display())))
    }

    fn is_dir(&self, path: &Path) -> SysResult<bool> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(SysError::io(format!(
                "cannot inspect {}: {error}",
                path.display()
            ))),
        }
    }

    fn list_dir(&self, path: &Path) -> SysResult<Vec<String>> {
        let mut names = Vec::new();
        let entries = std::fs::read_dir(path)
            .map_err(|error| SysError::io(format!("cannot list {}: {error}", path.display())))?;
        for entry in entries {
            let entry = entry.map_err(|error| SysError::io(format!("cannot list: {error}")))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    fn write_atomic(&self, path: &Path, contents: &str) -> SysResult<()> {
        use std::io::Write as _;

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(|error| {
                SysError::io(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        let temporary = path.with_extension("spot-tmp");
        // Write, flush to disk, then rename: after a crash the path holds the old contents
        // or the new, never a torn file.
        let mut file = std::fs::File::create(&temporary).map_err(|error| {
            SysError::io(format!("cannot write {}: {error}", temporary.display()))
        })?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                SysError::io(format!("cannot write {}: {error}", temporary.display()))
            })?;
        drop(file);
        std::fs::rename(&temporary, path)
            .map_err(|error| SysError::io(format!("cannot place {}: {error}", path.display())))?;
        // Make the rename itself durable (best effort: a directory cannot always be synced).
        if let Some(parent) = parent
            && let Ok(directory) = std::fs::File::open(parent)
        {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> SysResult<()> {
        std::fs::remove_file(path)
            .map_err(|error| SysError::io(format!("cannot remove {}: {error}", path.display())))
    }

    fn create_dir_all(&self, path: &Path) -> SysResult<()> {
        std::fs::create_dir_all(path)
            .map_err(|error| SysError::io(format!("cannot create {}: {error}", path.display())))
    }

    fn rename(&self, from: &Path, to: &Path) -> SysResult<()> {
        std::fs::rename(from, to).map_err(|error| {
            SysError::io(format!(
                "cannot rename {} to {}: {error}",
                from.display(),
                to.display()
            ))
        })
    }

    fn metadata_len(&self, path: &Path) -> SysResult<u64> {
        std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .map_err(|error| SysError::io(format!("cannot stat {}: {error}", path.display())))
    }

    fn read_link(&self, path: &Path) -> SysResult<std::path::PathBuf> {
        std::fs::read_link(path)
            .map_err(|error| SysError::io(format!("cannot read link {}: {error}", path.display())))
    }

    fn try_lock_exclusive(&self, path: &Path) -> SysResult<Option<FileLock>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|error| {
                SysError::io(format!("cannot open lock {}: {error}", path.display()))
            })?;
        // `flock(2)` through the standard library: a lock per open file, released when the
        // file is closed (the guard is dropped, or the process dies).
        match file.try_lock() {
            Ok(()) => Ok(Some(Box::new(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(SysError::io(format!(
                "cannot lock {}: {error}",
                path.display()
            ))),
        }
    }
}

/// The real [`Sleeper`]: the thread sleeps.
#[derive(Debug, Default)]
pub struct StdSleeper;

impl Sleeper for StdSleeper {
    fn sleep(&self, duration: std::time::Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_real_command_runs_without_a_shell() {
        let runner = StdCommandRunner::new();
        let spec = CommandSpec::new("echo").unwrap().arg("hello").unwrap();
        let output = runner.run(&spec).unwrap();
        assert_eq!(output.status(), 0);
        assert_eq!(output.stdout_trimmed(), "hello");
    }

    #[test]
    fn a_missing_tool_is_reported() {
        let runner = StdCommandRunner::new();
        let spec = CommandSpec::new("spot-no-such-tool").unwrap();
        assert_eq!(
            runner.run(&spec).unwrap_err().code,
            crate::error::TOOL_MISSING
        );
    }

    #[test]
    fn the_real_lock_excludes_a_second_taker_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let fs = StdFileSystem::new();
        let held = fs.try_lock_exclusive(&path).unwrap();
        assert!(held.is_some());
        assert!(fs.try_lock_exclusive(&path).unwrap().is_none());
        drop(held);
        // `flock` belongs to the open file description. A sibling test that spawns a process
        // forks while this descriptor is open, and the child keeps the description (and so the
        // lock) until its `exec` closes it, so the release can lag the drop by a moment on a
        // slow runner. The lock must come back; wait for it rather than assume the instant.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut retaken = false;
        while std::time::Instant::now() < deadline {
            if fs.try_lock_exclusive(&path).unwrap().is_some() {
                retaken = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            retaken,
            "the lock was not released after the holder was dropped"
        );
    }

    #[test]
    fn the_output_cap_truncates() {
        let (text, cut) = cap(vec![b'a'; 100], 10);
        assert_eq!(text.len(), 10);
        assert!(cut);
        let (text, cut) = cap(vec![b'a'; 5], 10);
        assert_eq!(text.len(), 5);
        assert!(!cut);
    }
}
