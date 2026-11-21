//! The session recorder: the shared, hash-chained recording component of
//! `spot-break-glass` (WP-ADM-11, ADR-070 section 9), reached through this crate's own trait.
//!
//! An emergency session records every action it takes. The recording is the same component the
//! platform's break-glass path uses: a sequence of timestamped frames, each committing to the
//! one before it, with echo-off input redacted. This crate does not re-implement any of that;
//! [`SessionRecorder`] is a narrow view of it, and the blanket implementation below is the
//! adapter, so a host passes its `MemoryRecorder` or `FileRecorder` unchanged.

use spot_break_glass::{BreakGlassError, Frame, FrameKind, Hash, Recorder};

/// The session-recording component, as this crate uses it: append a frame, read the head hash,
/// count frames, learn whether the recording was truncated.
pub trait SessionRecorder {
    /// Appends one frame, assigning its sequence number and chain link. When `echo` is off and
    /// the kind is redactable the content is replaced with the component's redaction marker.
    ///
    /// # Errors
    ///
    /// The component's own errors: a full recording or an I/O failure.
    fn append(
        &mut self,
        kind: FrameKind,
        at: i64,
        data: &[u8],
        echo: bool,
    ) -> Result<Frame, BreakGlassError>;

    /// The current head hash: a commitment to everything recorded so far.
    fn digest(&self) -> Hash;

    /// How many frames have been appended.
    fn frame_count(&self) -> u64;

    /// Whether the recording reached its size cap.
    fn truncated(&self) -> bool;
}

impl<T: Recorder + ?Sized> SessionRecorder for T {
    fn append(
        &mut self,
        kind: FrameKind,
        at: i64,
        data: &[u8],
        echo: bool,
    ) -> Result<Frame, BreakGlassError> {
        Recorder::append(self, kind, at, data, echo)
    }

    fn digest(&self) -> Hash {
        Recorder::digest(self)
    }

    fn frame_count(&self) -> u64 {
        Recorder::frame_count(self)
    }

    fn truncated(&self) -> bool {
        Recorder::truncated(self)
    }
}
