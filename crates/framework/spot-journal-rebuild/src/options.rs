//! The options and the outcome of a rebuild.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use spot_events_contract::envelope::Position;
use spot_journal_rebuild_plan::{Base, ForkPoint};
use spot_journal_store::TenantId;

use crate::error::RebuildError;

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

impl EventClass {
    /// The class as the key of [`RebuildOutcome::classes`].
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Domain => "domain",
            Self::Audit => "audit",
        }
    }
}

/// The key of [`RebuildOutcome::classes`] for entries whose topic the classifier did not know.
pub const UNKNOWN_CLASS: &str = "unknown";

/// The most distinct unknown topics a [`RebuildOutcome`] lists.
pub const MAX_UNKNOWN_TOPICS: usize = 100;

/// Decides the class of a topic. The rebuild never replays by guessing: a topic the classifier
/// does not know answers `None`; the executor refuses it unless told otherwise (see
/// [`RebuildOptions::strict_topics`] and [`RebuildOptions::allow_unregistered`]).
pub trait EventClassifier: Send + Sync {
    /// The class of `topic`, or `None` when the classifier does not know the topic.
    fn class_of(&self, topic: &str) -> Option<EventClass>;
}

/// The default classifier: the events contract registry (`spot_events_contract::registry`, one
/// class per platform topic, ADR-061 s2 and amendment A.0.1). A topic the registry does not hold
/// answers `None`: it is **never guessed** from its name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegistryClassifier;

impl EventClassifier for RegistryClassifier {
    fn class_of(&self, topic: &str) -> Option<EventClass> {
        use spot_events_contract::registry::{Class, class_of};
        class_of(topic).map(|class| match class {
            Class::State => EventClass::State,
            Class::Domain => EventClass::Domain,
            Class::Audit => EventClass::Audit,
        })
    }
}

/// The prefix of the identity audit topics: counted separately in [`RebuildOutcome`].
pub const IDENTITY_PREFIX: &str = "identity_";

/// The outcome warning when `allow_unregistered` applied unregistered topics as state.
pub const WARN_UNREGISTERED_AS_STATE: &str = "rebuild.unregistered_topics_applied_as_state";

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
    /// Decides the class of each topic. `None` is [`RegistryClassifier`], the events contract
    /// registry; tests and apps with their own topics inject a classifier.
    pub classifier: Option<&'a dyn EventClassifier>,
    /// Refuses a rebuild that meets a topic the classifier does not know. `true` (the default):
    /// the prescan stops before anything is applied (`journal_rebuild.unknown_topic`); without
    /// the prescan the check is made at the entry and the sink is aborted, like any other
    /// refusal. With `false` an unknown topic is treated as domain (not replayed) and listed in
    /// [`RebuildOutcome::unknown_topics`].
    pub strict_topics: bool,
    /// For a tenant in recovery only: apply an unknown topic as **state** instead of refusing it.
    /// The entry must then carry a state payload, the topics are listed in
    /// [`RebuildOutcome::unknown_topics`] and the outcome carries the warning
    /// [`WARN_UNREGISTERED_AS_STATE`]. Overrides `strict_topics`.
    pub allow_unregistered: bool,
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
            classifier: None,
            strict_topics: true,
            allow_unregistered: false,
        }
    }

    /// The class of `topic` and whether the classifier did not know it (an unknown topic is
    /// treated as domain).
    pub(crate) fn resolve(&self, topic: &str) -> (EventClass, bool) {
        let known = match self.classifier {
            Some(classifier) => classifier.class_of(topic),
            None => RegistryClassifier.class_of(topic),
        };
        match known {
            Some(class) => (class, false),
            None if self.allow_unregistered => (EventClass::State, true),
            None => (EventClass::Domain, true),
        }
    }

    /// Whether an unknown topic stops the rebuild.
    pub(crate) fn refuses_unknown(&self) -> bool {
        self.strict_topics && !self.allow_unregistered
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
    /// Topics the classifier did not know, treated as domain: sorted, at most
    /// [`MAX_UNKNOWN_TOPICS`] (the smallest names when there are more).
    #[serde(default)]
    pub unknown_topics: Vec<String>,
    /// Whether more unknown topics were met than `unknown_topics` lists.
    #[serde(default)]
    pub unknown_topics_truncated: bool,
    /// Stream entries read per class (`state`, `domain`, `audit`) and `unknown`, so a report can
    /// show what a rebuild skipped.
    #[serde(default)]
    pub classes: BTreeMap<String, u64>,
    /// Of `audit_applied`, the identity audit entries (`identity_*`), counted separately.
    #[serde(default)]
    pub identity_audit_applied: u64,
    /// Visible warnings, for example [`WARN_UNREGISTERED_AS_STATE`].
    #[serde(default)]
    pub warnings: Vec<String>,
}
