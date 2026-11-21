//! The `tracing` integration: a `MakeWriter` that redacts each formatted event before it is
//! written (WP-PRD-08.T1 deliverable 5), behind the `tracing` feature.
//!
//! `tracing-subscriber` formats one event into text and hands it to the wrapper as one
//! write call, so per-call redaction sees the whole event. Callers split across write calls
//! are redacted per chunk; a secret split across two chunks of one event could therefore
//! survive — pass well-formed events (the fmt layer does).

use std::io;
use std::sync::Arc;

use tracing_subscriber::fmt::MakeWriter;

use crate::redactor::Redactor;

/// A `MakeWriter` that runs every written chunk through the [`Redactor`] first. Build it
/// once and hand it to `tracing_subscriber::fmt().with_writer(...)`.
pub struct RedactingMakeWriter<M> {
    redactor: Arc<Redactor>,
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    /// Wraps `inner` (stdout, a file, a network sink) with `redactor`.
    pub fn new(redactor: Arc<Redactor>, inner: M) -> Self {
        Self { redactor, inner }
    }
}

impl<'a, M: for<'w> MakeWriter<'w>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<'a, <M as MakeWriter<'a>>::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        // The redactor is shared through `Arc` at construction, so `'a` borrows are fine.
        RedactingWriter {
            redactor: &self.redactor,
            inner: self.inner.make_writer(),
        }
    }
}

/// The writer redacting each chunk before passing it on.
pub struct RedactingWriter<'r, W> {
    redactor: &'r Redactor,
    inner: W,
}

impl<W: io::Write> io::Write for RedactingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Lossy: logs are text; broken bytes never panic and never skip redaction.
        let text = String::from_utf8_lossy(buf);
        let redacted = self.redactor.redact_str(&text);
        self.inner.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
