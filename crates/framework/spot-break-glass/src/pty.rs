//! The interactive terminal boundary: a [`Pty`] trait and its fake.
//!
//! Break-glass is the one place raw shell exists, so the terminal is an explicit seam.
//! The crate carries no real terminal by default: [`FakePty`] is the in-memory double the
//! session tests drive, and the platform PTY implementation lives behind the `real-pty`
//! feature ([`crate::real_pty`]) and is not part of acceptance.

use std::collections::VecDeque;

use crate::error::BreakGlassError;

/// A bidirectional terminal.
///
/// The platform supplies the real implementation; [`FakePty`] is the reference the tests
/// use. Reads and writes never block in the fake.
pub trait Pty {
    /// Reads up to `buf.len()` bytes of terminal output, returning how many were read
    /// (zero means the terminal is closed).
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::PtyUnavailable`] when the terminal cannot be read.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, BreakGlassError>;

    /// Writes terminal input.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::PtyUnavailable`] when the terminal cannot be written.
    fn write(&mut self, data: &[u8]) -> Result<(), BreakGlassError>;

    /// Records a window-size change (columns, rows).
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::PtyUnavailable`] when the terminal cannot be resized.
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), BreakGlassError>;
}

/// An in-memory terminal: a scripted input queue and a captured output buffer.
#[derive(Debug)]
pub struct FakePty {
    input: VecDeque<u8>,
    output: Vec<u8>,
    cols: u16,
    rows: u16,
    closed: bool,
}

impl FakePty {
    /// A terminal pre-loaded with `input` bytes to read back.
    #[must_use]
    pub fn new(input: &[u8]) -> FakePty {
        FakePty {
            input: input.iter().copied().collect(),
            output: Vec::new(),
            cols: 80,
            rows: 24,
            closed: false,
        }
    }

    /// The bytes written to the terminal so far.
    #[must_use]
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// The current window size (columns, rows).
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Whether the terminal's scripted input is exhausted.
    #[must_use]
    pub fn at_end(&self) -> bool {
        self.input.is_empty()
    }
}

impl Pty for FakePty {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, BreakGlassError> {
        if self.closed {
            return Ok(0);
        }
        let mut read = 0;
        while read < buf.len() {
            match self.input.pop_front() {
                Some(byte) => {
                    buf[read] = byte;
                    read += 1;
                }
                None => break,
            }
        }
        if read == 0 && self.input.is_empty() {
            self.closed = true;
        }
        Ok(read)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), BreakGlassError> {
        self.output.extend_from_slice(data);
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), BreakGlassError> {
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
}
