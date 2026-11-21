//! What a dispatcher hands the gate: the effect reference, the ledger record, and the sequence
//! that preserves the original order.

use serde::{Deserialize, Serialize};
use spot_core::SiteId;

use crate::class::EffectClass;
use crate::hold::MigrationId;

/// The ledger's id for a held effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EffectId(pub u64);

/// An effect's place in its site's original order. Monotonic per site, assigned by the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Sequence(pub u64);

/// Where a held effect's payload lives. The payload itself never enters this crate's ledger when
/// it holds personal or secret data; the ledger keeps an opaque handle, a hash and a size.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadRef {
    /// The effect carries no separately stored payload (its arguments live in the outbox row).
    Absent,
    /// A payload stored elsewhere: an opaque handle, a content hash and a byte size.
    Stored {
        /// The opaque handle the owner can resolve.
        handle: String,
        /// The content hash, so a release can prove it dispatches the same bytes.
        hash: String,
        /// The size in bytes.
        size: u64,
    },
}

impl PayloadRef {
    /// A reference to a payload stored elsewhere.
    #[must_use]
    pub fn stored(handle: impl Into<String>, hash: impl Into<String>, size: u64) -> Self {
        PayloadRef::Stored {
            handle: handle.into(),
            hash: hash.into(),
            size,
        }
    }
}

/// What a dispatcher hands [`Gate::admit`](crate::Gate::admit) immediately before the
/// irrevocable act.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectRef {
    /// The site the effect belongs to.
    #[serde(with = "crate::serde_site")]
    pub site: SiteId,
    /// The class.
    pub class: EffectClass,
    /// The specific kind, for example an invoice mail. Free-form; used in reports.
    pub kind: String,
    /// The idempotency key. The original one is preserved through hold and release, so a crash
    /// mid-release followed by a retry never double-sends.
    pub key: String,
    /// The ordering group. Effects of the same source keep their relative order. Defaults to the
    /// class key; a job effect uses its queue name.
    pub source: String,
    /// The outbox event that produced the effect, if any.
    pub source_event: Option<String>,
    /// Unix microseconds, supplied by the caller.
    pub created_at: i64,
    /// Where the payload lives.
    pub payload_ref: PayloadRef,
}

impl EffectRef {
    /// A reference with the class key as its source and no outbox event or payload.
    #[must_use]
    pub fn new(
        site: SiteId,
        class: EffectClass,
        kind: impl Into<String>,
        key: impl Into<String>,
        created_at: i64,
    ) -> Self {
        Self {
            source: class.key(),
            site,
            class,
            kind: kind.into(),
            key: key.into(),
            source_event: None,
            created_at,
            payload_ref: PayloadRef::Absent,
        }
    }

    /// The same reference with an explicit ordering source.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// The same reference with the outbox event that produced it.
    #[must_use]
    pub fn with_source_event(mut self, event: impl Into<String>) -> Self {
        self.source_event = Some(event.into());
        self
    }

    /// The same reference with a payload stored elsewhere.
    #[must_use]
    pub fn with_payload_ref(mut self, payload_ref: PayloadRef) -> Self {
        self.payload_ref = payload_ref;
        self
    }
}

/// A new effect the caller asks the ledger to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewEffect {
    /// The reference.
    pub effect: EffectRef,
    /// The migration the effect is held under.
    pub migration_id: MigrationId,
}

/// Where one held effect is on its way to a terminal state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectState {
    /// Recorded and held; not yet dispatched.
    Held,
    /// A release has started dispatching it; a crash here re-dispatches with the same key.
    Releasing,
    /// Dispatched and done.
    Dispatched,
    /// Dropped because the migration was rolled back.
    Discarded,
    /// Older than its class' `max_hold`; waits for explicit confirmation.
    NeedsConfirmation,
}

impl EffectState {
    /// Whether the state is terminal (no later mechanism replays it).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, EffectState::Dispatched | EffectState::Discarded)
    }
}

/// The stable outcome code of a transition that lost its compare-and-set: someone else acted on
/// the effect first. It is an outcome, counted and reported, never an error and never logged as
/// one.
pub const STALE_OUTCOME: &str = concat!("effect_hold", ".stale");

/// What [`LedgerStore::transition_effect`](crate::LedgerStore::transition_effect) did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    /// The effect was in the expected state and now is in the new one.
    Applied,
    /// The effect was not in the expected state: another node (or an earlier pass) acted on it
    /// first. Nothing changed; the caller must not act on the effect.
    Stale {
        /// The state the effect was actually in.
        actual: EffectState,
    },
}

impl Transition {
    /// Whether the transition was applied.
    #[must_use]
    pub fn is_applied(self) -> bool {
        matches!(self, Transition::Applied)
    }
}

/// Refuses a transition no ledger may apply: one that leaves a terminal state. A terminal effect
/// never acts again, so asking to move it is a bug in the caller, reported rather than raced.
///
/// # Errors
/// [`Error::InvalidInput`](crate::Error::InvalidInput) when `expected` is terminal.
pub fn validate_transition(expected: EffectState) -> crate::error::Result<()> {
    if expected.is_terminal() {
        return Err(crate::error::Error::InvalidInput {
            what: "effect transition".to_owned(),
            reason: "a terminal state cannot be left".to_owned(),
        });
    }
    Ok(())
}

/// One held effect as the ledger stores it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectRecord {
    /// The ledger id.
    pub id: EffectId,
    /// The site.
    #[serde(with = "crate::serde_site")]
    pub site: SiteId,
    /// The class.
    pub class: EffectClass,
    /// The kind.
    pub kind: String,
    /// The idempotency key.
    pub key: String,
    /// The ordering source.
    pub source: String,
    /// The outbox event, if any.
    pub source_event: Option<String>,
    /// Unix microseconds the effect was created at.
    pub created_at: i64,
    /// The original order within the site.
    pub sequence: Sequence,
    /// Where the payload lives.
    pub payload_ref: PayloadRef,
    /// The migration that held it.
    pub migration_id: MigrationId,
    /// Its state.
    pub state: EffectState,
    /// A free-form note: the discard reason, a needs-confirmation hint.
    pub note: Option<String>,
    /// How many times a release has taken the effect into `Releasing`. Counted by the ledger on
    /// every transition into that state, so a retry after a failure or a crash is visible.
    pub attempts: u32,
}

impl EffectRecord {
    /// The scope that holds this effect: a migration, or a request (ADR-080 section 6.7).
    ///
    /// Derived from the ledger key, so every ledger backend remembers it without a new column.
    #[must_use]
    pub fn scope(&self) -> crate::scope::HoldScope {
        crate::scope::HoldScope::from_key(&self.migration_id)
    }

    /// The reference a dispatcher receives on release.
    #[must_use]
    pub fn as_ref(&self) -> EffectRef {
        EffectRef {
            site: self.site.clone(),
            class: self.class.clone(),
            kind: self.kind.clone(),
            key: self.key.clone(),
            source: self.source.clone(),
            source_event: self.source_event.clone(),
            created_at: self.created_at,
            payload_ref: self.payload_ref.clone(),
        }
    }
}
