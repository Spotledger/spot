//! Running one child: argv only (no shell), cleared environment, its own process group,
//! a timeout that kills the whole group, and a byte cap on each output stream.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What to run.
#[derive(Clone, Debug)]
pub struct ChildSpec {
    /// Absolute program path.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Timeout in seconds.
    pub timeout_secs: u64,
    /// Cap per stream, bytes.
    pub output_cap: usize,
    /// Bytes written to the child's stdin (the helper's parameter line), if any.
    pub stdin: Option<Vec<u8>>,
}

/// What it produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildResult {
    /// Exit status, or -1 when killed.
    pub status: i32,
    /// Captured stdout (lossy UTF-8, capped).
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// The child ran past the timeout and its process group was killed.
    pub timed_out: bool,
    /// A stream hit the cap.
    pub truncated: bool,
}

/// Why a child could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnFailed;

/// The seam the helper and its tests use.
pub trait ChildRunner: Send + Sync {
    /// Runs the child.
    ///
    /// # Errors
    /// [`SpawnFailed`] when the program cannot be started.
    fn run(&self, spec: &ChildSpec) -> Result<ChildResult, SpawnFailed>;
}

/// The real runner.
#[derive(Debug, Default)]
pub struct StdChildRunner;

fn drain(
    mut stream: impl std::io::Read + Send + 'static,
    cap: usize,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(kept.len());
                    if n > room {
                        truncated = true;
                    }
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        (kept, truncated)
    })
}

impl ChildRunner for StdChildRunner {
    fn run(&self, spec: &ChildSpec) -> Result<ChildResult, SpawnFailed> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LC_ALL", "C")
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|_| SpawnFailed)?;
        if let (Some(data), Some(mut pipe)) = (spec.stdin.as_ref(), child.stdin.take()) {
            use std::io::Write as _;
            let _ = pipe.write_all(data);
        }
        let out = child.stdout.take().map(|s| drain(s, spec.output_cap));
        let err = child.stderr.take().map(|s| drain(s, spec.output_cap));
        let deadline = Instant::now() + Duration::from_secs(spec.timeout_secs);
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code().unwrap_or(-1),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        kill_group(&mut child);
                        let _ = child.wait();
                        break -1;
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(_) => {
                    kill_group(&mut child);
                    let _ = child.wait();
                    break -1;
                }
            }
        };
        let (stdout, t1) = out.and_then(|h| h.join().ok()).unwrap_or_default();
        let (stderr, t2) = err.and_then(|h| h.join().ok()).unwrap_or_default();
        Ok(ChildResult {
            status,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            timed_out,
            truncated: t1 || t2,
        })
    }
}

/// Kills the child's whole process group (the child is its own group leader).
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = rustix::process::Pid::from_child(child);
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
}
