//! The bootstrap session: plan once, serve chunk by chunk, resume from a cursor.

use crate::error::BootstrapError;
use crate::hash::Fnv;
use crate::plan::{self, Plan, image_bytes};
use crate::source::{Seams, SnapshotSource};
use crate::start::{HubHead, NodeState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use spot_sync_closure::{Limits, RecordKey};
use std::collections::{BTreeMap, BTreeSet};

/// The cursor's format version.
const CURSOR_VERSION: u32 = 1;

/// How the caller starts a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StartOptions {
    /// Accept a snapshot no rebuild test has verified. Off by default: a node is never seeded from
    /// something nobody checked unless the caller decides so, explicitly.
    pub allow_untrusted: bool,
}

/// How much one chunk may carry. Never above the session's limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Most image bytes in the chunk.
    pub max_bytes: usize,
    /// Most records in the chunk.
    pub max_records: usize,
}

impl Budget {
    /// The most the limits allow.
    #[must_use]
    pub fn of(limits: &Limits) -> Self {
        Self {
            max_bytes: limits.max_bytes,
            max_records: limits.max_records,
        }
    }
}

/// One record as the node applies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// Which record.
    pub key: RecordKey,
    /// The record's version: the node applies by the version rule, so a repeat is harmless.
    pub version: u64,
    /// The entity schema version the image was captured under.
    pub entity_schema: u64,
    /// The image after `excluded_fields.down`.
    pub image: Value,
}

/// A group of records applied in one transaction: a record, or a cycle that cannot be split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    /// The records. Every link of a record points into an earlier bundle or into this one.
    pub records: Vec<ChunkRecord>,
    /// Total image bytes (compact JSON length).
    pub bytes: usize,
}

/// What the hub sends next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// Bundles in the order they must be applied.
    pub bundles: Vec<Bundle>,
    /// Total image bytes of the chunk; never above the limits.
    pub bytes: usize,
    /// Groups delivered up to and including this chunk.
    pub groups_done: u64,
    /// Groups in the whole plan.
    pub groups_total: u64,
    /// The cursor to persist (with the applied records, in one transaction) after applying.
    pub cursor: Cursor,
    /// Set on the final chunk: the node sets its applied prefix to the snapshot position and
    /// continues incrementally from this position (`P + 1`).
    pub resume_from: Option<u64>,
}

/// Where a session stands. Opaque and serialisable; it holds positions and a plan digest, no
/// record content, no key and no identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    version: u32,
    binding: u64,
    generation: u64,
    position: u64,
    next_group: u64,
    done: bool,
}

impl Cursor {
    /// The cursor as JSON text.
    ///
    /// # Errors
    /// `SourceFailed` cannot happen for this type; the error is only the encoder's.
    pub fn to_json(&self) -> Result<String, BootstrapError> {
        serde_json::to_string(self).map_err(|_| BootstrapError::CursorMismatch {
            reason: "the cursor cannot be encoded",
        })
    }

    /// Read a cursor back. Anything that is not a cursor is refused.
    ///
    /// # Errors
    /// `CursorMismatch` for unparseable text.
    pub fn from_json(text: &str) -> Result<Self, BootstrapError> {
        serde_json::from_str(text).map_err(|_| BootstrapError::CursorMismatch {
            reason: "the text is not a cursor",
        })
    }
}

/// The entry point.
pub struct Bootstrap;

impl Bootstrap {
    /// Plan a bootstrap of `node` from `source`, for the scope in `seams`.
    ///
    /// Refuses: unusable limits; a snapshot of another tenant; an untrusted snapshot unless
    /// `opts.allow_untrusted`; a snapshot from another generation than the hub's, or beyond the
    /// hub's head; a link the catalog cannot read; a cycle too large for one bundle.
    ///
    /// # Errors
    /// See above; each is a [`BootstrapError`].
    pub fn start<'a>(
        source: &'a dyn SnapshotSource,
        seams: Seams<'a>,
        limits: Limits,
        node: &NodeState,
        hub: HubHead,
        opts: StartOptions,
    ) -> Result<Session<'a>, BootstrapError> {
        Session::start(source, seams, limits, node, hub, opts)
    }
}

/// One bootstrap in progress.
pub struct Session<'a> {
    source: &'a dyn SnapshotSource,
    seams: Seams<'a>,
    limits: Limits,
    plan: Plan,
    generation: u64,
    position: u64,
    resume_from: u64,
    binding: u64,
    next_group: usize,
    done: bool,
}

impl<'a> Session<'a> {
    fn start(
        source: &'a dyn SnapshotSource,
        seams: Seams<'a>,
        limits: Limits,
        node: &NodeState,
        hub: HubHead,
        opts: StartOptions,
    ) -> Result<Self, BootstrapError> {
        limits
            .validate()
            .map_err(|e| BootstrapError::InvalidSettings {
                reason: match e {
                    spot_sync_closure::ClosureError::InvalidLimits { reason } => reason,
                    _ => "the limits cannot be used",
                },
            })?;
        if source.tenant() != node.tenant {
            return Err(BootstrapError::TenantMismatch);
        }
        let info = source.info();
        if !info.verified && !opts.allow_untrusted {
            return Err(BootstrapError::UntrustedSnapshot);
        }
        if info.generation != hub.generation {
            return Err(BootstrapError::SnapshotMismatch {
                reason: "the snapshot is from another generation than the hub's stream",
            });
        }
        if info.position > hub.head_position {
            return Err(BootstrapError::SnapshotMismatch {
                reason: "the snapshot is beyond the hub's head",
            });
        }
        let resume_from = info
            .position
            .checked_add(1)
            .ok_or(BootstrapError::PositionOverflow)?;

        let mut id = Fnv::new();
        id.str(source.tenant());
        id.u64(info.generation);
        id.u64(info.position);
        id.str(&node.node_id);
        id.str(&seams.scope.fingerprint());
        let plan = plan::build(source, seams, &limits, id.finish())?;
        // The plan digest covers the identity above and every planned group, so a cursor only
        // resumes against the very same plan.
        let binding = plan.digest;
        Ok(Self {
            source,
            seams,
            limits,
            plan,
            generation: info.generation,
            position: info.position,
            resume_from,
            binding,
            next_group: 0,
            done: false,
        })
    }

    /// The position the node continues from once the snapshot is applied (`P + 1`).
    #[must_use]
    pub fn resume_from(&self) -> u64 {
        self.resume_from
    }

    /// Records that will be delivered (in scope).
    #[must_use]
    pub fn records_planned(&self) -> usize {
        self.plan.records
    }

    /// Records the snapshot held that the scope does not cover. They are not in any chunk.
    #[must_use]
    pub fn records_out_of_scope(&self) -> usize {
        self.plan.out_of_scope
    }

    /// Links from delivered records to records that are not delivered (out of scope or absent).
    /// Those targets are never sent; the node parks the referrer and repairs by pull-on-miss.
    #[must_use]
    pub fn dangling_links(&self) -> usize {
        self.plan.dangling_links
    }

    /// Groups in the plan.
    #[must_use]
    pub fn groups_total(&self) -> u64 {
        self.plan.groups.len() as u64
    }

    /// The cursor for the current position.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        Cursor {
            version: CURSOR_VERSION,
            binding: self.binding,
            generation: self.generation,
            position: self.position,
            next_group: self.next_group as u64,
            done: self.done,
        }
    }

    /// Continue after a cursor the node persisted: the chunks it covers are not sent again.
    ///
    /// # Errors
    /// `CursorMismatch` when the cursor is from another snapshot, node, scope or plan, or is not
    /// possible for this plan.
    pub fn resume(&mut self, cursor: &Cursor) -> Result<(), BootstrapError> {
        let mismatch = |reason| Err(BootstrapError::CursorMismatch { reason });
        if cursor.version != CURSOR_VERSION {
            return mismatch("the cursor format is not known");
        }
        if cursor.binding != self.binding
            || cursor.generation != self.generation
            || cursor.position != self.position
        {
            return mismatch("the cursor is for another snapshot, node or scope");
        }
        let Ok(next) = usize::try_from(cursor.next_group) else {
            return mismatch("the cursor is past the plan");
        };
        if next > self.plan.groups.len() {
            return mismatch("the cursor is past the plan");
        }
        if cursor.done && next != self.plan.groups.len() {
            return mismatch("the cursor says done before the plan ends");
        }
        self.next_group = next;
        self.done = cursor.done;
        Ok(())
    }

    /// The next chunk within `budget` (and never beyond the limits), or `None` once the final
    /// chunk, the one carrying `resume_from`, has been returned.
    ///
    /// A chunk holds whole groups only. The first group of a chunk is taken even when it alone is
    /// above `budget`, as long as it is within the limits (a cycle cannot be split).
    ///
    /// # Errors
    /// `SourceFailed`, `ScopeViolation` (the source returned a record outside the request or the
    /// scope) or `SnapshotChanged` (a planned record is missing or its size differs).
    pub fn next_chunk(&mut self, budget: Budget) -> Result<Option<Chunk>, BootstrapError> {
        if self.done {
            return Ok(None);
        }
        let byte_cap = budget.max_bytes.min(self.limits.max_bytes);
        let record_cap = budget.max_records.min(self.limits.max_records);
        let total = self.plan.groups.len();

        let mut end = self.next_group;
        let (mut bytes, mut records) = (0_usize, 0_usize);
        while let Some(group) = self.plan.groups.get(end) {
            let fits = bytes.saturating_add(group.bytes) <= byte_cap
                && records.saturating_add(group.keys.len()) <= record_cap;
            if end > self.next_group && !fits {
                break;
            }
            bytes += group.bytes;
            records += group.keys.len();
            end += 1;
        }

        let wanted: Vec<RecordKey> = self.plan.groups[self.next_group..end]
            .iter()
            .flat_map(|g| g.keys.iter().cloned())
            .collect();
        let asked: BTreeSet<&RecordKey> = wanted.iter().collect();
        let mut fetched: BTreeMap<RecordKey, spot_journal_snapshot::ImageRecord> = BTreeMap::new();
        if !wanted.is_empty() {
            for record in self.source.fetch(&wanted)? {
                let key = RecordKey::new(&record.entity, &record.id);
                if !asked.contains(&key) {
                    return Err(BootstrapError::ScopeViolation {
                        entity: record.entity,
                    });
                }
                if fetched.insert(key, record).is_some() {
                    return Err(BootstrapError::SourceFailed {
                        reason: "the source returned a record twice".to_owned(),
                    });
                }
            }
        }

        let mut bundles = Vec::with_capacity(end - self.next_group);
        let mut chunk_bytes = 0_usize;
        for group in &self.plan.groups[self.next_group..end] {
            let mut out = Vec::with_capacity(group.keys.len());
            let mut group_bytes = 0_usize;
            for key in &group.keys {
                let record = fetched.remove(key).ok_or(BootstrapError::SnapshotChanged {
                    reason: "a planned record is missing from the snapshot",
                })?;
                // The scope is asked again at serve time: the plan is not trusted to be the
                // last word on what leaves this process.
                if !self.seams.scope.covers(key, &record.image) {
                    return Err(BootstrapError::ScopeViolation {
                        entity: record.entity,
                    });
                }
                let mut image = record.image;
                self.seams.redactor.redact_down(&key.entity, &mut image);
                group_bytes += image_bytes(&image);
                out.push(ChunkRecord {
                    key: key.clone(),
                    version: record.version,
                    entity_schema: record.entity_schema,
                    image,
                });
            }
            if group_bytes != group.bytes {
                return Err(BootstrapError::SnapshotChanged {
                    reason: "a group's size differs from the plan",
                });
            }
            chunk_bytes += group_bytes;
            bundles.push(Bundle {
                records: out,
                bytes: group_bytes,
            });
        }

        self.next_group = end;
        let last = end == total;
        if last {
            self.done = true;
        }
        Ok(Some(Chunk {
            bundles,
            bytes: chunk_bytes,
            groups_done: end as u64,
            groups_total: total as u64,
            cursor: self.cursor(),
            resume_from: last.then_some(self.resume_from),
        }))
    }
}
