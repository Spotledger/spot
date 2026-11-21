//! The journal access seam of `journal_info_get`, `journal_manifest_get` and
//! `journal_segment_get` (contract 0.16.0, WP-JRN-09.T4, ADR-061 section 7 surface 2).
//!
//! `spot-http` does not depend on the journal crates. It defines [`JournalAccessPort`], the three
//! reads the routes need; the host writes the adapter over `spot-journal-access`. The port
//! answers for the caller its credential names: it decides who the caller is, whether a
//! `journal_reader` grant (or the administrator role) allows the read, what is named by the
//! manifest, the limits and the audit record. The routes decide none of it, and the path
//! numbers reach the port as the raw text of the URL so that even a hostile path is audited.
//! [`MemoryJournalAccess`] is the in-memory fake the tests use.

use std::sync::Mutex;

use axum::body::Bytes;
use serde_json::{Value as Json, json};

use super::PortKey;
use crate::api::files::{BoxByteStream, ChunkStream};
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`JournalAccessPort`].
pub const KEY: PortKey = PortKey::of::<dyn JournalAccessPort>();

/// Why a read was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalAccessError {
    /// The credential is unknown or ended: `401 auth.session_expired`.
    Unauthenticated,
    /// No usable grant: one `403 permission.denied`. `expired` adds
    /// `details.reason = grant_expired`, the only distinction the wire makes.
    Denied {
        /// Whether the caller's grant ran out.
        expired: bool,
    },
    /// The generation, segment or range is not a sealed segment of the signed manifest, or the
    /// path is not a canonical name: `404 journal.range_unavailable`.
    NotFound,
    /// Over the concurrency limit or the daily byte budget: `429 api.rate_limited`.
    RateLimited {
        /// Seconds for `Retry-After`.
        retry_after_secs: u64,
    },
    /// The access could not be audited, or the journal could not be read: `503
    /// server.unavailable`.
    Unavailable,
}

/// One generation of `journal_info_get`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalGeneration {
    /// The generation, at least 1.
    pub generation: u64,
    /// The first sealed position.
    pub first_seq: u64,
    /// The last sealed position.
    pub last_seq: u64,
    /// Positions up to here are sealed.
    pub sealed_until_seq: u64,
    /// When the generation was opened: a UTC RFC 3339 instant ending in `Z`.
    pub opened_at: String,
}

/// The data of `journal_info_get`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    /// The generations, ascending.
    pub generations: Vec<JournalGeneration>,
    /// The durability policy name.
    pub policy: String,
    /// The journal's region.
    pub region: String,
}

impl JournalInfo {
    /// The contract's `JournalInfoResponse.data`.
    pub fn to_json(&self) -> Json {
        let generations: Vec<Json> = self
            .generations
            .iter()
            .map(|g| {
                json!({
                    "generation": g.generation,
                    "first_seq": g.first_seq,
                    "last_seq": g.last_seq,
                    "sealed_until_seq": g.sealed_until_seq,
                    "opened_at": g.opened_at,
                })
            })
            .collect();
        json!({ "generations": generations, "policy": self.policy, "region": self.region })
    }
}

/// How a segment read answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentStatus {
    /// `200`: the whole segment.
    Full,
    /// `206`: a byte range.
    Partial,
    /// `416`: the range is outside the segment.
    RangeNotSatisfiable,
}

/// A segment read.
pub struct SegmentDownload {
    /// The status.
    pub status: SegmentStatus,
    /// The segment's recorded hash (the `ETag`).
    pub etag: String,
    /// The length of `body`.
    pub content_length: u64,
    /// `bytes a-b/total` (206) or `bytes */total` (416).
    pub content_range: Option<String>,
    /// The body. A failure mid-stream aborts the response (the head is sent); the port records
    /// the access when the stream ends or is dropped.
    pub body: Option<BoxByteStream>,
}

/// The three reads of the journal access surface.
pub trait JournalAccessPort: Send + Sync + 'static {
    /// `journal_info_get`.
    fn info<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<JournalInfo, JournalAccessError>>;

    /// `journal_manifest_get`: the signed manifest of the generation named by the raw path text.
    fn manifest<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        generation: &'a str,
    ) -> BoxFuture<'a, Result<Json, JournalAccessError>>;

    /// `journal_segment_get`: the segment named by the raw path texts (`range` is the
    /// `{first_seq}-{last_seq}` part), with the raw `Range` header.
    fn segment<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        generation: &'a str,
        range: &'a str,
        range_header: Option<&'a str>,
    ) -> BoxFuture<'a, Result<SegmentDownload, JournalAccessError>>;
}

/// One call the fake saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeenCall {
    /// `info`, `manifest` or `segment`.
    pub operation: &'static str,
    /// The raw path texts, as the route passed them.
    pub path: Vec<String>,
    /// The raw `Range` header.
    pub range: Option<String>,
}

/// An in-memory [`JournalAccessPort`] for tests: a token table decides the answer.
#[derive(Default)]
pub struct MemoryJournalAccess {
    inner: Mutex<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    /// token to refusal (a token with none is allowed).
    refusals: Vec<(String, JournalAccessError)>,
    allowed: Vec<String>,
    info: Option<JournalInfo>,
    manifest: Option<(String, Json)>,
    segment: Option<(String, String, Vec<u8>)>,
    seen: Vec<SeenCall>,
}

impl MemoryJournalAccess {
    /// An empty fake: nobody is allowed.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Allows `token`.
    pub fn allow(&self, token: &str) {
        self.lock().allowed.push(token.to_owned());
    }

    /// Refuses `token` with `error` (checked before the allow list).
    pub fn refuse(&self, token: &str, error: JournalAccessError) {
        self.lock().refusals.push((token.to_owned(), error));
    }

    /// The info answer.
    pub fn set_info(&self, info: JournalInfo) {
        self.lock().info = Some(info);
    }

    /// The manifest of `generation` (its text).
    pub fn set_manifest(&self, generation: &str, manifest: Json) {
        self.lock().manifest = Some((generation.to_owned(), manifest));
    }

    /// The segment at `generation` / `range` (texts) with its bytes.
    pub fn set_segment(&self, generation: &str, range: &str, bytes: Vec<u8>) {
        self.lock().segment = Some((generation.to_owned(), range.to_owned(), bytes));
    }

    /// The calls seen, oldest first.
    pub fn seen(&self) -> Vec<SeenCall> {
        self.lock().seen.clone()
    }

    fn check(&self, ctx: &RequestCtx) -> Result<(), JournalAccessError> {
        let token = ctx
            .credential
            .as_ref()
            .map(|c| c.token.clone())
            .unwrap_or_default();
        let inner = self.lock();
        if let Some((_, error)) = inner.refusals.iter().find(|(t, _)| *t == token) {
            return Err(*error);
        }
        if inner.allowed.contains(&token) {
            Ok(())
        } else {
            Err(JournalAccessError::Denied { expired: false })
        }
    }
}

impl JournalAccessPort for MemoryJournalAccess {
    fn info<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<JournalInfo, JournalAccessError>> {
        Box::pin(async move {
            self.lock().seen.push(SeenCall {
                operation: "info",
                path: Vec::new(),
                range: None,
            });
            self.check(ctx)?;
            self.lock()
                .info
                .clone()
                .ok_or(JournalAccessError::Unavailable)
        })
    }

    fn manifest<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        generation: &'a str,
    ) -> BoxFuture<'a, Result<Json, JournalAccessError>> {
        Box::pin(async move {
            self.lock().seen.push(SeenCall {
                operation: "manifest",
                path: vec![generation.to_owned()],
                range: None,
            });
            self.check(ctx)?;
            match &self.lock().manifest {
                Some((g, manifest)) if g == generation => Ok(manifest.clone()),
                _ => Err(JournalAccessError::NotFound),
            }
        })
    }

    fn segment<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        generation: &'a str,
        range: &'a str,
        range_header: Option<&'a str>,
    ) -> BoxFuture<'a, Result<SegmentDownload, JournalAccessError>> {
        Box::pin(async move {
            self.lock().seen.push(SeenCall {
                operation: "segment",
                path: vec![generation.to_owned(), range.to_owned()],
                range: range_header.map(str::to_owned),
            });
            self.check(ctx)?;
            let (g, r, bytes) = self
                .lock()
                .segment
                .clone()
                .ok_or(JournalAccessError::NotFound)?;
            if g != generation || r != range {
                return Err(JournalAccessError::NotFound);
            }
            let total = bytes.len() as u64;
            // The fake honours `bytes=a-b` only; the real port's ranges are tested in its crate.
            let wanted = range_header
                .and_then(|h| h.strip_prefix("bytes="))
                .and_then(|spec| spec.split_once('-'))
                .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
            let (status, slice, content_range) = match wanted {
                Some((a, b)) if a <= b && a < total => {
                    let end = b.min(total - 1);
                    (
                        SegmentStatus::Partial,
                        bytes[a as usize..=end as usize].to_vec(),
                        Some(format!("bytes {a}-{end}/{total}")),
                    )
                }
                Some(_) => {
                    return Ok(SegmentDownload {
                        status: SegmentStatus::RangeNotSatisfiable,
                        etag: "fake".to_owned(),
                        content_length: 0,
                        content_range: Some(format!("bytes */{total}")),
                        body: None,
                    });
                }
                None => (SegmentStatus::Full, bytes, None),
            };
            Ok(SegmentDownload {
                status,
                etag: "fake".to_owned(),
                content_length: slice.len() as u64,
                content_range,
                body: Some(Box::pin(ChunkStream::once(Bytes::from(slice)))),
            })
        })
    }
}
