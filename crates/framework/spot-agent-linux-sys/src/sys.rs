//! The two seams every module reaches the machine through: [`CommandRunner`] (argv
//! vectors, never a shell) and [`FileSystem`] (atomic writes).
//!
//! The modules are platform logic over these traits, so the same code runs on a real
//! Fedora/RHEL/Ubuntu host through [`crate::real::StdCommandRunner`] and
//! [`crate::real::StdFileSystem`], and in a test through the recording fakes in
//! [`crate::testing`]. A [`CommandSpec`] is validated when it is built: the program is a
//! bare name or an absolute path, no shell is ever named, every argument is a separate
//! argv entry with no NUL, and every environment key is on the allow-list. There is
//! therefore no string a caller can build that a shell would interpret.

use std::path::{Path, PathBuf};

use crate::error::{COMMAND_FAILED, COMMAND_OUTPUT_TOO_LARGE, SysError, SysResult, TOOL_MISSING};

/// How long a command may run when the caller does not say.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// How much of a command's combined output is kept when the caller does not say.
pub const DEFAULT_OUTPUT_CAP: usize = 1 << 20;

/// The environment keys a command may carry. Nothing else is ever passed to a child, so
/// no ambient secret (a token, a `PGPASSWORD`, an `AWS_*`) can leak into a tool.
pub const ENV_ALLOW_LIST: &[&str] = &["LANG", "LC_ALL", "LC_CTYPE", "PATH", "TERM", "TZ"];

/// Program names that would turn an argv vector back into a shell string. Refused at
/// [`CommandSpec::new`], so no module can accidentally build a shell pipeline.
pub const SHELL_PROGRAMS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "zsh",
    "ksh",
    "csh",
    "tcsh",
    "fish",
    "cmd",
    "powershell",
    "pwsh",
];

/// One command to run: the program, its arguments, an allow-listed environment, a timeout
/// and an output cap.
///
/// Constructed only through [`CommandSpec::new`] and the builder methods, each of which
/// validates its input, so a malformed spec cannot exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    timeout_secs: u64,
    output_cap: usize,
}

impl CommandSpec {
    /// Starts a spec for `program`, refusing an empty name, a NUL, a character outside
    /// `[A-Za-z0-9_./+-]`, and any shell.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] when the program is not a safe bare name or path.
    pub fn new(program: &str) -> SysResult<Self> {
        validate_program(program)?;
        Ok(Self {
            program: program.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            output_cap: DEFAULT_OUTPUT_CAP,
        })
    }

    /// Appends one argument (one argv entry, never joined into a shell string).
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] when the argument contains a NUL.
    pub fn arg(mut self, argument: impl Into<String>) -> SysResult<Self> {
        let argument = argument.into();
        validate_argument(&argument)?;
        self.args.push(argument);
        Ok(self)
    }

    /// Appends several arguments in order.
    ///
    /// # Errors
    /// The same refusal as [`CommandSpec::arg`].
    pub fn args<I, S>(mut self, arguments: I) -> SysResult<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for argument in arguments {
            self = self.arg(argument)?;
        }
        Ok(self)
    }

    /// Adds one environment pair; the key must be on [`ENV_ALLOW_LIST`].
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for an unknown key, a malformed key or a NUL value.
    pub fn env(mut self, key: &str, value: impl Into<String>) -> SysResult<Self> {
        let value = value.into();
        if !ENV_ALLOW_LIST.contains(&key) {
            return Err(SysError::invalid(format!(
                "the environment key {key} is not on the allow-list"
            )));
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(SysError::invalid(format!(
                "the environment key {key} is not upper-case"
            )));
        }
        if value.contains('\0') {
            return Err(SysError::invalid("an environment value contains a NUL"));
        }
        self.env.push((key.to_owned(), value));
        Ok(self)
    }

    /// Sets the timeout in whole seconds.
    #[must_use]
    pub fn timeout_secs(mut self, seconds: u64) -> Self {
        self.timeout_secs = seconds;
        self
    }

    /// Sets the output cap in bytes.
    #[must_use]
    pub fn output_cap(mut self, bytes: usize) -> Self {
        self.output_cap = bytes;
        self
    }

    /// The program name or path.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The arguments, in order.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.args
    }

    /// The environment pairs, in insertion order.
    #[must_use]
    pub fn env_pairs(&self) -> &[(String, String)] {
        &self.env
    }

    /// The timeout in whole seconds.
    #[must_use]
    pub fn timeout(&self) -> u64 {
        self.timeout_secs
    }

    /// The output cap in bytes.
    #[must_use]
    pub fn cap(&self) -> usize {
        self.output_cap
    }

    /// The canonical key a transcript matches on: the program and arguments joined by a
    /// single space. It is never executed; it only identifies a command.
    #[must_use]
    pub fn argv_key(&self) -> String {
        let mut key = String::with_capacity(self.program.len() + self.args.len() * 8);
        key.push_str(&self.program);
        for argument in &self.args {
            key.push(' ');
            key.push_str(argument);
        }
        key
    }
}

/// Whether `program` is a safe bare name or absolute path and not a shell.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] with the reason.
pub fn validate_program(program: &str) -> SysResult<()> {
    if program.is_empty() {
        return Err(SysError::invalid("a command needs a program name"));
    }
    if program.contains('\0') {
        return Err(SysError::invalid("a program name contains a NUL"));
    }
    if !program
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '+' | '-'))
    {
        return Err(SysError::invalid(format!(
            "the program {program} contains a character outside [A-Za-z0-9_./+-]"
        )));
    }
    let base = program.rsplit('/').next().unwrap_or(program);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    if SHELL_PROGRAMS.contains(&base) {
        return Err(SysError::invalid(format!(
            "the program {program} is a shell; commands are argv vectors, never shell strings"
        )));
    }
    Ok(())
}

/// Whether `argument` is a valid argv entry (no NUL).
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] when it contains a NUL.
pub fn validate_argument(argument: &str) -> SysResult<()> {
    if argument.contains('\0') {
        return Err(SysError::invalid("an argument contains a NUL"));
    }
    Ok(())
}

/// What running one command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    status: i32,
    stdout: String,
    stderr: String,
    truncated: bool,
}

impl CommandOutput {
    /// Builds an output from an exit status and the two streams.
    #[must_use]
    pub fn new(status: i32, stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
            truncated: false,
        }
    }

    /// Marks the output as truncated at the spec's cap.
    #[must_use]
    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// The exit status (`0` is success).
    #[must_use]
    pub fn status(&self) -> i32 {
        self.status
    }

    /// Whether the command exited zero.
    #[must_use]
    pub fn success(&self) -> bool {
        self.status == 0
    }

    /// The standard output.
    #[must_use]
    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    /// The standard error.
    #[must_use]
    pub fn stderr(&self) -> &str {
        &self.stderr
    }

    /// Whether the output hit the cap and was cut.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The standard output without trailing whitespace.
    #[must_use]
    pub fn stdout_trimmed(&self) -> &str {
        self.stdout.trim_end()
    }

    /// Fails with [`COMMAND_FAILED`] unless the command exited zero. `what` is a short,
    /// secret-free description of the operation; the message names the status and the
    /// first line of standard error.
    ///
    /// # Errors
    /// [`COMMAND_FAILED`] on a non-zero status.
    pub fn require_success(&self, what: &str) -> SysResult<()> {
        if self.success() {
            return Ok(());
        }
        let reason = self
            .stderr
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(200)
            .collect::<String>();
        if reason.is_empty() {
            Err(SysError::new(
                COMMAND_FAILED,
                format!("{what} failed with status {}", self.status),
            ))
        } else {
            Err(SysError::new(
                COMMAND_FAILED,
                format!("{what} failed with status {}: {reason}", self.status),
            ))
        }
    }

    /// Fails with [`COMMAND_OUTPUT_TOO_LARGE`] when the output was truncated, so a module
    /// never parses a cut document as if it were whole.
    ///
    /// # Errors
    /// [`COMMAND_OUTPUT_TOO_LARGE`] when the output hit the cap.
    pub fn require_complete(&self, what: &str) -> SysResult<()> {
        if self.truncated {
            return Err(SysError::new(
                COMMAND_OUTPUT_TOO_LARGE,
                format!("{what} produced more output than the cap allows"),
            ));
        }
        Ok(())
    }
}

/// Runs one [`CommandSpec`] and returns its output. Implementations never involve a shell.
pub trait CommandRunner: Send + Sync {
    /// Runs `spec`.
    ///
    /// # Errors
    /// [`TOOL_MISSING`] when the program cannot be started, [`crate::error::COMMAND_TIMEOUT`]
    /// when it runs past its timeout, and [`crate::error::IO`] for an underlying failure.
    fn run(&self, spec: &CommandSpec) -> SysResult<CommandOutput>;
}

/// The filesystem seam. Writes are atomic so a crash never leaves a half-written config.
pub trait FileSystem: Send + Sync {
    /// The whole contents of `path` as UTF-8.
    ///
    /// # Errors
    /// [`crate::error::IO`] when the file is missing or not valid UTF-8.
    fn read_to_string(&self, path: &Path) -> SysResult<String>;

    /// Whether `path` exists (a file or a directory).
    ///
    /// # Errors
    /// [`crate::error::IO`] when existence cannot be determined.
    fn exists(&self, path: &Path) -> SysResult<bool>;

    /// Whether `path` is a directory.
    ///
    /// # Errors
    /// [`crate::error::IO`] when it cannot be determined.
    fn is_dir(&self, path: &Path) -> SysResult<bool>;

    /// The names directly under `path`, sorted.
    ///
    /// # Errors
    /// [`crate::error::IO`] when `path` is not a readable directory.
    fn list_dir(&self, path: &Path) -> SysResult<Vec<String>>;

    /// Writes `contents` to `path` atomically (a temporary file, then a rename).
    ///
    /// # Errors
    /// [`crate::error::IO`] when the write fails.
    fn write_atomic(&self, path: &Path, contents: &str) -> SysResult<()>;

    /// Removes one file.
    ///
    /// # Errors
    /// [`crate::error::IO`] when it cannot be removed.
    fn remove_file(&self, path: &Path) -> SysResult<()>;

    /// Creates `path` and every missing parent.
    ///
    /// # Errors
    /// [`crate::error::IO`] when it cannot be created.
    fn create_dir_all(&self, path: &Path) -> SysResult<()>;

    /// Renames `from` over `to`, atomically where the platform allows.
    ///
    /// # Errors
    /// [`crate::error::IO`] when the rename fails.
    fn rename(&self, from: &Path, to: &Path) -> SysResult<()>;

    /// The size of the file at `path` in bytes.
    ///
    /// # Errors
    /// [`crate::error::IO`] when it cannot be read.
    fn metadata_len(&self, path: &Path) -> SysResult<u64>;

    /// The target of the symbolic link at `path`.
    ///
    /// # Errors
    /// [`crate::error::IO`] when it is not a link or cannot be read.
    fn read_link(&self, path: &Path) -> SysResult<PathBuf>;

    /// Takes an exclusive advisory lock on the file at `path` (created when missing)
    /// **without blocking**: `Ok(Some(guard))` when it is now held, `Ok(None)` when another
    /// holder (a thread or another process) has it. Dropping the guard releases the lock,
    /// and a holder that dies loses it, so a crashed process cannot wedge the others.
    ///
    /// # Errors
    /// [`crate::error::IO`] when the lock file cannot be opened or locked for another reason.
    fn try_lock_exclusive(&self, path: &Path) -> SysResult<Option<FileLock>>;
}

/// A held advisory file lock; dropping it releases the lock.
pub type FileLock = Box<dyn Send>;

/// Waits between lock attempts. A seam so no code here reads a clock or really sleeps in a
/// test.
pub trait Sleeper: Send + Sync {
    /// Pauses for `duration`.
    fn sleep(&self, duration: std::time::Duration);
}

/// The conventional [`TOOL_MISSING`] failure for a program that could not be started.
#[must_use]
pub fn tool_missing(program: &str) -> SysError {
    SysError::new(
        TOOL_MISSING,
        format!("the required tool {program} is not installed"),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_spec_keeps_arguments_separate() {
        let spec = CommandSpec::new("lsblk")
            .unwrap()
            .args(["-J", "-b", "-o", "NAME,KNAME,TYPE,SIZE"])
            .unwrap();
        assert_eq!(spec.program(), "lsblk");
        assert_eq!(spec.arguments()[0], "-J");
        assert_eq!(spec.argv_key(), "lsblk -J -b -o NAME,KNAME,TYPE,SIZE");
        assert_eq!(spec.timeout(), DEFAULT_TIMEOUT_SECS);
        assert_eq!(spec.cap(), DEFAULT_OUTPUT_CAP);
    }

    #[test]
    fn shells_and_bad_programs_are_refused() {
        assert!(CommandSpec::new("").is_err());
        assert!(CommandSpec::new("sh").is_err());
        assert!(CommandSpec::new("/bin/bash").is_err());
        assert!(CommandSpec::new("powershell.exe").is_err());
        assert!(CommandSpec::new("lsblk; rm -rf /").is_err());
        assert!(CommandSpec::new("a b").is_err());
        assert!(CommandSpec::new("nul\0").is_err());
        assert!(CommandSpec::new("/usr/sbin/getenforce").is_ok());
        assert!(CommandSpec::new("update-ca-trust").is_ok());
    }

    #[test]
    fn arguments_refuse_a_nul() {
        assert!(CommandSpec::new("echo").unwrap().arg("a\0b").is_err());
        assert!(CommandSpec::new("echo").unwrap().arg("-n").is_ok());
    }

    #[test]
    fn the_environment_is_allow_listed() {
        let ok = CommandSpec::new("date").unwrap().env("TZ", "UTC").unwrap();
        assert_eq!(ok.env_pairs()[0].0, "TZ");
        assert!(
            CommandSpec::new("date")
                .unwrap()
                .env("PGPASSWORD", "x")
                .is_err()
        );
        assert!(CommandSpec::new("date").unwrap().env("lower", "x").is_err());
    }

    #[test]
    fn require_success_names_the_status_and_first_error_line() {
        let failed = CommandOutput::new(1, "", "boom\nmore");
        let error = failed.require_success("the list").unwrap_err();
        assert_eq!(error.code, COMMAND_FAILED);
        assert!(error.message.contains("status 1"));
        assert!(error.message.contains("boom"));
        assert!(!error.message.contains("more"));
        assert!(CommandOutput::new(0, "ok", "").require_success("x").is_ok());
    }

    #[test]
    fn a_truncated_output_is_not_parseable() {
        let cut = CommandOutput::new(0, "partial", "").truncated(true);
        assert_eq!(
            cut.require_complete("the dump").unwrap_err().code,
            COMMAND_OUTPUT_TOO_LARGE
        );
    }
}
