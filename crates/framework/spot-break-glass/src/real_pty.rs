//! The real terminal, behind the `real-pty` feature.
//!
//! This module is the platform-specific seam and is not part of acceptance. It is written
//! with only `std` so the crate adds no dependency the brief has not approved: a true
//! pseudoterminal needs `portable-pty` (Unix) or the Windows ConPTY API, neither of which
//! is in the workspace yet. Until that dependency is approved, [`SystemPty`] drives a
//! child shell over piped standard I/O — a real interactive process, but not a true PTY —
//! so the feature compiles and the boundary is exercised. Swapping the body for
//! `portable-pty` changes nothing above this module.

use std::io::{Read as _, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::error::BreakGlassError;
use crate::pty::Pty;

/// A child shell driven over piped standard I/O.
#[derive(Debug)]
pub struct SystemPty {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    cols: u16,
    rows: u16,
}

impl SystemPty {
    /// Spawns `shell` (for example `/bin/sh`) with piped standard I/O.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::PtyUnavailable`] when the child cannot be spawned or its pipes
    /// are not available.
    pub fn spawn(shell: &str) -> Result<SystemPty, BreakGlassError> {
        let mut child = Command::new(shell)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| BreakGlassError::PtyUnavailable)?;
        let stdin = child.stdin.take().ok_or(BreakGlassError::PtyUnavailable)?;
        let stdout = child.stdout.take().ok_or(BreakGlassError::PtyUnavailable)?;
        Ok(SystemPty {
            child,
            stdin,
            stdout,
            cols: 80,
            rows: 24,
        })
    }

    /// The current window size (columns, rows). A pipe cannot be resized, so this records
    /// the requested size for the recording without changing the child.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Waits for the child to exit.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::PtyUnavailable`] when the wait fails.
    pub fn wait(&mut self) -> Result<(), BreakGlassError> {
        self.child
            .wait()
            .map(|_| ())
            .map_err(|_| BreakGlassError::PtyUnavailable)
    }
}

impl Pty for SystemPty {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, BreakGlassError> {
        self.stdout
            .read(buf)
            .map_err(|_| BreakGlassError::PtyUnavailable)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), BreakGlassError> {
        self.stdin
            .write_all(data)
            .and_then(|()| self.stdin.flush())
            .map_err(|_| BreakGlassError::PtyUnavailable)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), BreakGlassError> {
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
}

impl Drop for SystemPty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
