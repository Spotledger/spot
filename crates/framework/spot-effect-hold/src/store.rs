//! The durable ledger the hold keeps, and the in-memory reference implementation.
//!
//! # The ledger lives outside the database the migration restores
//!
//! A migration's rollback restores the *business* database. If the ledger lived there, the
//! discard step would erase the very record it is meant to act on, and a held effect whose
//! source rows disappeared in the restore could never be accounted for. Every implementation of
//! [`LedgerStore`] must therefore keep its state in a store (or schema) that the snapshot
//! excludes. The [contract suite](crate::contract) proves the observable half of that rule: the
//! ledger's contents survive a reset of the business data, and an effect whose source event no
//! longer exists is still listed until it is discarded.

use std::collections::HashMap;
use std::sync::Mutex;

use futures::future::BoxFuture;
use spot_core::SiteId;

use crate::effect::{
    EffectId, EffectRecord, EffectState, NewEffect, PayloadRef, Sequence, Transition,
    validate_transition,
};
use crate::error::{Error, Result};
use crate::hold::{HoldState, MigrationId};

/// A boxed future, so [`LedgerStore`] stays object-safe.
pub type LedgerFuture<'a, T> = BoxFuture<'a, Result<T>>;

/// How far a release has got. Checkpointed so a crash mid-release resumes rather than restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseCursor {
    /// The migration being released.
    pub migration_id: MigrationId,
    /// How many effects have been dispatched so far.
    pub dispatched: u64,
    /// The highest sequence dispatched, for a resuming `WHERE sequence > ...` scan.
    pub last_sequence: Option<Sequence>,
    /// The number of effects that waited for confirmation at the last checkpoint.
    pub needs_confirmation: u64,
}

impl ReleaseCursor {
    /// A cursor that has dispatched nothing.
    #[must_use]
    pub fn start(migration_id: MigrationId) -> Self {
        Self {
            migration_id,
            dispatched: 0,
            last_sequence: None,
            needs_confirmation: 0,
        }
    }
}

/// The durable backing for a site's hold: the hold state and the effects it holds.
///
/// Object-safe and async, so a database implementation (a later task) implements the same trait
/// and runs the same [`contract`](crate::contract) suite.
pub trait LedgerStore: Send + Sync {
    /// Appends a held effect, deduplicating on `(site, key)`. An effect already present keeps its
    /// id, sequence and state; the same id is returned.
    fn append_effect<'a>(&'a self, effect: NewEffect) -> LedgerFuture<'a, EffectId>;

    /// Lists a site's effects in the requested states, in original (sequence) order.
    fn list_effects<'a>(
        &'a self,
        site: &'a SiteId,
        states: &'a [EffectState],
    ) -> LedgerFuture<'a, Vec<EffectRecord>>;

    /// Moves one effect from `expected` to `next`, replacing its note, when and only when it is
    /// still in `expected` (a compare-and-set against the state the caller read).
    ///
    /// Returns [`Transition::Applied`], or [`Transition::Stale`] with the state it found when
    /// someone else acted first; a stale transition changes nothing, and the caller must not act
    /// on the effect. Every transition into [`EffectState::Releasing`] adds one to the effect's
    /// `attempts`. There is no unconditional setter: two nodes cannot both move one effect.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when the effect is not in the ledger or `expected` is terminal.
    fn transition_effect<'a>(
        &'a self,
        id: EffectId,
        expected: EffectState,
        next: EffectState,
        note: Option<String>,
    ) -> LedgerFuture<'a, Transition>;

    /// As [`LedgerStore::transition_effect`], also remembering `at` (unix microseconds) as the time
    /// the effect last changed state, which the retention purge ages released and discarded effects
    /// by. The default ignores `at` (a ledger that keeps no change time then ages by `created_at`);
    /// both shipped ledgers override it. `at` is an argument: the ledger reads no clock.
    fn transition_effect_at<'a>(
        &'a self,
        id: EffectId,
        expected: EffectState,
        next: EffectState,
        note: Option<String>,
        at: i64,
    ) -> LedgerFuture<'a, Transition> {
        let _ = at;
        self.transition_effect(id, expected, next, note)
    }

    /// Replaces the payload reference of at most `limit` effects of `site` with the empty marker
    /// ([`PayloadRef::Absent`]) and keeps the row: key, state, timestamps, class and the audit
    /// counts stay, so deduplication on `(site, key)` still works and the checkpoint arithmetic is
    /// unchanged. Only effects in a **terminal** state (`dispatched` or `discarded`) among `states`
    /// are touched, whatever else is asked: a `held`, `releasing` or `needs_confirmation` effect is
    /// never purged. An effect qualifies when its last change time (its `created_at` when the ledger
    /// kept none) is strictly before `older_than`. Idempotent: a second call purges nothing new.
    /// **Irreversible.** Returns how many references were replaced.
    ///
    /// The default refuses (`effect_hold.ledger_unavailable`): a ledger that cannot purge says so.
    fn purge_payloads<'a>(
        &'a self,
        site: &'a SiteId,
        states: &'a [EffectState],
        older_than: i64,
        limit: usize,
    ) -> LedgerFuture<'a, u64> {
        let _ = (site, states, older_than, limit);
        Box::pin(async {
            Err(Error::LedgerUnavailable {
                reason: "this ledger does not purge payload references".to_owned(),
            })
        })
    }

    /// The payload handles of `site` whose references were purged and whose bytes are still to be
    /// deleted, **minus** any handle some effect still references (a payload shared by two holds is
    /// kept until the last reference goes), at most `limit`, in a stable order.
    ///
    /// [`LedgerStore::purge_payloads`] queues each handle it clears in the same transaction as the
    /// clearing, so a crash cannot lose the intent to delete. The default has no queue and returns
    /// nothing.
    fn pending_payload_deletes<'a>(
        &'a self,
        site: &'a SiteId,
        limit: usize,
    ) -> LedgerFuture<'a, Vec<String>> {
        let _ = (site, limit);
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Removes `handle` from the delete queue of `site` once its bytes are gone. Removing an entry
    /// that is not queued is success. The default has no queue.
    fn complete_payload_delete<'a>(
        &'a self,
        site: &'a SiteId,
        handle: &'a str,
    ) -> LedgerFuture<'a, ()> {
        let _ = (site, handle);
        Box::pin(async { Ok(()) })
    }

    /// The site's hold state; `Open` when it has never been held.
    fn get_hold<'a>(&'a self, site: &'a SiteId) -> LedgerFuture<'a, HoldState>;

    /// Sets the hold state when, and only when, the current state equals `expected`.
    ///
    /// Returns whether it did. A compare-and-set, so two `begin`s racing on one site cannot both
    /// succeed.
    fn compare_and_set_hold<'a>(
        &'a self,
        site: &'a SiteId,
        expected: &'a HoldState,
        new: HoldState,
    ) -> LedgerFuture<'a, bool>;

    /// Records a release's progress.
    fn checkpoint<'a>(&'a self, site: &'a SiteId, cursor: ReleaseCursor) -> LedgerFuture<'a, ()>;

    /// The last recorded release progress for a migration, if any.
    fn get_checkpoint<'a>(
        &'a self,
        site: &'a SiteId,
        migration_id: &'a MigrationId,
    ) -> LedgerFuture<'a, Option<ReleaseCursor>>;
}

#[derive(Default)]
struct LedgerData {
    next_effect: u64,
    effects: Vec<EffectRecord>,
    by_key: HashMap<(String, String), EffectId>,
    next_sequence: HashMap<String, u64>,
    holds: HashMap<String, HoldState>,
    cursors: HashMap<(String, String), ReleaseCursor>,
    /// When each effect last changed state (see [`LedgerStore::transition_effect_at`]).
    changed: HashMap<u64, i64>,
    /// `(site, handle)` of purged references whose bytes are still to be deleted.
    pending_deletes: std::collections::BTreeSet<(String, String)>,
}

impl LedgerData {
    fn next_id(&mut self) -> EffectId {
        self.next_effect += 1;
        EffectId(self.next_effect)
    }

    fn next_sequence(&mut self, site: &str) -> Sequence {
        let next = self.next_sequence.entry(site.to_owned()).or_insert(0);
        *next += 1;
        Sequence(*next)
    }

    fn hold(&self, site: &str) -> HoldState {
        self.holds.get(site).cloned().unwrap_or(HoldState::Open)
    }
}

/// An in-memory ledger, the reference implementation of [`LedgerStore`].
///
/// It stands in for the separate store a real deployment keeps outside the restorable business
/// database; dropping it is the only way its contents go away.
#[derive(Default)]
pub struct InMemoryLedger {
    data: Mutex<LedgerData>,
}

impl InMemoryLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LedgerData>> {
        self.data.lock().map_err(|_| Error::LedgerUnavailable {
            reason: "the in-memory ledger lock is poisoned".to_owned(),
        })
    }
}

impl std::fmt::Debug for InMemoryLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryLedger").finish_non_exhaustive()
    }
}

impl LedgerStore for InMemoryLedger {
    fn transition_effect_at<'a>(
        &'a self,
        id: EffectId,
        expected: EffectState,
        next: EffectState,
        note: Option<String>,
        at: i64,
    ) -> LedgerFuture<'a, Transition> {
        Box::pin(async move {
            let outcome = self.transition_effect(id, expected, next, note).await?;
            if matches!(outcome, Transition::Applied) {
                self.lock()?.changed.insert(id.0, at);
            }
            Ok(outcome)
        })
    }

    fn purge_payloads<'a>(
        &'a self,
        site: &'a SiteId,
        states: &'a [EffectState],
        older_than: i64,
        limit: usize,
    ) -> LedgerFuture<'a, u64> {
        Box::pin(async move {
            let mut data = self.lock()?;
            let changed = data.changed.clone();
            let mut purged = 0_u64;
            let mut queued: Vec<(String, String)> = Vec::new();
            for record in &mut data.effects {
                if usize::try_from(purged).unwrap_or(usize::MAX) >= limit {
                    break;
                }
                let terminal = matches!(
                    record.state,
                    EffectState::Dispatched | EffectState::Discarded
                );
                if record.site != *site
                    || !terminal
                    || !states.contains(&record.state)
                    || record.payload_ref == PayloadRef::Absent
                {
                    continue;
                }
                let at = changed
                    .get(&record.id.0)
                    .copied()
                    .unwrap_or(record.created_at);
                if at < older_than {
                    // Clearing and queuing happen under one lock: the same atomic step as the
                    // PostgreSQL ledger's single statement.
                    if let PayloadRef::Stored { handle, .. } = &record.payload_ref {
                        queued.push((record.site.as_str().to_owned(), handle.clone()));
                    }
                    record.payload_ref = PayloadRef::Absent;
                    purged += 1;
                }
            }
            data.pending_deletes.extend(queued);
            Ok(purged)
        })
    }

    fn pending_payload_deletes<'a>(
        &'a self,
        site: &'a SiteId,
        limit: usize,
    ) -> LedgerFuture<'a, Vec<String>> {
        Box::pin(async move {
            let data = self.lock()?;
            let in_use = |handle: &str| {
                data.effects.iter().any(|record| {
                    matches!(&record.payload_ref, PayloadRef::Stored { handle: h, .. } if h == handle)
                })
            };
            Ok(data
                .pending_deletes
                .iter()
                .filter(|(queued_site, handle)| queued_site == site.as_str() && !in_use(handle))
                .map(|(_, handle)| handle.clone())
                .take(limit)
                .collect())
        })
    }

    fn complete_payload_delete<'a>(
        &'a self,
        site: &'a SiteId,
        handle: &'a str,
    ) -> LedgerFuture<'a, ()> {
        Box::pin(async move {
            self.lock()?
                .pending_deletes
                .remove(&(site.as_str().to_owned(), handle.to_owned()));
            Ok(())
        })
    }

    fn append_effect<'a>(&'a self, effect: NewEffect) -> LedgerFuture<'a, EffectId> {
        Box::pin(async move {
            let mut data = self.lock()?;
            let key = (
                effect.effect.site.as_str().to_owned(),
                effect.effect.key.clone(),
            );
            if let Some(existing) = data.by_key.get(&key) {
                return Ok(*existing);
            }
            let id = data.next_id();
            let sequence = data.next_sequence(effect.effect.site.as_str());
            let record = EffectRecord {
                id,
                site: effect.effect.site,
                class: effect.effect.class,
                kind: effect.effect.kind,
                key: effect.effect.key,
                source: effect.effect.source,
                source_event: effect.effect.source_event,
                created_at: effect.effect.created_at,
                sequence,
                payload_ref: effect.effect.payload_ref,
                migration_id: effect.migration_id,
                state: EffectState::Held,
                note: None,
                attempts: 0,
            };
            data.by_key.insert(key, id);
            data.effects.push(record);
            Ok(id)
        })
    }

    fn list_effects<'a>(
        &'a self,
        site: &'a SiteId,
        states: &'a [EffectState],
    ) -> LedgerFuture<'a, Vec<EffectRecord>> {
        Box::pin(async move {
            let data = self.lock()?;
            let mut effects: Vec<EffectRecord> = data
                .effects
                .iter()
                .filter(|r| r.site == *site && states.contains(&r.state))
                .cloned()
                .collect();
            effects.sort_by_key(|r| r.sequence);
            Ok(effects)
        })
    }

    fn transition_effect<'a>(
        &'a self,
        id: EffectId,
        expected: EffectState,
        next: EffectState,
        note: Option<String>,
    ) -> LedgerFuture<'a, Transition> {
        Box::pin(async move {
            validate_transition(expected)?;
            let mut data = self.lock()?;
            let record = data
                .effects
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or_else(|| Error::InvalidInput {
                    what: format!("effect id {}", id.0),
                    reason: "is not in the ledger".to_owned(),
                })?;
            if record.state != expected {
                return Ok(Transition::Stale {
                    actual: record.state,
                });
            }
            record.state = next;
            record.note = note;
            if next == EffectState::Releasing {
                record.attempts = record.attempts.saturating_add(1);
            }
            Ok(Transition::Applied)
        })
    }

    fn get_hold<'a>(&'a self, site: &'a SiteId) -> LedgerFuture<'a, HoldState> {
        Box::pin(async move {
            let data = self.lock()?;
            Ok(data.hold(site.as_str()))
        })
    }

    fn compare_and_set_hold<'a>(
        &'a self,
        site: &'a SiteId,
        expected: &'a HoldState,
        new: HoldState,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(async move {
            let mut data = self.lock()?;
            if data.hold(site.as_str()) != *expected {
                return Ok(false);
            }
            data.holds.insert(site.as_str().to_owned(), new);
            Ok(true)
        })
    }

    fn checkpoint<'a>(&'a self, site: &'a SiteId, cursor: ReleaseCursor) -> LedgerFuture<'a, ()> {
        Box::pin(async move {
            let mut data = self.lock()?;
            let key = (
                site.as_str().to_owned(),
                cursor.migration_id.as_str().to_owned(),
            );
            data.cursors.insert(key, cursor);
            Ok(())
        })
    }

    fn get_checkpoint<'a>(
        &'a self,
        site: &'a SiteId,
        migration_id: &'a MigrationId,
    ) -> LedgerFuture<'a, Option<ReleaseCursor>> {
        Box::pin(async move {
            let data = self.lock()?;
            let key = (site.as_str().to_owned(), migration_id.as_str().to_owned());
            Ok(data.cursors.get(&key).cloned())
        })
    }
}

/// A [`LedgerStore`] that always fails to read or write.
///
/// Tests use it to prove the gate fails closed: an unreadable ledger means the site is treated as
/// held, never as open.
#[derive(Debug, Default)]
pub struct UnavailableLedger;

impl LedgerStore for UnavailableLedger {
    fn append_effect<'a>(&'a self, _effect: NewEffect) -> LedgerFuture<'a, EffectId> {
        Box::pin(async move {
            Err(Error::LedgerUnavailable {
                reason: "the ledger is unavailable".to_owned(),
            })
        })
    }

    fn list_effects<'a>(
        &'a self,
        _site: &'a SiteId,
        _states: &'a [EffectState],
    ) -> LedgerFuture<'a, Vec<EffectRecord>> {
        Box::pin(async move {
            Err(Error::LedgerUnavailable {
                reason: "the ledger is unavailable".to_owned(),
            })
        })
    }

    fn transition_effect<'a>(
        &'a self,
        _id: EffectId,
        _expected: EffectState,
        _next: EffectState,
        _note: Option<String>,
    ) -> LedgerFuture<'a, Transition> {
        Box::pin(async move {
            Err(Error::LedgerUnavailable {
                reason: "the ledger is unavailable".to_owned(),
            })
        })
    }

    fn get_hold<'a>(&'a self, _site: &'a SiteId) -> LedgerFuture<'a, HoldState> {
        Box::pin(async move {
            Err(Error::LedgerUnavailable {
                reason: "the ledger is unavailable".to_owned(),
            })
        })
    }

    fn compare_and_set_hold<'a>(
        &'a self,
        _site: &'a SiteId,
        _expected: &'a HoldState,
        _new: HoldState,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(async move {
            Err(Error::LedgerUnavailable {
                reason: "the ledger is unavailable".to_owned(),
            })
        })
    }

    fn checkpoint<'a>(&'a self, _site: &'a SiteId, _cursor: ReleaseCursor) -> LedgerFuture<'a, ()> {
        Box::pin(async move {
            Err(Error::LedgerUnavailable {
                reason: "the ledger is unavailable".to_owned(),
            })
        })
    }

    fn get_checkpoint<'a>(
        &'a self,
        _site: &'a SiteId,
        _migration_id: &'a MigrationId,
    ) -> LedgerFuture<'a, Option<ReleaseCursor>> {
        Box::pin(async move {
            Err(Error::LedgerUnavailable {
                reason: "the ledger is unavailable".to_owned(),
            })
        })
    }
}
