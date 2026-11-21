//! Master-data versions, effective dating, and the active view (ADR-065 section 6).
//!
//! A **master-data version** is a [`MasterChange`]: an entity and record name, a version, the
//! scope keys it requires, and an opaque payload. Routing (ADR-063) reads only the entity and the
//! scope keys; this crate never interprets the payload.
//!
//! An [`EffectiveChange`] is that version with the instant it becomes effective. The rule of
//! ADR-065 section 6 is that a receiver **stores the version on arrival but activates it by the
//! clock**, never because a message arrived: an [`ActiveView`] answers "which version is in force
//! at this instant?" and a change that arrived *after* its effective instant is flagged
//! [`late`](ArrivedChange::is_late) with the [`delay`](ArrivedChange::lateness).

use std::time::Duration;

use spot_sync_protocol::record::MAX_PAYLOAD_BYTES;
use spot_sync_protocol::{RecordKey, RecordName};

use crate::error::{Error, Result};
use crate::scope::{Doctype, ScopeKey};
use crate::time::Instant;

/// One master-data version the hub publishes.
///
/// The `(name, version)` pair is the idempotency key (ADR-023, ADR-062 G4): the same version
/// applied twice changes nothing. `scope_keys` are the keys the change requires: a node receives
/// it only if its subscription holds every one of them (ADR-063 section 3). An empty set means
/// "every node subscribed to the entity" (the `all` selector).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MasterChange {
    entity: Doctype,
    name: RecordName,
    version: u64,
    scope_keys: Vec<ScopeKey>,
    payload: String,
}

impl MasterChange {
    /// A master-data version.
    ///
    /// Fails when `version` is zero (ADR-053: versions start at 1) or the payload is larger than
    /// the sync protocol's record limit. The scope keys are sorted and deduplicated so two
    /// changes with the same keys compare equal whatever order they were given in.
    pub fn new(
        entity: Doctype,
        name: RecordName,
        version: u64,
        scope_keys: impl IntoIterator<Item = ScopeKey>,
        payload: impl Into<String>,
    ) -> Result<Self> {
        if version == 0 {
            return Err(Error::invalid_argument(
                "a master-data version is at least 1 (ADR-053)",
            ));
        }
        let payload = payload.into();
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::invalid_argument(
                "a master-data payload is at most 1 MiB",
            ));
        }
        let mut scope_keys: Vec<ScopeKey> = scope_keys.into_iter().collect();
        scope_keys.sort();
        scope_keys.dedup();
        Ok(Self {
            entity,
            name,
            version,
            scope_keys,
            payload,
        })
    }

    /// The entity (document type) this version belongs to.
    pub fn entity(&self) -> &Doctype {
        &self.entity
    }

    /// The record name.
    pub fn name(&self) -> &RecordName {
        &self.name
    }

    /// The version number.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The scope keys this change requires, sorted and deduplicated.
    pub fn scope_keys(&self) -> &[ScopeKey] {
        &self.scope_keys
    }

    /// Whether this change requires `key`.
    pub fn requires(&self, key: &ScopeKey) -> bool {
        self.scope_keys.binary_search(key).is_ok()
    }

    /// The opaque payload.
    pub fn payload(&self) -> &str {
        &self.payload
    }

    /// The idempotency key: the record name and this version.
    pub fn key(&self) -> RecordKey {
        RecordKey::new(self.name.clone(), self.version)
    }
}

/// A master-data version with the instant it becomes effective.
///
/// The instant is what the version means, not when it was sent: a change may be delivered days
/// early and still only take effect at `effective_from` (ADR-065 section 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveChange {
    change: MasterChange,
    effective_from: Instant,
}

impl EffectiveChange {
    /// A version with its effective instant.
    pub fn new(change: MasterChange, effective_from: Instant) -> Self {
        Self {
            change,
            effective_from,
        }
    }

    /// The version.
    pub fn change(&self) -> &MasterChange {
        &self.change
    }

    /// The instant this version becomes effective.
    pub fn effective_from(&self) -> Instant {
        self.effective_from
    }

    /// Whether the version is in force at `now`, judged by the clock alone.
    pub fn is_effective_at(&self, now: Instant) -> bool {
        now.is_at_or_after(self.effective_from)
    }
}

/// One version as a receiver saw it: the version and when it arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrivedChange {
    change: EffectiveChange,
    arrived_at: Instant,
}

impl ArrivedChange {
    /// A version and its arrival instant.
    pub fn new(change: EffectiveChange, arrived_at: Instant) -> Self {
        Self { change, arrived_at }
    }

    /// The version.
    pub fn change(&self) -> &EffectiveChange {
        &self.change
    }

    /// When the version arrived.
    pub fn arrived_at(&self) -> Instant {
        self.arrived_at
    }

    /// Whether the version arrived **after** the instant it should have taken effect.
    ///
    /// This is the ADR-065 freshness signal: a late change is one the branch could not have been
    /// selling under at the moment it became effective.
    pub fn is_late(&self) -> bool {
        self.arrived_at.unix_millis() > self.change.effective_from.unix_millis()
    }

    /// How late the version was: zero when it arrived at or before its effective instant.
    pub fn lateness(&self) -> Duration {
        self.arrived_at.duration_since(self.change.effective_from)
    }
}

/// What [`ActiveView::store`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreOutcome {
    /// The version was stored. `late` and `delay` are the arrival's freshness verdict.
    Stored {
        /// Whether the version arrived after its effective instant.
        late: bool,
        /// How late it was (zero when it was on time).
        delay: Duration,
    },
    /// The same `(name, version)` was already stored: idempotent, nothing changed.
    Duplicate,
}

/// The versions of one master record a node holds, and which one is in force.
///
/// A view is keyed by `(entity, name)`. [`store`](ActiveView::store) records arrivals; nothing is
/// activated by the act of storing. [`at`](ActiveView::at) answers the effective-dated question
/// with both facts the clock needs: the version's `effective_from` is at or before `now`, **and**
/// the version had arrived by `now`. A change that has not arrived yet cannot be in force, which
/// is exactly the offline-branch case of ADR-065 section 6.
#[derive(Clone, Debug)]
pub struct ActiveView {
    entity: Doctype,
    name: RecordName,
    stored: Vec<ArrivedChange>,
}

impl ActiveView {
    /// An empty view for one master record.
    pub fn new(entity: Doctype, name: RecordName) -> Self {
        Self {
            entity,
            name,
            stored: Vec::new(),
        }
    }

    /// The entity this view is for.
    pub fn entity(&self) -> &Doctype {
        &self.entity
    }

    /// The record name this view is for.
    pub fn name(&self) -> &RecordName {
        &self.name
    }

    /// Stores a version that arrived at `arrived_at`.
    ///
    /// Idempotent keyed by `(name, version)`: storing the same version again is a
    /// [`StoreOutcome::Duplicate`] and changes nothing. Fails when `change` is for another
    /// record than this view's.
    pub fn store(&mut self, change: EffectiveChange, arrived_at: Instant) -> Result<StoreOutcome> {
        let version = change.change();
        if version.entity() != &self.entity || version.name() != &self.name {
            return Err(Error::invalid_argument(
                "a change stored in a view must be for that view's entity and record name",
            ));
        }
        let key = version.key();
        if self
            .stored
            .iter()
            .any(|held| held.change.change().key() == key)
        {
            return Ok(StoreOutcome::Duplicate);
        }
        let arrived = ArrivedChange::new(change, arrived_at);
        let outcome = StoreOutcome::Stored {
            late: arrived.is_late(),
            delay: arrived.lateness(),
        };
        self.stored.push(arrived);
        Ok(outcome)
    }

    /// The version in force at `now`: the stored version with the greatest effective instant at
    /// or before `now` that had also arrived by `now`, or `None`.
    pub fn at(&self, now: Instant) -> Option<&EffectiveChange> {
        self.stored
            .iter()
            .filter(|held| {
                held.change.effective_from.unix_millis() <= now.unix_millis()
                    && held.arrived_at.unix_millis() <= now.unix_millis()
            })
            .max_by_key(|held| {
                (
                    held.change.effective_from.unix_millis(),
                    held.change.change().version(),
                )
            })
            .map(|held| &held.change)
    }

    /// The versions that have arrived but are not yet in force at `now` (future-dated), ordered
    /// by effective instant.
    pub fn pending(&self, now: Instant) -> Vec<&EffectiveChange> {
        let mut pending: Vec<&ArrivedChange> = self
            .stored
            .iter()
            .filter(|held| {
                held.arrived_at.unix_millis() <= now.unix_millis()
                    && held.change.effective_from.unix_millis() > now.unix_millis()
            })
            .collect();
        pending.sort_by_key(|held| held.change.effective_from);
        pending.into_iter().map(|held| &held.change).collect()
    }

    /// Every version that arrived after its effective instant.
    pub fn late(&self) -> Vec<&ArrivedChange> {
        self.stored.iter().filter(|held| held.is_late()).collect()
    }

    /// Whether any stored version arrived late.
    pub fn is_late(&self) -> bool {
        self.stored.iter().any(ArrivedChange::is_late)
    }

    /// The greatest lateness among the stored versions, or `None` when every one was on time.
    pub fn max_lateness(&self) -> Option<Duration> {
        self.late().iter().map(|held| held.lateness()).max()
    }

    /// Every version the view holds, in arrival order.
    pub fn stored(&self) -> &[ArrivedChange] {
        &self.stored
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn doctype() -> Doctype {
        Doctype::new("price_list").unwrap()
    }

    fn name() -> RecordName {
        RecordName::new("PL-1").unwrap()
    }

    fn change(version: u64, payload: &str) -> MasterChange {
        MasterChange::new(doctype(), name(), version, [], payload).unwrap()
    }

    fn at(millis: i64) -> Instant {
        Instant::from_unix_millis(millis)
    }

    #[test]
    fn a_future_change_is_stored_but_not_in_force_early() {
        let mut view = ActiveView::new(doctype(), name());
        view.store(EffectiveChange::new(change(1, "old"), at(0)), at(0))
            .unwrap();
        let outcome = view
            .store(EffectiveChange::new(change(2, "new"), at(1_000)), at(10))
            .unwrap();
        assert_eq!(
            outcome,
            StoreOutcome::Stored {
                late: false,
                delay: Duration::ZERO
            }
        );
        // Arrived early, but not in force until its effective instant.
        assert_eq!(view.at(at(10)).unwrap().change().payload(), "old");
        assert_eq!(view.pending(at(10)).len(), 1);
        assert_eq!(view.at(at(1_000)).unwrap().change().payload(), "new");
    }

    #[test]
    fn a_change_that_arrives_after_its_instant_is_late() {
        let mut view = ActiveView::new(doctype(), name());
        let outcome = view
            .store(EffectiveChange::new(change(2, "new"), at(1_000)), at(4_000))
            .unwrap();
        assert_eq!(
            outcome,
            StoreOutcome::Stored {
                late: true,
                delay: Duration::from_millis(3_000)
            }
        );
        assert!(view.is_late());
        assert_eq!(view.max_lateness(), Some(Duration::from_millis(3_000)));
        // It was not in force at the effective instant: it had not arrived yet.
        assert!(view.at(at(1_000)).is_none());
    }

    #[test]
    fn storing_a_version_twice_is_idempotent() {
        let mut view = ActiveView::new(doctype(), name());
        let change = EffectiveChange::new(change(2, "new"), at(1_000));
        assert!(matches!(
            view.store(change.clone(), at(10)).unwrap(),
            StoreOutcome::Stored { .. }
        ));
        assert_eq!(view.store(change, at(20)).unwrap(), StoreOutcome::Duplicate);
        assert_eq!(view.stored().len(), 1);
    }

    #[test]
    fn the_latest_effective_version_wins() {
        let mut view = ActiveView::new(doctype(), name());
        view.store(EffectiveChange::new(change(1, "v1"), at(0)), at(0))
            .unwrap();
        view.store(EffectiveChange::new(change(3, "v3"), at(300)), at(0))
            .unwrap();
        view.store(EffectiveChange::new(change(2, "v2"), at(200)), at(0))
            .unwrap();
        assert_eq!(view.at(at(250)).unwrap().change().payload(), "v2");
        assert_eq!(view.at(at(350)).unwrap().change().payload(), "v3");
    }

    #[test]
    fn a_change_for_another_record_is_refused() {
        let mut view = ActiveView::new(doctype(), name());
        let other =
            MasterChange::new(doctype(), RecordName::new("PL-2").unwrap(), 1, [], "x").unwrap();
        let error = view
            .store(EffectiveChange::new(other, at(0)), at(0))
            .err()
            .unwrap();
        assert_eq!(error.code, crate::error::INVALID_ARGUMENT);
    }

    #[test]
    fn scope_keys_are_sorted_and_deduplicated() {
        let change = MasterChange::new(
            doctype(),
            name(),
            1,
            [
                ScopeKey::new("warehouse", "W7").unwrap(),
                ScopeKey::new("company", "C1").unwrap(),
                ScopeKey::new("warehouse", "W7").unwrap(),
            ],
            "x",
        )
        .unwrap();
        assert_eq!(change.scope_keys().len(), 2);
        assert!(change.requires(&ScopeKey::new("company", "C1").unwrap()));
        assert!(!change.requires(&ScopeKey::new("company", "C2").unwrap()));
    }
}
