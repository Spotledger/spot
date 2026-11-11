//! The options and the outcome of a rebuild.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use spot_events_contract::envelope::Position;
use spot_journal_rebuild_plan::{Base, ForkPoint};
use spot_journal_store::TenantId;

use crate::error::RebuildError;

/// The topic that carries the full state of one record (ADR-061 s3).
pub const STATE_TOPIC: &str = "doc_state";
/// The reserved prefix of audit topics (ADR-056, ADR-061 s2).
pub const AUDIT_PREFIX: &str = "sys_";

/// The class of an event (ADR-061 s2). Telemetry is never in the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventClass {
    /// The full state of one record: replayed.
    State,
    /// A business or app fact for consumers: not replayed (the state it caused is).
    Domain,
    /// A system fact that matters for proof or rebuild: replayed.
    Audit,
}

/// The class of `topic`: `doc_state` is state, the reserved `sys_` prefix is audit, anything else
/// is domain.
///
/// The events contract has no class registry yet (ADR-061 s2 adds one), so the executor derives
/// the class from the topic the same way the reserved prefixes do. A topic that is new to the
/// platform and is neither is domain, which is never replayed: a state or audit fact must use its
/// reserved name.
#[must_use]
pub fn classify(topic: &str) -> EventClass {
    if topic == STATE_TOPIC {
        EventClass::State
    } else if topic.starts_with(AUDIT_PREFIX) {
        EventClass::Audit
    } else {
        EventClass::Domain
    }
}

/// What a progress callback is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Snapshot images and stream entries handed over or skipped so far.
    pub processed: u64,
    /// The last position reached (the snapshot's, while images are streamed).
    pub position: Position,
}

/// How a rebuild runs. All of it is supplied by the caller: the library reads no clock, random
/// source or key.
pub struct RebuildOptions<'a> {
    /// The tenant being rebuilt. Every key the executor reads is built from it.
    pub tenant: TenantId,
    /// The source whose stream is replayed: every manifest, segment header and envelope must
    /// name it.
    pub source: String,
    /// The schema every image is upcast to, for entities without an entry in
    /// [`RebuildOptions::entity_targets`].
    pub target_schema: u64,
    /// Per-entity target schemas, for a catalog where entities are at different versions.
    pub entity_targets: BTreeMap<String, u64>,
    /// Skips the light journal verification (the manifest signature, ownership and every
    /// segment's hash on read are still checked).
    pub skip_verify: bool,
    /// Reads the planned segments once before applying anything, so an unreadable entry, a gap
    /// or a missing upcaster is found with nothing applied. Costs a second read of the segments.
    pub prescan: bool,
    /// Hands audit-class entries to the sink (the default). Without it they are counted and
    /// skipped.
    pub replay_audit: bool,
    /// Called every [`RebuildOptions::progress_every`] items.
    pub progress: Option<&'a (dyn Fn(Progress) + Send + Sync)>,
    /// The progress interval; `0` is refused when a callback is set.
    pub progress_every: u64,
    /// Checked between items; when set the rebuild aborts the sink and returns
    /// [`RebuildError::Cancelled`].
    pub cancel: Option<&'a AtomicBool>,
}

impl<'a> RebuildOptions<'a> {
    /// Options for `tenant`/`source` upcasting to `target_schema`, with verification and the
    /// prescan on, audit replayed, no progress and no cancel.
    #[must_use]
    pub fn new(tenant: TenantId, source: impl Into<String>, target_schema: u64) -> Self {
        Self {
            tenant,
            source: source.into(),
            target_schema,
            entity_targets: BTreeMap::new(),
            skip_verify: false,
            prescan: true,
            replay_audit: true,
            progress: None,
            progress_every: 1000,
            cancel: None,
        }
    }

    /// The schema `entity` is upcast to.
    #[must_use]
    pub fn target_for(&self, entity: &str) -> u64 {
        self.entity_targets
            .get(entity)
            .copied()
            .unwrap_or(self.target_schema)
    }

    pub(crate) fn check(&self) -> Result<(), RebuildError> {
        if self.source.trim().is_empty() {
            return Err(RebuildError::OptionsInvalid("the source is empty"));
        }
        if self.progress.is_some() && self.progress_every == 0 {
            return Err(RebuildError::OptionsInvalid(
                "the progress interval is zero",
            ));
        }
        Ok(())
    }
}

/// What a completed rebuild did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildOutcome {
    /// The tenant rebuilt.
    pub tenant: String,
    /// What was restored first.
    pub base: Base,
    /// The last position applied and committed.
    pub last_position: Position,
    /// Where history was cut, when the rebuild forked. The executor never writes the manifest's
    /// fork record; that is the relay's (WP-JRN-03.T4).
    pub fork_point: Option<ForkPoint>,
    /// The generation the rebuilt stream continues in, when it forks.
    pub new_generation: Option<u64>,
    /// Whether the rebuilt database adopts the journal's positions unchanged.
    pub preserves_positions: bool,
    /// Images streamed from the base snapshot.
    pub snapshot_images: u64,
    /// Stream entries read.
    pub entries_read: u64,
    /// State images applied from the stream.
    pub state_applied: u64,
    /// Tombstones applied.
    pub deletes_applied: u64,
    /// Audit entries applied.
    pub audit_applied: u64,
    /// Audit entries skipped because replay was off.
    pub audit_skipped: u64,
    /// Domain entries skipped (not replayed).
    pub domain_skipped: u64,
    /// Re-deliveries of a version already applied, skipped.
    pub duplicates_skipped: u64,
    /// The most stream entries held in memory at once.
    pub peak_buffered_entries: usize,
}
