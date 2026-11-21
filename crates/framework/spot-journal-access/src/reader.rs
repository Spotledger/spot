//! The manifest-gated reader: the three operations of the access surface.
//!
//! One [`JournalReader`] is built **per site** with that site's tenant id and store, so it can
//! only name keys of its own tenant. Names never come from the caller: the manifest is read from
//! the store and verified against the tenant's public keys, and a segment is served only when
//! the request names exactly one segment **listed in that signed manifest**. Every key is built
//! through [`Layout`]'s typed constructors; a request path is only ever parsed as canonical
//! decimal numbers ([`canonical_number`]).
//!
//! Every call writes one [`AccessRecord`], denied and failed calls included. A denial is
//! recorded before the answer; a served manifest or segment is recorded at the end (a segment
//! when its stream finishes, with the bytes sent). If the audit write fails the call is refused
//! (`ReadError::AuditUnavailable`): there is no unaudited read of the tenant.

use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_journal_manifest::{SignedManifest, Verified, VerifyingKeys, verify};
use spot_journal_store::{JournalStore, Key, Layout, etag};

use crate::access::{Access, AuthorizeError, AuthorizedReader, DenyReason};
use crate::error::{AccessError, ReadError};
use crate::limits::{Limits, Permit};
use crate::model::{
    AccessObject, AccessOutcome, AccessRecord, CHUNK_BYTES, Fut, MAX_SEGMENT_BYTES,
};
use crate::range::{RangeRequest, canonical_number, canonical_range, parse_range};
use crate::timefmt::rfc3339;

/// Writes the `sys_journal_access` record. A failure refuses the call.
pub trait AccessAudit: Send + Sync {
    /// Records one access.
    fn record<'a>(&'a self, record: &'a AccessRecord) -> Fut<'a, ()>;
}

/// The wall clock, injected. `None` means it cannot be read, and the call is refused.
pub trait TimeSource: Send + Sync {
    /// Unix milliseconds now.
    fn now_millis(&self) -> Option<i64>;
}

/// What `journal_info_get` reports besides the generations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalMeta {
    /// The durability policy name only.
    pub policy: String,
    /// The journal's region.
    pub region: String,
}

/// One generation in the info answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInfo {
    /// The generation.
    pub generation: u64,
    /// The first sealed position (0 when nothing is sealed yet).
    pub first_seq: u64,
    /// The last sealed position.
    pub last_seq: u64,
    /// Positions up to here are sealed in segments.
    pub sealed_until_seq: u64,
    /// When the generation was opened, Unix milliseconds.
    pub opened_at_millis: i64,
}

/// The info answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoOut {
    /// The generations, ascending.
    pub generations: Vec<GenerationInfo>,
    /// The policy name.
    pub policy: String,
    /// The region.
    pub region: String,
}

impl InfoOut {
    /// The contract's `JournalInfoResponse.data`.
    #[must_use]
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
                    "opened_at": rfc3339(g.opened_at_millis),
                })
            })
            .collect();
        json!({ "generations": generations, "policy": self.policy, "region": self.region })
    }
}

/// How a segment read answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentStatus {
    /// The whole segment (`200`).
    Full,
    /// A byte range (`206`).
    Partial,
    /// A range outside the segment (`416`).
    RangeNotSatisfiable,
}

/// A segment read, ready to stream.
pub struct SegmentRead {
    /// `200`, `206` or `416`.
    pub status: SegmentStatus,
    /// The recorded hash, hex: the `ETag`.
    pub etag: String,
    /// The segment's size.
    pub total_len: u64,
    /// The length of the body that follows.
    pub content_length: u64,
    /// `bytes a-b/total` for a partial answer, `bytes */total` for a refused range.
    pub content_range: Option<String>,
    /// The body; `None` for a refused range.
    pub stream: Option<SegmentStream>,
}

/// The bytes of one segment (already verified against the manifest's recorded hash), streamed in
/// bounded chunks. Call [`SegmentStream::finish`] once the stream ended or was abandoned: it
/// writes the access record.
pub struct SegmentStream {
    data: Vec<u8>,
    start: usize,
    pos: usize,
    end: usize,
    audit: Arc<dyn AccessAudit>,
    record: AccessRecord,
    _permit: Option<Permit>,
}

impl SegmentStream {
    /// The next chunk of at most [`CHUNK_BYTES`] bytes, or `None` at the end of the range.
    pub fn next_chunk(&mut self) -> Option<Vec<u8>> {
        if self.pos >= self.end {
            return None;
        }
        let stop = (self.pos + CHUNK_BYTES).min(self.end);
        let chunk = self.data.get(self.pos..stop)?.to_vec();
        self.pos = stop;
        Some(chunk)
    }

    /// Bytes sent so far.
    #[must_use]
    pub fn sent(&self) -> u64 {
        u64::try_from(self.pos - self.start).unwrap_or(u64::MAX)
    }

    /// Whether the whole range was sent.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.pos >= self.end
    }

    /// Writes the access record: `served` with the bytes sent when the range was sent in full,
    /// `failed` (reason `aborted`) otherwise. Frees the stream slot.
    ///
    /// # Errors
    /// [`ReadError::AuditUnavailable`] when the record cannot be written.
    pub async fn finish(self) -> Result<(), ReadError> {
        let mut record = self.record.clone();
        record.bytes = Some(self.sent());
        if self.complete() {
            record.outcome = AccessOutcome::Served;
        } else {
            record.outcome = AccessOutcome::Failed;
            record.reason = Some("aborted".to_owned());
        }
        self.audit
            .record(&record)
            .await
            .map_err(|_| ReadError::AuditUnavailable)
    }
}

/// The reader of one tenant's journal.
pub struct JournalReader {
    tenant: String,
    store: Arc<dyn JournalStore>,
    keys: Arc<VerifyingKeys>,
    access: Arc<Access>,
    audit: Arc<dyn AccessAudit>,
    limits: Arc<Limits>,
    clock: Arc<dyn TimeSource>,
    meta: JournalMeta,
}

/// Everything a reader is built from.
pub struct ReaderParts {
    /// The site's tenant id (one safe path component).
    pub tenant: String,
    /// The site's journal store.
    pub store: Arc<dyn JournalStore>,
    /// The tenant's public keys, with their generation windows.
    pub keys: Arc<VerifyingKeys>,
    /// The grant model.
    pub access: Arc<Access>,
    /// The access audit.
    pub audit: Arc<dyn AccessAudit>,
    /// The concurrency and budget limits.
    pub limits: Arc<Limits>,
    /// The clock.
    pub clock: Arc<dyn TimeSource>,
    /// The policy and region names.
    pub meta: JournalMeta,
}

impl JournalReader {
    /// A reader over `parts`.
    ///
    /// # Errors
    /// [`AccessError::SettingsInvalid`] when the tenant id is not a safe key component.
    pub fn new(parts: ReaderParts) -> Result<Self, AccessError> {
        Layout::manifest(&parts.tenant, 1).map_err(|_| {
            AccessError::SettingsInvalid("the tenant id is not a safe key component")
        })?;
        Ok(Self {
            tenant: parts.tenant,
            store: parts.store,
            keys: parts.keys,
            access: parts.access,
            audit: parts.audit,
            limits: parts.limits,
            clock: parts.clock,
            meta: parts.meta,
        })
    }

    /// The limits (for the host's metric).
    #[must_use]
    pub fn limits(&self) -> &Arc<Limits> {
        &self.limits
    }

    async fn write(&self, record: &AccessRecord) -> Result<(), ReadError> {
        self.audit
            .record(record)
            .await
            .map_err(|_| ReadError::AuditUnavailable)
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        principal: &str,
        grant: String,
        object: AccessObject,
        generation: u64,
        range: Option<(u64, u64)>,
        outcome: AccessOutcome,
        reason: Option<&str>,
    ) -> AccessRecord {
        AccessRecord {
            principal: principal.to_owned(),
            grant,
            object,
            generation,
            first_seq: range.map(|r| r.0),
            last_seq: range.map(|r| r.1),
            outcome,
            reason: reason.map(str::to_owned),
            bytes: None,
        }
    }

    /// Authorizes the caller, auditing a denial before it is returned.
    async fn gate(
        &self,
        principal: &str,
        object: AccessObject,
        generation: u64,
        range: Option<(u64, u64)>,
    ) -> Result<(AuthorizedReader, i64), ReadError> {
        let Some(now) = self.clock.now_millis() else {
            let record = Self::record(
                principal,
                "none".to_owned(),
                object,
                generation,
                range,
                AccessOutcome::Denied,
                Some(DenyReason::ClockUnreadable.code()),
            );
            self.write(&record).await?;
            return Err(ReadError::Denied { expired: false });
        };
        match self.access.authorize(principal, now).await {
            Ok(reader) => Ok((reader, now)),
            Err(AuthorizeError::Denied(reason)) => {
                let outcome = if reason == DenyReason::Expired {
                    AccessOutcome::Expired
                } else {
                    AccessOutcome::Denied
                };
                let record = Self::record(
                    principal,
                    "none".to_owned(),
                    object,
                    generation,
                    range,
                    outcome,
                    Some(reason.code()),
                );
                self.write(&record).await?;
                Err(ReadError::Denied {
                    expired: reason == DenyReason::Expired,
                })
            }
            Err(AuthorizeError::Unavailable) => {
                // Fail closed. The audit is best effort here: the answer is a refusal anyway.
                let record = Self::record(
                    principal,
                    "none".to_owned(),
                    object,
                    generation,
                    range,
                    AccessOutcome::Failed,
                    Some("authorization_unavailable"),
                );
                let _ = self.audit.record(&record).await;
                Err(ReadError::Unavailable)
            }
        }
    }

    async fn not_found(
        &self,
        reader: &AuthorizedReader,
        object: AccessObject,
        generation: u64,
        range: Option<(u64, u64)>,
        reason: &str,
    ) -> ReadError {
        let record = Self::record(
            &reader.principal,
            reader.grant.audit_text(),
            object,
            generation,
            range,
            AccessOutcome::NotFound,
            Some(reason),
        );
        match self.write(&record).await {
            Ok(()) => ReadError::NotFound,
            Err(error) => error,
        }
    }

    async fn failed(
        &self,
        reader: &AuthorizedReader,
        object: AccessObject,
        generation: u64,
        range: Option<(u64, u64)>,
        reason: &str,
    ) -> ReadError {
        let record = Self::record(
            &reader.principal,
            reader.grant.audit_text(),
            object,
            generation,
            range,
            AccessOutcome::Failed,
            Some(reason),
        );
        // The caller gets "unavailable" whether or not the failure record could be written.
        let _ = self.audit.record(&record).await;
        ReadError::Unavailable
    }

    /// Reads and verifies one generation's manifest. `Ok(None)` when there is none.
    async fn load_manifest(
        &self,
        generation: u64,
    ) -> Result<Option<(Vec<u8>, SignedManifest, Verified)>, &'static str> {
        let key = Layout::manifest(&self.tenant, generation)
            .map_err(|_| "key_invalid")?
            .to_key();
        let Some(object) = self.store.get(&key).await.map_err(|_| "store_failed")? else {
            return Ok(None);
        };
        let signed = SignedManifest::from_bytes(&object.bytes).map_err(|_| "manifest_invalid")?;
        let verified = verify(&signed, &self.keys).map_err(|_| "manifest_unverified")?;
        if verified.generation() != generation || verified.source() != self.tenant {
            return Err("manifest_mismatch");
        }
        Ok(Some((object.bytes, signed, verified)))
    }

    /// `journal_info_get`: the generations and the policy.
    ///
    /// # Errors
    /// [`ReadError::Denied`], [`ReadError::AuditUnavailable`] or [`ReadError::Unavailable`].
    pub async fn info(&self, principal: &str) -> Result<InfoOut, ReadError> {
        let (reader, _now) = self
            .gate(principal, AccessObject::Manifest, 0, None)
            .await?;
        let prefix = format!("{}/manifest/", self.tenant);
        let entries = match self.store.list(&prefix, None, 1000).await {
            Ok(entries) => entries,
            Err(_) => {
                return Err(self
                    .failed(&reader, AccessObject::Manifest, 0, None, "store_failed")
                    .await);
            }
        };
        let mut generations = Vec::new();
        for entry in entries {
            let Ok(Key::Manifest(found)) = Layout::parse(&entry.key) else {
                continue;
            };
            let generation = found.generation;
            let (_bytes, _signed, verified) = match self.load_manifest(generation).await {
                Ok(Some(loaded)) => loaded,
                Ok(None) => continue,
                Err(reason) => {
                    return Err(self
                        .failed(&reader, AccessObject::Manifest, generation, None, reason)
                        .await);
                }
            };
            let segments = verified.segments();
            let first_seq = segments.first().map_or(0, |s| s.first_seq);
            let last_seq = verified.last_seq();
            let opened = verified
                .forks()
                .iter()
                .find(|f| f.generation == generation)
                .map_or_else(|| verified.issued_at_millis(), |f| f.at_millis);
            generations.push(GenerationInfo {
                generation,
                first_seq,
                last_seq,
                sealed_until_seq: last_seq,
                opened_at_millis: opened,
            });
        }
        generations.sort_by_key(|g| g.generation);
        let mut record = Self::record(
            &reader.principal,
            reader.grant.audit_text(),
            AccessObject::Manifest,
            0,
            None,
            AccessOutcome::Served,
            None,
        );
        record.bytes = Some(0);
        self.write(&record).await?;
        Ok(InfoOut {
            generations,
            policy: self.meta.policy.clone(),
            region: self.meta.region.clone(),
        })
    }

    /// `journal_manifest_get`: the signed manifest of generation `generation_text`.
    ///
    /// # Errors
    /// [`ReadError::Denied`], [`ReadError::NotFound`] (also for a non-canonical number),
    /// [`ReadError::AuditUnavailable`] or [`ReadError::Unavailable`].
    pub async fn manifest(
        &self,
        principal: &str,
        generation_text: &str,
    ) -> Result<Json, ReadError> {
        let parsed = canonical_number(generation_text).filter(|g| *g >= 1);
        let (reader, _now) = self
            .gate(principal, AccessObject::Manifest, parsed.unwrap_or(0), None)
            .await?;
        let Some(generation) = parsed else {
            return Err(self
                .not_found(&reader, AccessObject::Manifest, 0, None, "not_canonical")
                .await);
        };
        let (bytes, signed, verified) = match self.load_manifest(generation).await {
            Ok(Some(loaded)) => loaded,
            Ok(None) => {
                return Err(self
                    .not_found(
                        &reader,
                        AccessObject::Manifest,
                        generation,
                        None,
                        "no_such_generation",
                    )
                    .await);
            }
            Err(reason) => {
                return Err(self
                    .failed(&reader, AccessObject::Manifest, generation, None, reason)
                    .await);
            }
        };
        let mut record = Self::record(
            &reader.principal,
            reader.grant.audit_text(),
            AccessObject::Manifest,
            generation,
            None,
            AccessOutcome::Served,
            None,
        );
        record.bytes = Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.write(&record).await?;
        Ok(manifest_json(&signed, &verified, &bytes))
    }

    /// `journal_segment_get`: one sealed segment listed in the signed manifest, whole or by
    /// range.
    ///
    /// # Errors
    /// [`ReadError::Denied`], [`ReadError::NotFound`], [`ReadError::RateLimited`],
    /// [`ReadError::AuditUnavailable`] or [`ReadError::Unavailable`].
    pub async fn segment(
        &self,
        principal: &str,
        generation_text: &str,
        range_text: &str,
        range_header: Option<&str>,
    ) -> Result<SegmentRead, ReadError> {
        let generation = canonical_number(generation_text).filter(|g| *g >= 1);
        let range = canonical_range(range_text).filter(|(first, last)| first <= last);
        let (reader, now) = self
            .gate(
                principal,
                AccessObject::Segment,
                generation.unwrap_or(0),
                range,
            )
            .await?;
        let (Some(generation), Some((first, last))) = (generation, range) else {
            return Err(self
                .not_found(
                    &reader,
                    AccessObject::Segment,
                    generation.unwrap_or(0),
                    range,
                    "not_canonical",
                )
                .await);
        };
        let loaded = match self.load_manifest(generation).await {
            Ok(Some(loaded)) => loaded,
            Ok(None) => {
                return Err(self
                    .not_found(
                        &reader,
                        AccessObject::Segment,
                        generation,
                        range,
                        "no_such_generation",
                    )
                    .await);
            }
            Err(reason) => {
                return Err(self
                    .failed(&reader, AccessObject::Segment, generation, range, reason)
                    .await);
            }
        };
        let (_bytes, _signed, verified) = loaded;
        // Only a segment the signed manifest lists, exactly: never a span, never a tail chunk.
        let Some(summary) = verified
            .segments()
            .iter()
            .find(|s| s.generation == generation && s.first_seq == first && s.last_seq == last)
            .cloned()
        else {
            return Err(self
                .not_found(
                    &reader,
                    AccessObject::Segment,
                    generation,
                    range,
                    "not_listed",
                )
                .await);
        };
        let Ok(key) = Layout::segment(&self.tenant, generation, first, last) else {
            return Err(self
                .not_found(
                    &reader,
                    AccessObject::Segment,
                    generation,
                    range,
                    "not_canonical",
                )
                .await);
        };
        let key = key.to_key();
        let recorded = hex(&summary.segment_hash);
        let object = match self.store.get(&key).await {
            Ok(Some(object)) => object,
            Ok(None) => {
                return Err(self
                    .failed(
                        &reader,
                        AccessObject::Segment,
                        generation,
                        range,
                        "segment_missing",
                    )
                    .await);
            }
            Err(_) => {
                return Err(self
                    .failed(
                        &reader,
                        AccessObject::Segment,
                        generation,
                        range,
                        "store_failed",
                    )
                    .await);
            }
        };
        let total = u64::try_from(object.bytes.len()).unwrap_or(u64::MAX);
        if total > MAX_SEGMENT_BYTES {
            return Err(self
                .failed(
                    &reader,
                    AccessObject::Segment,
                    generation,
                    range,
                    "segment_too_large",
                )
                .await);
        }
        // Verify the whole segment against the signed manifest before the first byte goes out:
        // a corrupted segment is never served as a 200 that looks complete.
        if etag(&object.bytes) != recorded {
            return Err(self
                .failed(
                    &reader,
                    AccessObject::Segment,
                    generation,
                    range,
                    "hash_mismatch",
                )
                .await);
        }
        let mut base = Self::record(
            &reader.principal,
            reader.grant.audit_text(),
            AccessObject::Segment,
            generation,
            range,
            AccessOutcome::Served,
            None,
        );
        let (status, start, end) = match parse_range(range_header, total) {
            RangeRequest::Full => (SegmentStatus::Full, 0, total),
            RangeRequest::Partial { start, end } => (SegmentStatus::Partial, start, end + 1),
            RangeRequest::Unsatisfiable => {
                base.outcome = AccessOutcome::Failed;
                base.reason = Some("range_unsatisfiable".to_owned());
                base.bytes = Some(0);
                self.write(&base).await?;
                return Ok(SegmentRead {
                    status: SegmentStatus::RangeNotSatisfiable,
                    etag: recorded,
                    total_len: total,
                    content_length: 0,
                    content_range: Some(format!("bytes */{total}")),
                    stream: None,
                });
            }
        };
        let length = end - start;
        let permit = match self.limits.acquire(&reader.principal, length, now) {
            Ok(permit) => permit,
            Err(over) => {
                base.outcome = AccessOutcome::Denied;
                base.reason = Some("rate_limited".to_owned());
                self.write(&base).await?;
                return Err(ReadError::RateLimited {
                    retry_after_secs: over.retry_after_secs,
                });
            }
        };
        let content_range = (status == SegmentStatus::Partial)
            .then(|| format!("bytes {start}-{}/{total}", end - 1));
        let start_at = usize::try_from(start).map_err(|_| ReadError::Unavailable)?;
        let end_at = usize::try_from(end).map_err(|_| ReadError::Unavailable)?;
        Ok(SegmentRead {
            status,
            etag: recorded,
            total_len: total,
            content_length: length,
            content_range,
            stream: Some(SegmentStream {
                data: object.bytes,
                start: start_at,
                pos: start_at,
                end: end_at,
                audit: Arc::clone(&self.audit),
                record: base,
                _permit: Some(permit),
            }),
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// The signed manifest as JSON: a readable summary and `signed_bytes`, the stored byte form in
/// hex, which is what a verifier checks (`SignedManifest::from_bytes`).
fn manifest_json(signed: &SignedManifest, verified: &Verified, stored: &[u8]) -> Json {
    let segments: Vec<Json> = verified
        .segments()
        .iter()
        .map(|s| {
            json!({
                "generation": s.generation,
                "first_seq": s.first_seq,
                "last_seq": s.last_seq,
                "entry_count": s.entry_count,
                "segment_hash": hex(&s.segment_hash),
                "previous_hash": hex(&s.previous_hash),
            })
        })
        .collect();
    let forks: Vec<Json> = verified
        .forks()
        .iter()
        .map(|f| {
            json!({
                "generation": f.generation,
                "forked_from_position": f.forked_from_position,
                "reason": f.reason,
                "at": rfc3339(f.at_millis),
            })
        })
        .collect();
    json!({
        "format": "spot.journal.manifest.v1",
        "source": verified.source(),
        "generation": verified.generation(),
        "issued_at": rfc3339(verified.issued_at_millis()),
        "key_id": signed.key_id,
        "segments": segments,
        "forks": forks,
        "signature": hex(&signed.signature),
        "signed_bytes": hex(stored),
    })
}
