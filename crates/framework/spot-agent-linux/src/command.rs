//! The command and filesystem seams: everything a Linux module touches the operating system
//! through.
//!
//! A module never builds a command line and never spawns anything itself. It assembles a
//! [`CommandRequest`] — a program and a **vector** of separate arguments, an allow-listed
//! environment, a timeout and an output cap — and hands it to an injected [`CommandRunner`].
//! No argument is ever concatenated into a string and nothing is ever run through a shell, so
//! a value that contains a space, a quote or a semicolon is one argument, not a command.
//! File reads go through [`FileSystem`], so a test drives the same code as a real host.
//!
//! [`real`](crate::real) holds the implementations over `std::process` and `std::fs`;
//! [`testing`](crate::testing) holds the recording fake the golden transcripts drive.

use std::collections::BTreeMap;

use crate::error::LinuxError;

/// The default timeout of a management command, in whole seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// The default cap on a command's captured output, in bytes (one mebibyte).
pub const DEFAULT_OUTPUT_CAP: usize = 1 << 20;
/// The fixed `PATH` the real runner gives a child when the caller names none. Only the system
/// binary directories: a management command never needs a user's `PATH`.
pub const SAFE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// One command to run: a program, separate arguments, an allow-listed environment, a timeout
/// and an output cap.
///
/// The environment is an explicit allow-list, not a copy of the agent's: a child sees exactly
/// the variables the caller named (plus the fixed `PATH`, `LANG` and `LC_ALL` the real runner
/// adds when absent), never the agent's own secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRequest {
    /// The program to run, by name (resolved on `PATH`) or by absolute path.
    pub program: String,
    /// The arguments, each one a separate `argv` entry.
    pub args: Vec<String>,
    /// The environment variables the child is allowed to see.
    pub env: BTreeMap<String, String>,
    /// The timeout, in whole seconds.
    pub timeout_secs: u64,
    /// The cap on captured output, in bytes.
    pub output_cap: usize,
}

impl CommandRequest {
    /// A request for `program` with the default timeout and output cap.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            output_cap: DEFAULT_OUTPUT_CAP,
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends every argument in `args`, in order.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Allows one environment variable through to the child.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Sets the timeout, in whole seconds.
    #[must_use]
    pub fn timeout_secs(mut self, seconds: u64) -> Self {
        self.timeout_secs = seconds;
        self
    }

    /// Sets the output cap, in bytes.
    #[must_use]
    pub fn output_cap(mut self, bytes: usize) -> Self {
        self.output_cap = bytes;
        self
    }

    /// The command as a shell-free, display-only string: the program and each argument,
    /// separated by single spaces. **Never** run; it exists so a preview can show a human what
    /// would happen. An argument is shown verbatim and is not re-parseable as a command line,
    /// which is the point.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::with_capacity(self.program.len() + 8);
        out.push_str(&self.program);
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

/// What running one command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// The exit status, or `-1` when the process was killed for timing out.
    pub status: i32,
    /// The captured standard output, as text (lossy for non-UTF-8 bytes).
    pub stdout: String,
    /// The captured standard error, as text.
    pub stderr: String,
    /// Whether the command was killed after its timeout.
    pub timed_out: bool,
    /// Whether either stream hit the output cap (its tail was dropped).
    pub truncated: bool,
}

impl CommandOutput {
    /// Whether the command exited successfully.
    #[must_use]
    pub fn success(&self) -> bool {
        !self.timed_out && self.status == 0
    }

    /// The non-empty lines of standard output, trimmed of surrounding whitespace.
    #[must_use]
    pub fn stdout_lines(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect()
    }
}

/// Runs commands for a module. The one seam every command passes through.
///
/// Implementations must never involve a shell and must never merge arguments into a string:
/// the vector is the command.
pub trait CommandRunner: Send + Sync {
    /// Runs `request` and returns its output.
    ///
    /// # Errors
    /// [`LinuxError::CommandSpawn`] when the program cannot be spawned,
    /// [`LinuxError::CommandTimeout`] when it is killed after its timeout, and
    /// [`LinuxError::OutputTooLarge`] when its output exceeds the cap.
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput, LinuxError>;
}

/// Reads files for a module. The seam every filesystem read passes through.
pub trait FileSystem: Send + Sync {
    /// The whole contents of `path`, as text.
    ///
    /// # Errors
    /// [`LinuxError::Io`] when the file is missing or unreadable.
    fn read_to_string(&self, path: &str) -> Result<String, LinuxError>;

    /// Whether `path` exists.
    ///
    /// # Errors
    /// [`LinuxError::Io`] when existence cannot be determined.
    fn exists(&self, path: &str) -> Result<bool, LinuxError>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_request_keeps_arguments_separate() {
        let request = CommandRequest::new("dnf")
            .arg("-y")
            .arg("install")
            .arg("nginx; rm -rf /")
            .env("LC_ALL", "C");
        assert_eq!(request.args.len(), 3);
        assert_eq!(request.args[2], "nginx; rm -rf /");
        assert_eq!(request.env.get("LC_ALL").map(String::as_str), Some("C"));
        assert_eq!(request.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(request.output_cap, DEFAULT_OUTPUT_CAP);
    }

    #[test]
    fn display_is_flat_and_shell_free() {
        let request =
            CommandRequest::new("systemctl").args(["--no-pager", "start", "sshd.service"]);
        assert_eq!(request.display(), "systemctl --no-pager start sshd.service");
    }

    #[test]
    fn output_success_and_lines() {
        let output = CommandOutput {
            status: 0,
            stdout: "a\n\n b \n".to_owned(),
            stderr: String::new(),
            timed_out: false,
            truncated: false,
        };
        assert!(output.success());
        assert_eq!(output.stdout_lines(), vec!["a", "b"]);
        let killed = CommandOutput {
            status: -1,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
            truncated: false,
        };
        assert!(!killed.success());
    }
}
