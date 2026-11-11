//! The real seams over `std::process` and `std::fs`.
//!
//! [`StdCommandRunner`] spawns a program directly — never a shell — with the child's
//! environment cleared and then rebuilt from the request's allow-list (plus a fixed `PATH`,
//! `LANG` and `LC_ALL` when the caller names none), its standard input closed, and a bounded
//! wait: a command that outlives its timeout is killed and reported. Output is captured with a
//! hard cap, so a runaway tool cannot exhaust the agent's memory.
//!
//! The monotonic `Instant` used for the timeout is a process timer, not the wall clock: the
//! modules never read the time of day (time is an argument everywhere else).
//!
//! [`StdFileSystem`] is the plain `std::fs` reader.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::command::{CommandOutput, CommandRequest, CommandRunner, FileSystem, SAFE_PATH};
use crate::error::LinuxError;

/// How often the real runner checks whether a child has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The real command runner.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdCommandRunner;

impl StdCommandRunner {
    /// A runner.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for StdCommandRunner {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput, LinuxError> {
        let mut command = Command::new(&request.program);
        command.args(&request.args);
        // Clear the inherited environment, then rebuild it from the allow-list. The fixed
        // PATH/LANG/LC_ALL are defaults a caller may override.
        command.env_clear();
        command.env("PATH", SAFE_PATH);
        command.env("LANG", "C");
        command.env("LC_ALL", "C");
        for (name, value) in &request.env {
            command.env(name, value);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);

        let mut child = command.spawn().map_err(|error| LinuxError::CommandSpawn {
            program: request.program.clone(),
            detail: error.to_string(),
        })?;

        let cap = request.output_cap;
        let pgid = child.id();
        let stdout_reader = spawn_reader(child.stdout.take(), cap);
        let stderr_reader = spawn_reader(child.stderr.take(), cap);

        let timeout = Duration::from_secs(request.timeout_secs.max(1));
        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code().unwrap_or(-1),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        kill_group_and_reap(&mut child, pgid);
                        timed_out = true;
                        break -1;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(error) => {
                    signal_group("KILL", pgid);
                    return Err(LinuxError::CommandSpawn {
                        program: request.program.clone(),
                        detail: error.to_string(),
                    });
                }
            }
        };

        // A grandchild (a daemon the command started) may keep a pipe open after the child is
        // gone. Wait for the readers only until the grace deadline, then detach them: they hold
        // no lock on the caller and end when the pipe finally closes.
        let reader_deadline = Instant::now() + READER_GRACE;
        let (out_bytes, out_truncated) = stdout_reader.collect(reader_deadline);
        let (err_bytes, err_truncated) = stderr_reader.collect(reader_deadline);
        // Whatever still holds the pipes belongs to the command: end the group.
        signal_group("KILL", pgid);

        if timed_out {
            return Err(LinuxError::CommandTimeout {
                program: request.program.clone(),
                timeout_secs: request.timeout_secs,
            });
        }
        if out_truncated || err_truncated {
            return Err(LinuxError::OutputTooLarge {
                program: request.program.clone(),
                cap,
            });
        }

        Ok(CommandOutput {
            status,
            stdout: String::from_utf8_lossy(&out_bytes).into_owned(),
            stderr: String::from_utf8_lossy(&err_bytes).into_owned(),
            timed_out: false,
            truncated: false,
        })
    }
}

/// How long the readers get to finish once the child has exited or been killed.
const READER_GRACE: Duration = Duration::from_millis(500);

/// How long a child gets between SIGTERM and SIGKILL on a timeout.
const TERM_GRACE: Duration = Duration::from_millis(300);

/// What a reader thread has gathered so far.
#[derive(Default)]
struct Captured {
    kept: Vec<u8>,
    truncated: bool,
}

/// A running reader thread plus the buffer it fills, so partial output survives a detach.
struct Reader {
    captured: Arc<Mutex<Captured>>,
    done: mpsc::Receiver<()>,
}

impl Reader {
    /// Waits until `deadline` for the reader to reach end of file, then returns what it has
    /// (kept bytes, truncated). A reader still blocked is left detached.
    fn collect(self, deadline: Instant) -> (Vec<u8>, bool) {
        let wait = deadline.saturating_duration_since(Instant::now());
        let _ = self.done.recv_timeout(wait);
        match self.captured.lock() {
            Ok(mut guard) => (std::mem::take(&mut guard.kept), guard.truncated),
            Err(_) => (Vec::new(), false),
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(reader: Option<R>, cap: usize) -> Reader {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let (sender, done) = mpsc::channel();
    let shared = Arc::clone(&captured);
    std::thread::spawn(move || {
        read_capped(reader, cap, &shared);
        let _ = sender.send(());
    });
    Reader { captured, done }
}

/// Sends `signal` (a `kill` signal name) to the whole process group `pgid`. The child was
/// started as its own group leader, so the group id is its pid. Uses `kill(1)` because the
/// crate forbids `unsafe`.
fn signal_group(signal: &str, pgid: u32) {
    let _ = Command::new("kill")
        .env_clear()
        .env("PATH", SAFE_PATH)
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{pgid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// SIGTERM the group, give it a short grace, then SIGKILL the group and reap the child.
fn kill_group_and_reap(child: &mut Child, pgid: u32) {
    signal_group("TERM", pgid);
    let until = Instant::now() + TERM_GRACE;
    while Instant::now() < until {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    signal_group("KILL", pgid);
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads all of `reader` into `into`, keeping at most `cap` bytes but always draining to end of
/// file so the child never blocks on a full pipe.
fn read_capped<R: Read>(reader: Option<R>, cap: usize, into: &Mutex<Captured>) {
    let Some(mut reader) = reader else {
        return;
    };
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let Ok(mut guard) = into.lock() else {
                    break;
                };
                let room = cap.saturating_sub(guard.kept.len());
                let take = room.min(read);
                guard.kept.extend_from_slice(&buffer[..take]);
                if take < read {
                    guard.truncated = true;
                }
            }
        }
    }
}

/// The real filesystem reader.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFileSystem;

impl StdFileSystem {
    /// A filesystem reader.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for StdFileSystem {
    fn read_to_string(&self, path: &str) -> Result<String, LinuxError> {
        std::fs::read_to_string(path).map_err(|error| LinuxError::Io {
            path: path.to_owned(),
            detail: error.to_string(),
        })
    }

    fn exists(&self, path: &str) -> Result<bool, LinuxError> {
        Ok(std::path::Path::new(path).exists())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The first of the candidate absolute paths that exists, if any. The real-runner tests
    /// need a program that is certainly present and never a shell.
    fn find(candidates: &[&'static str]) -> Option<&'static str> {
        candidates
            .iter()
            .find(|path| std::path::Path::new(path).exists())
            .copied()
    }

    #[test]
    fn the_real_runner_runs_a_program_with_separate_arguments() {
        let Some(echo) = find(&["/usr/bin/echo", "/bin/echo"]) else {
            return;
        };
        let runner = StdCommandRunner::new();
        let request = CommandRequest::new(echo).args(["-n", "hello", "world"]);
        let output = runner.run(&request).unwrap();
        assert!(output.success(), "{output:?}");
        assert_eq!(output.stdout, "hello world");
    }

    #[test]
    fn the_real_runner_reports_a_missing_program() {
        let runner = StdCommandRunner::new();
        let request = CommandRequest::new("/nonexistent/spot-agent-linux-test");
        let error = runner.run(&request).unwrap_err();
        assert_eq!(error.code(), crate::error::COMMAND_SPAWN);
    }

    #[test]
    fn the_real_runner_kills_a_command_that_outlives_its_timeout() {
        let Some(sleep) = find(&["/usr/bin/sleep", "/bin/sleep"]) else {
            return;
        };
        let runner = StdCommandRunner::new();
        let request = CommandRequest::new(sleep).arg("5").timeout_secs(1);
        let error = runner.run(&request).unwrap_err();
        assert_eq!(error.code(), crate::error::COMMAND_TIMEOUT);
    }

    #[test]
    fn a_grandchild_holding_the_pipes_does_not_hang_the_timeout() {
        let Some(sh) = find(&["/usr/bin/sh", "/bin/sh"]) else {
            return;
        };
        let runner = StdCommandRunner::new();
        let request = CommandRequest::new(sh)
            .args(["-c", "sleep 1000 & sleep 1000"])
            .timeout_secs(1);
        let started = Instant::now();
        let error = runner.run(&request).unwrap_err();
        assert_eq!(error.code(), crate::error::COMMAND_TIMEOUT);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_command_ignoring_sigterm_is_still_killed() {
        let Some(sh) = find(&["/usr/bin/sh", "/bin/sh"]) else {
            return;
        };
        let runner = StdCommandRunner::new();
        let request = CommandRequest::new(sh)
            .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
            .timeout_secs(1);
        let started = Instant::now();
        let error = runner.run(&request).unwrap_err();
        assert_eq!(error.code(), crate::error::COMMAND_TIMEOUT);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_daemon_left_holding_the_pipe_does_not_block_a_finished_command() {
        let Some(sh) = find(&["/usr/bin/sh", "/bin/sh"]) else {
            return;
        };
        let runner = StdCommandRunner::new();
        let request = CommandRequest::new(sh)
            .args(["-c", "echo done; sleep 1000 &"])
            .timeout_secs(5);
        let started = Instant::now();
        let output = runner.run(&request).unwrap();
        assert_eq!(output.stdout, "done\n");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn output_larger_than_the_cap_is_reported_not_buffered() {
        let Some(sh) = find(&["/usr/bin/sh", "/bin/sh"]) else {
            return;
        };
        let runner = StdCommandRunner::new();
        let mut request = CommandRequest::new(sh)
            .args(["-c", "head -c 100000 /dev/zero"])
            .timeout_secs(5);
        request.output_cap = 1024;
        let error = runner.run(&request).unwrap_err();
        assert_eq!(error.code(), crate::error::OUTPUT_TOO_LARGE);
    }

    #[test]
    fn the_real_filesystem_reads_and_reports_existence() {
        let fs = StdFileSystem::new();
        assert!(fs.exists("/").unwrap());
        assert!(!fs.exists("/nonexistent/spot-agent-linux-test").unwrap());
        let error = fs
            .read_to_string("/nonexistent/spot-agent-linux-test")
            .unwrap_err();
        assert_eq!(error.code(), crate::error::IO);
    }
}
