//! The interactive-session recorder: a hash-chained, append-only, redacting frame log.
//!
//! Break-glass is the one place raw shell exists, so every session is recorded and the
//! recording is visible to the customer. A recording is a sequence of timestamped
//! [`Frame`]s — terminal input and output bytes, window-size changes, and the explicit
//! truncation marker — each committing to the one before it by SHA-256, so any edit,
//! removal or reorder is detected. Input frames whose echo was off (a password prompt)
//! are marked and their bytes replaced with [`REDACTED`]; the secret never reaches the
//! file.
//!
//! The recorder is a component, not the whole crate: the session state machine
//! ([`crate::session`]) is pure, and the recorder is the one part that touches the file
//! system, because the brief requires an append-only recording on disk. The frame
//! encoding, the hash chain, [`MemoryRecorder`] and [`replay`] are pure and testable
//! without any file.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::BreakGlassError;

/// A SHA-256 hash.
pub type Hash = [u8; 32];

/// The recording file's magic bytes.
const FRAME_MAGIC: &[u8; 4] = b"SBBG";
/// The recording format version.
const RECORDING_VERSION: u8 = 1;
/// The domain separator of a frame hash.
const HASH_DOMAIN: &[u8] = b"spot.break-glass.frame.v1\0";
/// The domain separator of the genesis hash (the `prev_hash` of frame 1).
const GENESIS_DOMAIN: &[u8] = b"spot.break-glass.genesis.v1\0";
/// The manifest format version.
const MANIFEST_VERSION: u32 = 1;
/// The bytes written in place of a redacted input frame's content.
pub const REDACTED: &[u8] = b"[redacted input]";

/// The lower-case hex text of a byte string.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The `prev_hash` of the first frame of `session`'s recording.
#[must_use]
pub fn genesis_hash(session: &str) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(GENESIS_DOMAIN);
    hasher.update(session.as_bytes());
    hasher.finalize().into()
}

/// What a frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    /// Bytes the operator typed into the terminal.
    Input,
    /// Bytes the terminal produced.
    Output,
    /// A terminal window-size change (columns then rows, each a big-endian `u16`).
    Resize,
    /// The explicit marker written when the recording reached its size cap.
    Truncation,
}

impl FrameKind {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FrameKind::Input => "input",
            FrameKind::Output => "output",
            FrameKind::Resize => "resize",
            FrameKind::Truncation => "truncation",
        }
    }

    /// The on-disk code.
    fn code(self) -> u8 {
        match self {
            FrameKind::Input => 0,
            FrameKind::Output => 1,
            FrameKind::Resize => 2,
            FrameKind::Truncation => 3,
        }
    }

    /// The kind for an on-disk code, if any.
    fn from_code(code: u8) -> Option<FrameKind> {
        match code {
            0 => Some(FrameKind::Input),
            1 => Some(FrameKind::Output),
            2 => Some(FrameKind::Resize),
            3 => Some(FrameKind::Truncation),
            _ => None,
        }
    }

    /// Whether a frame of this kind is redacted when echo was off.
    #[must_use]
    pub fn is_redactable(self) -> bool {
        matches!(self, FrameKind::Input)
    }
}

/// One recorded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Position in the recording; starts at 1 and has no gaps.
    pub seq: u64,
    /// When the frame was recorded (unix seconds, supplied by the caller).
    pub at: i64,
    /// What the frame carries.
    pub kind: FrameKind,
    /// Whether the content was replaced with [`REDACTED`] (echo was off).
    pub redacted: bool,
    /// The frame's content.
    pub data: Vec<u8>,
    /// The previous frame's hash; the genesis hash for frame 1.
    pub prev_hash: Hash,
    /// `SHA-256(domain || seq || at || kind || flags || len || data || prev_hash)`.
    pub hash: Hash,
}

impl Frame {
    /// Builds a frame and computes its hash.
    #[must_use]
    pub fn new(
        seq: u64,
        at: i64,
        kind: FrameKind,
        redacted: bool,
        data: Vec<u8>,
        prev_hash: Hash,
    ) -> Frame {
        let hash = frame_hash(seq, at, kind, redacted, &data, &prev_hash);
        Frame {
            seq,
            at,
            kind,
            redacted,
            data,
            prev_hash,
            hash,
        }
    }

    /// An input frame: when `echo` is off, the bytes are replaced with [`REDACTED`] and
    /// the frame is marked redacted.
    #[must_use]
    pub fn input(at: i64, data: &[u8], echo: bool, prev_hash: Hash, seq: u64) -> Frame {
        if echo {
            Frame::new(seq, at, FrameKind::Input, false, data.to_vec(), prev_hash)
        } else {
            Frame::new(
                seq,
                at,
                FrameKind::Input,
                true,
                REDACTED.to_vec(),
                prev_hash,
            )
        }
    }

    /// An output frame.
    #[must_use]
    pub fn output(at: i64, data: &[u8], prev_hash: Hash, seq: u64) -> Frame {
        Frame::new(seq, at, FrameKind::Output, false, data.to_vec(), prev_hash)
    }

    /// A window-size frame.
    #[must_use]
    pub fn resize(at: i64, cols: u16, rows: u16, prev_hash: Hash, seq: u64) -> Frame {
        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&cols.to_be_bytes());
        data.extend_from_slice(&rows.to_be_bytes());
        Frame::new(seq, at, FrameKind::Resize, false, data, prev_hash)
    }

    /// The explicit truncation marker, naming the last frame kept.
    #[must_use]
    pub fn truncation(at: i64, dropped_after_seq: u64, prev_hash: Hash, seq: u64) -> Frame {
        Frame::new(
            seq,
            at,
            FrameKind::Truncation,
            false,
            dropped_after_seq.to_be_bytes().to_vec(),
            prev_hash,
        )
    }

    /// The hash the frame's current contents produce (compare with [`Frame::hash`]).
    #[must_use]
    pub fn recompute_hash(&self) -> Hash {
        frame_hash(
            self.seq,
            self.at,
            self.kind,
            self.redacted,
            &self.data,
            &self.prev_hash,
        )
    }

    /// The window size a [`FrameKind::Resize`] frame carries.
    #[must_use]
    pub fn resize_size(&self) -> Option<(u16, u16)> {
        if self.kind != FrameKind::Resize || self.data.len() != 4 {
            return None;
        }
        let cols = u16::from_be_bytes([self.data[0], self.data[1]]);
        let rows = u16::from_be_bytes([self.data[2], self.data[3]]);
        Some((cols, rows))
    }

    /// The last kept sequence a [`FrameKind::Truncation`] frame names.
    #[must_use]
    pub fn dropped_after_seq(&self) -> Option<u64> {
        if self.kind != FrameKind::Truncation || self.data.len() != 8 {
            return None;
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.data);
        Some(u64::from_be_bytes(bytes))
    }

    /// The frame's canonical bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(95 + self.data.len());
        out.extend_from_slice(FRAME_MAGIC);
        out.push(RECORDING_VERSION);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.at.to_be_bytes());
        out.push(self.kind.code());
        out.push(u8::from(self.redacted));
        out.extend_from_slice(&(self.data.len() as u64).to_be_bytes());
        out.extend_from_slice(&self.data);
        out.extend_from_slice(&self.prev_hash);
        out.extend_from_slice(&self.hash);
        out
    }

    /// Decodes one frame from the front of `bytes`, returning the frame and how many
    /// bytes it consumed.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::FrameInvalid`] when the magic, version, kind or length is
    /// wrong, or the buffer is short.
    pub fn decode(bytes: &[u8]) -> Result<(Frame, usize), BreakGlassError> {
        const FIXED: usize = 4 + 1 + 8 + 8 + 1 + 1 + 8;
        if bytes.len() < FIXED + 64 || &bytes[..4] != FRAME_MAGIC {
            return Err(BreakGlassError::FrameInvalid);
        }
        if bytes[4] != RECORDING_VERSION {
            return Err(BreakGlassError::FrameInvalid);
        }
        let kind = FrameKind::from_code(bytes[21]).ok_or(BreakGlassError::FrameInvalid)?;
        let redacted = match bytes[22] {
            0 => false,
            1 => true,
            _ => return Err(BreakGlassError::FrameInvalid),
        };
        let mut seq = [0u8; 8];
        seq.copy_from_slice(&bytes[5..13]);
        let mut at = [0u8; 8];
        at.copy_from_slice(&bytes[13..21]);
        let mut len = [0u8; 8];
        len.copy_from_slice(&bytes[23..31]);
        let len =
            usize::try_from(u64::from_be_bytes(len)).map_err(|_| BreakGlassError::FrameInvalid)?;
        let end = FIXED
            .checked_add(len)
            .and_then(|n| n.checked_add(64))
            .ok_or(BreakGlassError::FrameInvalid)?;
        if bytes.len() < end {
            return Err(BreakGlassError::FrameInvalid);
        }
        let data = bytes[FIXED..FIXED + len].to_vec();
        let mut prev_hash = [0u8; 32];
        prev_hash.copy_from_slice(&bytes[FIXED + len..FIXED + len + 32]);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[FIXED + len + 32..FIXED + len + 64]);
        Ok((
            Frame {
                seq: u64::from_be_bytes(seq),
                at: i64::from_be_bytes(at),
                kind,
                redacted,
                data,
                prev_hash,
                hash,
            },
            end,
        ))
    }
}

fn frame_hash(
    seq: u64,
    at: i64,
    kind: FrameKind,
    redacted: bool,
    data: &[u8],
    prev_hash: &Hash,
) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    hasher.update(seq.to_be_bytes());
    hasher.update(at.to_be_bytes());
    hasher.update([kind.code()]);
    hasher.update([u8::from(redacted)]);
    hasher.update((data.len() as u64).to_be_bytes());
    hasher.update(data);
    hasher.update(prev_hash);
    hasher.finalize().into()
}

/// The recorder boundary: something that appends frames to a session recording.
///
/// The trait has no method that edits or removes a frame: a recording is append-only.
pub trait Recorder {
    /// Appends one frame, assigning its sequence number and chain link. When `echo` is
    /// off and the kind is redactable, the content is replaced with [`REDACTED`].
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::RecordingFull`] when the recording reached its size cap (the
    /// truncation marker is written first), or [`BreakGlassError::RecordingIo`].
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

    /// Whether the recording reached its size cap and carries the truncation marker.
    fn truncated(&self) -> bool;
}

/// An in-memory recorder: the reference implementation, and the fake the session tests
/// use. It applies the same redaction and size-cap rules as the file recorder.
#[derive(Debug)]
pub struct MemoryRecorder {
    session: String,
    frames: Vec<Frame>,
    prev: Hash,
    max_bytes: u64,
    written: u64,
    truncated: bool,
}

impl MemoryRecorder {
    /// An empty recorder for `session` with the given size cap, in bytes.
    #[must_use]
    pub fn new(session: &str, max_bytes: u64) -> MemoryRecorder {
        MemoryRecorder {
            session: session.to_owned(),
            frames: Vec::new(),
            prev: genesis_hash(session),
            max_bytes,
            written: 0,
            truncated: false,
        }
    }

    /// The frames recorded so far.
    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// The session the recording is about.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }
}

impl Recorder for MemoryRecorder {
    fn append(
        &mut self,
        kind: FrameKind,
        at: i64,
        data: &[u8],
        echo: bool,
    ) -> Result<Frame, BreakGlassError> {
        if self.truncated {
            return Err(BreakGlassError::RecordingFull);
        }
        let seq = self.frames.len() as u64 + 1;
        let (redacted, payload) = if kind.is_redactable() && !echo {
            (true, REDACTED.to_vec())
        } else {
            (false, data.to_vec())
        };
        let frame = Frame::new(seq, at, kind, redacted, payload, self.prev);
        let encoded = frame.to_bytes();
        if self.written + encoded.len() as u64 > self.max_bytes {
            let marker = Frame::truncation(at, seq - 1, self.prev, seq);
            self.prev = marker.hash;
            self.frames.push(marker);
            self.truncated = true;
            return Err(BreakGlassError::RecordingFull);
        }
        self.written += encoded.len() as u64;
        self.prev = frame.hash;
        self.frames.push(frame.clone());
        Ok(frame)
    }

    fn digest(&self) -> Hash {
        self.prev
    }

    fn frame_count(&self) -> u64 {
        self.frames.len() as u64
    }

    fn truncated(&self) -> bool {
        self.truncated
    }
}

/// The manifest of one recording: a small sidecar that names the session, the frame
/// count and the head digest, so the customer's viewer can check the file at a glance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The manifest format version.
    pub version: u32,
    /// The session the recording is about.
    pub session: String,
    /// The tenant the session is about.
    pub tenant: String,
    /// When the recording was created (unix seconds).
    pub created_at: i64,
    /// How many frames the recording holds.
    pub frames: u64,
    /// The head hash, as lower-case hex.
    pub digest: String,
    /// Whether the recording carries the truncation marker.
    pub truncated: bool,
    /// The size cap applied, in bytes.
    pub max_bytes: u64,
}

/// A recording on disk: an append-only frame file plus its manifest.
#[derive(Debug)]
pub struct FileRecorder {
    file: fs::File,
    recording_path: PathBuf,
    manifest_path: PathBuf,
    session: String,
    tenant: String,
    created_at: i64,
    prev: Hash,
    seq: u64,
    written: u64,
    max_bytes: u64,
    truncated: bool,
}

impl FileRecorder {
    /// The recording file path for `session` in `dir`.
    #[must_use]
    pub fn recording_path(dir: &Path, session: &str) -> PathBuf {
        dir.join(format!("{session}.bgr"))
    }

    /// The manifest file path for `session` in `dir`.
    #[must_use]
    pub fn manifest_path(dir: &Path, session: &str) -> PathBuf {
        dir.join(format!("{session}.manifest.json"))
    }

    /// Creates an append-only recording in `dir`, refusing to clobber an existing one.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::RecordingIo`] when the directory or file cannot be created.
    pub fn create(
        dir: &Path,
        session: &str,
        tenant: &str,
        at: i64,
        max_bytes: u64,
    ) -> Result<FileRecorder, BreakGlassError> {
        // The session id becomes a file name: refuse anything that could leave `dir`.
        if session.is_empty()
            || session.starts_with('.')
            || !session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(BreakGlassError::RecordingIo);
        }
        fs::create_dir_all(dir).map_err(|_| BreakGlassError::RecordingIo)?;
        let recording_path = Self::recording_path(dir, session);
        let manifest_path = Self::manifest_path(dir, session);
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&recording_path)
            .map_err(|_| BreakGlassError::RecordingIo)?;
        let header = encode_header(session, tenant);
        file.write_all(&header)
            .map_err(|_| BreakGlassError::RecordingIo)?;
        let recorder = FileRecorder {
            file,
            recording_path,
            manifest_path,
            session: session.to_owned(),
            tenant: tenant.to_owned(),
            created_at: at,
            prev: genesis_hash(session),
            seq: 0,
            written: header.len() as u64,
            max_bytes,
            truncated: false,
        };
        recorder.write_manifest()?;
        Ok(recorder)
    }

    /// The session the recording is about.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The recording file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.recording_path
    }

    /// Writes the manifest from the current head, atomically (temp file then rename).
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::RecordingIo`] when the manifest cannot be written.
    pub fn write_manifest(&self) -> Result<(), BreakGlassError> {
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            session: self.session.clone(),
            tenant: self.tenant.clone(),
            created_at: self.created_at,
            frames: self.seq,
            digest: hex(&self.prev),
            truncated: self.truncated,
            max_bytes: self.max_bytes,
        };
        let text =
            serde_json::to_vec_pretty(&manifest).map_err(|_| BreakGlassError::ManifestInvalid)?;
        let temp = self.manifest_path.with_extension("json.tmp");
        fs::write(&temp, &text).map_err(|_| BreakGlassError::RecordingIo)?;
        fs::rename(&temp, &self.manifest_path).map_err(|_| BreakGlassError::RecordingIo)
    }

    /// Flushes the recording and writes the final manifest, returning the head digest.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::RecordingIo`] on a write failure.
    pub fn finish(&mut self) -> Result<Hash, BreakGlassError> {
        self.file
            .flush()
            .map_err(|_| BreakGlassError::RecordingIo)?;
        self.write_manifest()?;
        Ok(self.prev)
    }

    fn append_inner(
        &mut self,
        kind: FrameKind,
        at: i64,
        data: &[u8],
        echo: bool,
    ) -> Result<Frame, BreakGlassError> {
        if self.truncated {
            return Err(BreakGlassError::RecordingFull);
        }
        let seq = self.seq + 1;
        let (redacted, payload) = if kind.is_redactable() && !echo {
            (true, REDACTED.to_vec())
        } else {
            (false, data.to_vec())
        };
        let frame = Frame::new(seq, at, kind, redacted, payload, self.prev);
        let encoded = frame.to_bytes();
        if self.written + encoded.len() as u64 > self.max_bytes {
            let marker = Frame::truncation(at, self.seq, self.prev, seq);
            self.file
                .write_all(&marker.to_bytes())
                .map_err(|_| BreakGlassError::RecordingIo)?;
            self.prev = marker.hash;
            self.seq = seq;
            self.written += marker.to_bytes().len() as u64;
            self.truncated = true;
            self.write_manifest()?;
            return Err(BreakGlassError::RecordingFull);
        }
        self.file
            .write_all(&encoded)
            .map_err(|_| BreakGlassError::RecordingIo)?;
        self.written += encoded.len() as u64;
        self.prev = frame.hash;
        self.seq = seq;
        self.write_manifest()?;
        Ok(frame)
    }
}

impl Recorder for FileRecorder {
    fn append(
        &mut self,
        kind: FrameKind,
        at: i64,
        data: &[u8],
        echo: bool,
    ) -> Result<Frame, BreakGlassError> {
        self.append_inner(kind, at, data, echo)
    }

    fn digest(&self) -> Hash {
        self.prev
    }

    fn frame_count(&self) -> u64 {
        self.seq
    }

    fn truncated(&self) -> bool {
        self.truncated
    }
}

fn encode_header(session: &str, tenant: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(13 + session.len() + tenant.len());
    out.extend_from_slice(FRAME_MAGIC);
    out.push(RECORDING_VERSION);
    out.extend_from_slice(&(session.len() as u32).to_be_bytes());
    out.extend_from_slice(session.as_bytes());
    out.extend_from_slice(&(tenant.len() as u32).to_be_bytes());
    out.extend_from_slice(tenant.as_bytes());
    out
}

fn decode_header(bytes: &[u8]) -> Result<(String, String, usize), BreakGlassError> {
    if bytes.len() < 13 || &bytes[..4] != FRAME_MAGIC || bytes[4] != RECORDING_VERSION {
        return Err(BreakGlassError::FrameInvalid);
    }
    let session_len = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    let mut pos: usize = 9;
    let session_end = pos
        .checked_add(session_len)
        .ok_or(BreakGlassError::FrameInvalid)?;
    if bytes.len() < session_end + 4 {
        return Err(BreakGlassError::FrameInvalid);
    }
    let session = String::from_utf8(bytes[pos..session_end].to_vec())
        .map_err(|_| BreakGlassError::FrameInvalid)?;
    pos = session_end;
    let tenant_len =
        u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
    pos += 4;
    let tenant_end = pos
        .checked_add(tenant_len)
        .ok_or(BreakGlassError::FrameInvalid)?;
    if bytes.len() < tenant_end {
        return Err(BreakGlassError::FrameInvalid);
    }
    let tenant = String::from_utf8(bytes[pos..tenant_end].to_vec())
        .map_err(|_| BreakGlassError::FrameInvalid)?;
    Ok((session, tenant, tenant_end))
}

/// The customer-side replay of a recording: an iterator of frames that verifies the hash
/// chain as it goes.
///
/// A verified recording yields every frame. A tampered one yields the frames up to the
/// first bad link and then one [`BreakGlassError::RecordingTampered`] item; a file that
/// cannot be read yields one [`BreakGlassError::RecordingIo`] item. [`Replay::verified`]
/// tells the caller which happened.
#[derive(Debug)]
pub struct Replay {
    frames: Vec<Frame>,
    error: Option<BreakGlassError>,
    index: usize,
    digest: Hash,
    truncated: bool,
}

impl Replay {
    /// Whether the whole recording verified.
    #[must_use]
    pub fn verified(&self) -> bool {
        self.error.is_none()
    }

    /// The head digest of the verified part.
    #[must_use]
    pub fn digest(&self) -> Hash {
        self.digest
    }

    /// Whether the recording carries the truncation marker.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// The verified frames.
    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }
}

impl Iterator for Replay {
    type Item = Result<Frame, BreakGlassError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.frames.len() {
            let frame = self.frames[self.index].clone();
            self.index += 1;
            return Some(Ok(frame));
        }
        if self.index == self.frames.len() {
            self.index += 1;
            if let Some(error) = self.error {
                return Some(Err(error));
            }
        }
        None
    }
}

/// Replays a recording from `path`, verifying its hash chain.
///
/// The returned iterator is lazy in spirit but reads the file up front; see [`Replay`].
#[must_use]
pub fn replay(path: &Path) -> Replay {
    match fs::read(path) {
        Ok(bytes) => replay_bytes(&bytes),
        Err(_) => Replay {
            frames: Vec::new(),
            error: Some(BreakGlassError::RecordingNotFound),
            index: 0,
            digest: [0u8; 32],
            truncated: false,
        },
    }
}

/// Replays a recording from its bytes, verifying the hash chain.
#[must_use]
pub fn replay_bytes(bytes: &[u8]) -> Replay {
    let (session, _tenant, mut pos) = match decode_header(bytes) {
        Ok(header) => header,
        Err(error) => {
            return Replay {
                frames: Vec::new(),
                error: Some(error),
                index: 0,
                digest: [0u8; 32],
                truncated: false,
            };
        }
    };
    let mut prev = genesis_hash(&session);
    let mut frames = Vec::new();
    let mut truncated = false;
    while pos < bytes.len() {
        let (frame, consumed) = match Frame::decode(&bytes[pos..]) {
            Ok(decoded) => decoded,
            Err(error) => {
                return Replay {
                    frames,
                    error: Some(error),
                    index: 0,
                    digest: prev,
                    truncated,
                };
            }
        };
        if frame.seq != frames.len() as u64 + 1
            || frame.prev_hash != prev
            || frame.recompute_hash() != frame.hash
        {
            return Replay {
                frames,
                error: Some(BreakGlassError::RecordingTampered { frame: frame.seq }),
                index: 0,
                digest: prev,
                truncated,
            };
        }
        if frame.kind == FrameKind::Truncation {
            truncated = true;
        }
        prev = frame.hash;
        frames.push(frame);
        pos += consumed;
    }
    Replay {
        frames,
        error: None,
        index: 0,
        digest: prev,
        truncated,
    }
}

/// Reads and parses a manifest.
///
/// # Errors
///
/// [`BreakGlassError::RecordingNotFound`] when the file is missing;
/// [`BreakGlassError::ManifestInvalid`] when it does not parse.
pub fn read_manifest(path: &Path) -> Result<Manifest, BreakGlassError> {
    let bytes = fs::read(path).map_err(|_| BreakGlassError::RecordingNotFound)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| BreakGlassError::ManifestInvalid)?;
    if manifest.version != MANIFEST_VERSION {
        return Err(BreakGlassError::ManifestInvalid);
    }
    Ok(manifest)
}

/// Verifies a recording against its manifest: the hash chain, the frame count, the head
/// digest and the truncation flag must all agree.
///
/// # Errors
///
/// [`BreakGlassError::RecordingTampered`] when the chain does not verify;
/// [`BreakGlassError::ManifestInvalid`] when the manifest disagrees with the recording.
pub fn verify_recording(
    recording_path: &Path,
    manifest_path: &Path,
) -> Result<Hash, BreakGlassError> {
    let manifest = read_manifest(manifest_path)?;
    let replay = replay(recording_path);
    if let Some(error) = replay.error {
        return Err(error);
    }
    if manifest.frames != replay.frames.len() as u64
        || manifest.digest != hex(&replay.digest)
        || manifest.truncated != replay.truncated
    {
        return Err(BreakGlassError::ManifestInvalid);
    }
    Ok(replay.digest)
}
