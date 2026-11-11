//! The recording fakes: [`FakeRunner`] answers canned command transcripts, [`FakeFs`] is
//! an in-memory filesystem, and [`parse_transcript`] reads the `tests/transcripts` files.
//!
//! They are compiled for the crate's own tests and for any caller that enables the
//! `testing` feature, so a host can rehearse a change without touching a machine. Every
//! fake records the commands it was asked to run, so a test can assert on the exact argv
//! vector a module built.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::{SysError, SysResult, TOOL_MISSING};
use crate::sys::{CommandOutput, CommandRunner, CommandSpec, FileLock, FileSystem, Sleeper};

/// What the fake runner answers with: an output or an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeOutcome {
    /// A successful or non-zero command output.
    Output(CommandOutput),
    /// A failure the runner itself raises (a missing tool, for example).
    Error(SysError),
}

/// A command runner that answers canned transcripts and records every call.
#[derive(Debug, Default)]
pub struct FakeRunner {
    responses: Mutex<BTreeMap<String, FakeOutcome>>,
    calls: Mutex<Vec<CommandSpec>>,
}

impl FakeRunner {
    /// An empty runner; an unrecorded command fails loudly.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a canned successful output for the command whose argv key is `argv`.
    #[must_use]
    pub fn with_response(self, argv: &str, output: CommandOutput) -> Self {
        self.set_response(argv, output);
        self
    }

    /// Adds a canned outcome for the command whose argv key is `argv`.
    pub fn set_outcome(&self, argv: &str, outcome: FakeOutcome) {
        self.responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(argv.to_owned(), outcome);
    }

    /// Adds a canned output for the command whose argv key is `argv`.
    pub fn set_response(&self, argv: &str, output: CommandOutput) {
        self.set_outcome(argv, FakeOutcome::Output(output));
    }

    /// Adds a canned non-zero output (status and standard error) for `argv`.
    pub fn set_failure(&self, argv: &str, status: i32, stderr: &str) {
        self.set_response(argv, CommandOutput::new(status, "", stderr));
    }

    /// Adds a canned missing-tool failure for `argv`.
    pub fn set_tool_missing(&self, argv: &str, program: &str) {
        self.set_outcome(argv, FakeOutcome::Error(crate::sys::tool_missing(program)));
    }

    /// Removes a canned outcome, so the command fails as unrecorded again.
    pub fn forget(&self, argv: &str) {
        self.responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(argv);
    }

    /// Every command the runner was asked to run, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<CommandSpec> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The argv keys of every command the runner was asked to run, in order.
    #[must_use]
    pub fn call_keys(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(CommandSpec::argv_key)
            .collect::<Vec<_>>()
    }

    /// The most recent command, when there was one.
    #[must_use]
    pub fn last_call(&self) -> Option<CommandSpec> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last()
            .cloned()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, spec: &CommandSpec) -> SysResult<CommandOutput> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(spec.clone());
        let key = spec.argv_key();
        let outcome = self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned();
        match outcome {
            Some(FakeOutcome::Output(output)) => Ok(output),
            Some(FakeOutcome::Error(error)) => Err(error),
            None => Err(SysError::parse(format!(
                "the fake runner has no transcript for: {key}"
            ))),
        }
    }
}

/// An in-memory filesystem for tests.
#[derive(Debug, Default)]
pub struct FakeFs {
    files: Mutex<BTreeMap<PathBuf, String>>,
    dirs: Mutex<BTreeSet<PathBuf>>,
    links: Mutex<BTreeMap<PathBuf, PathBuf>>,
    locks: Arc<Mutex<BTreeSet<PathBuf>>>,
}

impl FakeFs {
    /// An empty filesystem.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a file with `contents`.
    #[must_use]
    pub fn with_file(self, path: &str, contents: &str) -> Self {
        self.set_file(path, contents);
        self
    }

    /// Adds a directory.
    #[must_use]
    pub fn with_dir(self, path: &str) -> Self {
        self.dirs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(PathBuf::from(path));
        self
    }

    /// Adds a symbolic link.
    #[must_use]
    pub fn with_link(self, path: &str, target: &str) -> Self {
        self.links
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(PathBuf::from(path), PathBuf::from(target));
        self
    }

    /// Sets a file's contents.
    pub fn set_file(&self, path: &str, contents: &str) {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(PathBuf::from(path), contents.to_owned());
    }

    /// The contents of a file, when present.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<String> {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(Path::new(path))
            .cloned()
    }

    /// Whether a file is present.
    #[must_use]
    pub fn has_file(&self, path: &str) -> bool {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(Path::new(path))
    }
}

impl FileSystem for FakeFs {
    fn read_to_string(&self, path: &Path) -> SysResult<String> {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(path)
            .cloned()
            .ok_or_else(|| SysError::io(format!("no such file: {}", path.display())))
    }

    fn exists(&self, path: &Path) -> SysResult<bool> {
        Ok(self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(path)
            || self
                .dirs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(path)
            || self
                .links
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(path))
    }

    fn is_dir(&self, path: &Path) -> SysResult<bool> {
        Ok(self
            .dirs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(path))
    }

    fn list_dir(&self, path: &Path) -> SysResult<Vec<String>> {
        let mut names = BTreeSet::new();
        for file in self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
        {
            if file.parent() == Some(path)
                && let Some(name) = file.file_name()
            {
                names.insert(name.to_string_lossy().into_owned());
            }
        }
        Ok(names.into_iter().collect())
    }

    fn write_atomic(&self, path: &Path, contents: &str) -> SysResult<()> {
        if let Some(parent) = path.parent() {
            self.dirs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(parent.to_path_buf());
        }
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.to_path_buf(), contents.to_owned());
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> SysResult<()> {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| SysError::io(format!("no such file: {}", path.display())))
    }

    fn create_dir_all(&self, path: &Path) -> SysResult<()> {
        let mut dirs = self
            .dirs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut current = Some(path);
        while let Some(dir) = current {
            dirs.insert(dir.to_path_buf());
            current = dir.parent();
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> SysResult<()> {
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let contents = files
            .remove(from)
            .ok_or_else(|| SysError::io(format!("no such file: {}", from.display())))?;
        files.insert(to.to_path_buf(), contents);
        Ok(())
    }

    fn metadata_len(&self, path: &Path) -> SysResult<u64> {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(path)
            .map(|contents| contents.len() as u64)
            .ok_or_else(|| SysError::io(format!("no such file: {}", path.display())))
    }

    fn read_link(&self, path: &Path) -> SysResult<PathBuf> {
        self.links
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(path)
            .cloned()
            .ok_or_else(|| SysError::io(format!("no such link: {}", path.display())))
    }

    fn try_lock_exclusive(&self, path: &Path) -> SysResult<Option<FileLock>> {
        let mut held = self
            .locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !held.insert(path.to_path_buf()) {
            return Ok(None);
        }
        Ok(Some(Box::new(FakeLockGuard {
            locks: Arc::clone(&self.locks),
            path: path.to_path_buf(),
        })))
    }
}

/// A held [`FakeFs`] lock; dropping it releases it.
struct FakeLockGuard {
    locks: Arc<Mutex<BTreeSet<PathBuf>>>,
    path: PathBuf,
}

impl Drop for FakeLockGuard {
    fn drop(&mut self) {
        self.locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.path);
    }
}

/// A [`Sleeper`] that only yields the thread and counts the pauses, so a test of the lock
/// wait takes no real time.
#[derive(Debug, Default)]
pub struct FakeSleeper {
    sleeps: std::sync::atomic::AtomicU64,
}

impl FakeSleeper {
    /// A sleeper that has not slept.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many pauses were asked for.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.sleeps.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Sleeper for FakeSleeper {
    fn sleep(&self, _duration: std::time::Duration) {
        self.sleeps
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::thread::yield_now();
    }
}

/// A successful [`CommandOutput`].
#[must_use]
pub fn ok_output(stdout: &str) -> CommandOutput {
    CommandOutput::new(0, stdout, "")
}

/// Parses a transcript into `(argv key, outcome)` pairs.
///
/// The format is one command per stanza:
///
/// ```text
/// # a comment
/// $ getenforce
/// Enforcing
/// $ rpm -q chrony
/// !exit 1
/// @package chrony is not installed
/// $ no-such-tool
/// !missing
/// ```
///
/// `$` starts a command (the rest of the line is its argv key); every other line before the
/// next `$` is standard output, except `!exit <code>` (the exit status, default 0),
/// `!missing` (the program is not installed) and `@<text>` (one line of standard error).
#[must_use]
pub fn parse_transcript(text: &str) -> Vec<(String, FakeOutcome)> {
    let mut entries: Vec<(String, FakeOutcome)> = Vec::new();
    let mut key: Option<String> = None;
    let mut stdout: Vec<String> = Vec::new();
    let mut stderr: Vec<String> = Vec::new();
    let mut status: i32 = 0;
    let mut missing = false;

    let flush = |key: &mut Option<String>,
                 stdout: &mut Vec<String>,
                 stderr: &mut Vec<String>,
                 status: &mut i32,
                 missing: &mut bool,
                 entries: &mut Vec<(String, FakeOutcome)>| {
        if let Some(name) = key.take() {
            let outcome = if *missing {
                FakeOutcome::Error(SysError::new(
                    TOOL_MISSING,
                    format!("the required tool {name} is not installed"),
                ))
            } else {
                FakeOutcome::Output(CommandOutput::new(
                    *status,
                    stdout.join("\n"),
                    stderr.join("\n"),
                ))
            };
            entries.push((name, outcome));
        }
        stdout.clear();
        stderr.clear();
        *status = 0;
        *missing = false;
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("$ ") {
            flush(
                &mut key,
                &mut stdout,
                &mut stderr,
                &mut status,
                &mut missing,
                &mut entries,
            );
            key = Some(rest.trim().to_owned());
        } else if let Some(code) = line.strip_prefix("!exit ") {
            status = code.trim().parse().unwrap_or(1);
        } else if line.trim() == "!missing" {
            missing = true;
        } else if let Some(rest) = line.strip_prefix('@') {
            stderr.push(rest.to_owned());
        } else if key.is_some() {
            stdout.push(line.to_owned());
        }
    }
    flush(
        &mut key,
        &mut stdout,
        &mut stderr,
        &mut status,
        &mut missing,
        &mut entries,
    );
    entries
}

/// Builds a runner from a transcript's text.
#[must_use]
pub fn runner_from_transcript(text: &str) -> FakeRunner {
    let runner = FakeRunner::new();
    for (key, outcome) in parse_transcript(text) {
        runner.set_outcome(&key, outcome);
    }
    runner
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_transcript_parses_output_exit_and_missing() {
        let text = "# facts\n$ getenforce\nEnforcing\n$ rpm -q chrony\n!exit 1\n@not installed\n$ zfs\n!missing\n";
        let entries = parse_transcript(text);
        assert_eq!(entries.len(), 3);
        match &entries[0].1 {
            FakeOutcome::Output(output) => {
                assert_eq!(output.status(), 0);
                assert_eq!(output.stdout(), "Enforcing");
            }
            other => panic!("unexpected {other:?}"),
        }
        match &entries[1].1 {
            FakeOutcome::Output(output) => {
                assert_eq!(output.status(), 1);
                assert_eq!(output.stderr(), "not installed");
            }
            other => panic!("unexpected {other:?}"),
        }
        match &entries[2].1 {
            FakeOutcome::Error(error) => assert_eq!(error.code, TOOL_MISSING),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_runner_records_calls_and_reports_unrecorded_ones() {
        let runner = runner_from_transcript("$ getenforce\nEnforcing\n");
        let spec = CommandSpec::new("getenforce").unwrap();
        assert_eq!(runner.run(&spec).unwrap().stdout(), "Enforcing");
        assert_eq!(runner.call_keys(), vec!["getenforce".to_owned()]);
        let other = CommandSpec::new("aa-status").unwrap();
        assert!(runner.run(&other).is_err());
    }

    #[test]
    fn the_fake_fs_round_trips_files() {
        let fs = FakeFs::new().with_file("/etc/os-release", "ID=fedora\n");
        assert!(fs.exists(Path::new("/etc/os-release")).unwrap());
        assert_eq!(
            fs.read_to_string(Path::new("/etc/os-release")).unwrap(),
            "ID=fedora\n"
        );
        fs.write_atomic(Path::new("/etc/sysctl.d/90.conf"), "x = 1\n")
            .unwrap();
        assert_eq!(
            fs.read_to_string(Path::new("/etc/sysctl.d/90.conf"))
                .unwrap(),
            "x = 1\n"
        );
        assert_eq!(
            fs.list_dir(Path::new("/etc/sysctl.d")).unwrap(),
            vec!["90.conf".to_owned()]
        );
    }
}
