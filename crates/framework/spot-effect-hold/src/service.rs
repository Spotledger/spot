//! The hold service: `begin`, `release` and `discard` over the ledger and the gate.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use spot_coordinator::{Coordinator, LockExt};
use spot_core::SiteId;

use crate::audit::{AuditEvent, AuditKind, AuditSink};
use crate::class::PolicyTable;
use crate::dispatch::{DiscardSummary, Dispatcher, Notice, Notifier};
use crate::effect::{EffectId, EffectRecord, EffectState, Sequence, Transition};
use crate::error::{Error, Result};
use crate::gate::Gate;
use crate::hold::{HoldState, MigrationId};
use crate::store::{LedgerStore, ReleaseCursor};

/// What a release should do beyond the defaults.
#[derive(Clone, Debug)]
pub struct ReleasePlan {
    /// Idempotency keys the operator explicitly confirms for release even though they are older
    /// than their class' `max_hold`.
    pub confirmation: BTreeSet<String>,
    /// The most effects of one class a release may dispatch at once. At least `1`; the reference
    /// release dispatches sequentially, which honours any cap.
    pub max_concurrency_per_class: usize,
}

impl Default for ReleasePlan {
    fn default() -> Self {
        Self {
            confirmation: BTreeSet::new(),
            max_concurrency_per_class: 1,
        }
    }
}

impl ReleasePlan {
    /// A plan that confirms the named keys and dispatches one effect at a time.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Confirms `key` for release despite its age.
    #[must_use]
    pub fn confirm(mut self, key: impl Into<String>) -> Self {
        self.confirmation.insert(key.into());
        self
    }
}

/// What a release did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseReport {
    /// The site.
    pub site: SiteId,
    /// The migration.
    pub migration_id: MigrationId,
    /// Effects dispatched, in dispatch order.
    pub dispatched: Vec<EffectId>,
    /// Stale effects that waited for confirmation.
    pub needs_confirmation: Vec<EffectId>,
    /// Effects whose dispatch failed and were returned to `Held`.
    pub failed: Vec<EffectId>,
    /// The checkpoint the release resumed from, if it resumed one.
    pub resumed_from: Option<ReleaseCursor>,
    /// Effects another node acted on first (`effect_hold.stale`): not dispatched here. A release
    /// with any stale effect leaves the hold held; running it again finishes it.
    pub stale: Vec<EffectId>,
    /// Whether the hold moved to `Released`.
    pub completed: bool,
}

/// What a discard did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscardReport {
    /// The site.
    pub site: SiteId,
    /// The migration.
    pub migration_id: MigrationId,
    /// How many effects were discarded.
    pub discarded: u64,
    /// How many raised a task for a person.
    pub notified: u64,
    /// Effects another node acted on first (`effect_hold.stale`): not discarded here.
    pub stale: u64,
    /// The summary event, also recorded in the audit chain.
    pub summary: DiscardSummary,
}

/// The side-effect hold for one site.
pub struct HoldService {
    pub(crate) site: SiteId,
    pub(crate) ledger: Arc<dyn LedgerStore>,
    pub(crate) policies: Arc<PolicyTable>,
    pub(crate) gate: Arc<Gate>,
    pub(crate) audit: Arc<dyn AuditSink>,
    pub(crate) dispatcher: Arc<dyn Dispatcher>,
    pub(crate) notifier: Arc<dyn Notifier>,
    pub(crate) coordinator: Option<Arc<dyn Coordinator>>,
    pub(crate) requests: crate::request::RequestGuards,
    stale: AtomicU64,
}

impl HoldService {
    /// A service for `site` over `ledger`, `policies` and the transports.
    #[must_use]
    pub fn new(
        site: SiteId,
        ledger: Arc<dyn LedgerStore>,
        policies: Arc<PolicyTable>,
        audit: Arc<dyn AuditSink>,
        dispatcher: Arc<dyn Dispatcher>,
        notifier: Arc<dyn Notifier>,
    ) -> Self {
        let gate = Arc::new(Gate::new(
            site.clone(),
            Arc::clone(&ledger),
            Arc::clone(&policies),
        ));
        Self {
            site,
            ledger,
            policies,
            gate,
            audit,
            dispatcher,
            notifier,
            coordinator: None,
            requests: crate::request::RequestGuards::default(),
            stale: AtomicU64::new(0),
        }
    }

    /// How many transitions this service has lost to another node (`effect_hold.stale`) since it
    /// was built. A counter, not an error: the loser simply does not act.
    #[must_use]
    pub fn stale_count(&self) -> u64 {
        self.stale.load(Ordering::Relaxed)
    }

    /// Moves `effect` from the state it was read in to `next`. A stale result is counted and
    /// returned as `false`; the caller must not act on the effect then.
    pub(crate) async fn transition(
        &self,
        effect: &EffectRecord,
        expected: EffectState,
        next: EffectState,
        note: Option<String>,
    ) -> Result<bool> {
        match self
            .ledger
            .transition_effect(effect.id, expected, next, note)
            .await?
        {
            Transition::Applied => Ok(true),
            Transition::Stale { .. } => {
                self.stale.fetch_add(1, Ordering::Relaxed);
                Ok(false)
            }
        }
    }

    /// The same service holding a coordinator to serialise releases per site.
    #[must_use]
    pub fn with_coordinator(mut self, coordinator: Arc<dyn Coordinator>) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The gate every dispatcher shares.
    #[must_use]
    pub fn gate(&self) -> Arc<Gate> {
        Arc::clone(&self.gate)
    }

    /// Reads the durable hold state into the gate, stamping the snapshot with `now`. An unreadable
    /// ledger leaves the gate unknown (treated as held).
    pub async fn load(&self, now: i64) {
        self.gate.load(now).await;
    }

    /// Starts holding the site while `migration_id` is unverified.
    ///
    /// The hold is durable before this returns, recorded in the audit chain, and the gate is
    /// marked held before it returns, so no held-class effect can be admitted to proceed after a
    /// successful `begin`.
    ///
    /// # Errors
    /// [`Error::AlreadyHeld`] when the site already has a live hold, and
    /// [`Error::LedgerUnavailable`] when the ledger cannot be written.
    pub async fn begin(
        &self,
        migration_id: impl Into<String>,
        reason: impl Into<String>,
        now: i64,
        expected_by: Option<i64>,
    ) -> Result<HoldState> {
        let migration_id = MigrationId::new(migration_id)?;
        if migration_id.is_request_key() {
            return Err(Error::InvalidInput {
                what: "migration id".to_owned(),
                reason: "the `request:` prefix is reserved for request scopes".to_owned(),
            });
        }
        let current = self.ledger.get_hold(&self.site).await?;
        if current.is_held() {
            return Err(Error::AlreadyHeld {
                site: self.site.to_string(),
                migration_id: current
                    .migration_id()
                    .map_or_else(String::new, ToString::to_string),
            });
        }
        let held = HoldState::Held {
            migration_id: migration_id.clone(),
            reason: reason.into(),
            since: now,
            expected_by,
        };
        let set = self
            .ledger
            .compare_and_set_hold(&self.site, &current, held.clone())
            .await?;
        if !set {
            return Err(Error::AlreadyHeld {
                site: self.site.to_string(),
                migration_id: migration_id.to_string(),
            });
        }
        // Mark the gate held *before* the audit write: if the audit fails, the site is still held
        // (fail closed), which is the safe side of the error.
        self.gate.begin_hold(migration_id.clone(), now).await;
        self.audit
            .record(AuditEvent::new(
                AuditKind::Begin,
                self.site.clone(),
                Some(migration_id.clone()),
                now,
            ))
            .await?;
        Ok(held)
    }

    /// Releases a held site: re-dispatches held effects in their original order per source, each
    /// with its original idempotency key, then moves the hold to `Released`.
    ///
    /// Effects older than their class' `max_hold` are not auto-released; they are listed in
    /// [`ReleaseReport::needs_confirmation`] and the hold stays held until a later release
    /// confirms them.
    ///
    /// # Errors
    /// [`Error::NotHeld`], [`Error::WrongMigration`], [`Error::ReleaseInProgress`] and
    /// [`Error::DispatchFailed`].
    pub async fn release(
        &self,
        migration_id: impl Into<String>,
        plan: ReleasePlan,
        now: i64,
    ) -> Result<ReleaseReport> {
        let migration_id = MigrationId::new(migration_id)?;
        let _lease = self.acquire_release_lease().await?;
        let _cap = plan.max_concurrency_per_class.max(1);
        let current = self.ledger.get_hold(&self.site).await?;
        let held = match &current {
            HoldState::Held {
                migration_id: live, ..
            } if *live == migration_id => current.clone(),
            HoldState::Held {
                migration_id: live, ..
            } => {
                return Err(Error::WrongMigration {
                    site: self.site.to_string(),
                    held: live.to_string(),
                    requested: migration_id.to_string(),
                });
            }
            _ => {
                return Err(Error::NotHeld {
                    site: self.site.to_string(),
                });
            }
        };

        let mut effects = self
            .ledger
            .list_effects(
                &self.site,
                &[
                    EffectState::Held,
                    EffectState::Releasing,
                    EffectState::NeedsConfirmation,
                ],
            )
            .await?;
        effects.retain(|effect| effect.migration_id == migration_id);
        effects.sort_by(|a, b| a.source.cmp(&b.source).then(a.sequence.cmp(&b.sequence)));

        let resumed_from = self
            .ledger
            .get_checkpoint(&self.site, &migration_id)
            .await?;
        let mut cursor = resumed_from
            .clone()
            .unwrap_or_else(|| ReleaseCursor::start(migration_id.clone()));
        if resumed_from.is_some() {
            self.audit
                .record(
                    AuditEvent::new(
                        AuditKind::Resume,
                        self.site.clone(),
                        Some(migration_id.clone()),
                        now,
                    )
                    .with_count(
                        "dispatched",
                        i64::try_from(cursor.dispatched).unwrap_or(i64::MAX),
                    ),
                )
                .await?;
        }

        let mut report = ReleaseReport {
            site: self.site.clone(),
            migration_id: migration_id.clone(),
            dispatched: Vec::new(),
            needs_confirmation: Vec::new(),
            failed: Vec::new(),
            resumed_from,
            stale: Vec::new(),
            completed: false,
        };

        for effect in effects {
            let policy = self.policies.policy_for(&effect.class);
            let stale = policy
                .max_hold
                .is_some_and(|max| now.saturating_sub(effect.created_at) > micros(max));
            if stale && !plan.confirmation.contains(&effect.key) {
                if self
                    .transition(
                        &effect,
                        effect.state,
                        EffectState::NeedsConfirmation,
                        Some("older than max_hold; needs confirmation".to_owned()),
                    )
                    .await?
                {
                    report.needs_confirmation.push(effect.id);
                } else {
                    report.stale.push(effect.id);
                }
                continue;
            }
            if effect.state == EffectState::NeedsConfirmation
                && !plan.confirmation.contains(&effect.key)
            {
                report.needs_confirmation.push(effect.id);
                continue;
            }
            // Take the effect before dispatching it: only the node whose transition applied may
            // act. A stale result means another node released or discarded it first.
            if !self
                .transition(&effect, effect.state, EffectState::Releasing, None)
                .await?
            {
                report.stale.push(effect.id);
                continue;
            }
            match self.dispatcher.dispatch(&effect).await {
                Ok(()) => {
                    if self
                        .transition(
                            &effect,
                            EffectState::Releasing,
                            EffectState::Dispatched,
                            None,
                        )
                        .await?
                    {
                        report.dispatched.push(effect.id);
                        cursor.dispatched += 1;
                        cursor.last_sequence = Some(effect.sequence);
                        self.ledger.checkpoint(&self.site, cursor.clone()).await?;
                    } else {
                        report.stale.push(effect.id);
                    }
                }
                Err(error) => {
                    // The note is the stable code only: a dispatcher's own text may carry a
                    // payload fragment or a credential.
                    let returned = self
                        .transition(
                            &effect,
                            EffectState::Releasing,
                            EffectState::Held,
                            Some(format!("dispatch failed: {}", error.code())),
                        )
                        .await?;
                    if !returned {
                        report.stale.push(effect.id);
                    }
                    report.failed.push(effect.id);
                }
            }
        }

        if !report.failed.is_empty() {
            return Err(Error::DispatchFailed {
                key: report
                    .failed
                    .first()
                    .map_or_else(String::new, |id| id.0.to_string()),
                class: "release".to_owned(),
                reason: "one or more held effects failed to dispatch; the hold is unchanged"
                    .to_owned(),
            });
        }

        if !report.needs_confirmation.is_empty() {
            cursor.needs_confirmation =
                u64::try_from(report.needs_confirmation.len()).unwrap_or(u64::MAX);
            self.ledger.checkpoint(&self.site, cursor).await?;
            self.audit
                .record(
                    AuditEvent::new(
                        AuditKind::Confirmation,
                        self.site.clone(),
                        Some(migration_id.clone()),
                        now,
                    )
                    .with_count(
                        "needs_confirmation",
                        i64::try_from(report.needs_confirmation.len()).unwrap_or(i64::MAX),
                    ),
                )
                .await?;
            return Ok(report);
        }

        if !report.stale.is_empty() {
            // Another node acted on some effect first. The hold stays held; a further release
            // finds nothing pending and completes it. Recorded, because it is a refusal.
            self.audit
                .record(
                    AuditEvent::new(
                        AuditKind::Release,
                        self.site.clone(),
                        Some(migration_id.clone()),
                        now,
                    )
                    .with_count(
                        "dispatched",
                        i64::try_from(report.dispatched.len()).unwrap_or(i64::MAX),
                    )
                    .with_count(
                        "stale",
                        i64::try_from(report.stale.len()).unwrap_or(i64::MAX),
                    ),
                )
                .await?;
            return Ok(report);
        }

        let released = HoldState::Released {
            migration_id: migration_id.clone(),
            at: now,
        };
        if !self
            .ledger
            .compare_and_set_hold(&self.site, &held, released)
            .await?
        {
            return Err(Error::NotHeld {
                site: self.site.to_string(),
            });
        }
        self.gate.open(now);
        self.audit
            .record(
                AuditEvent::new(
                    AuditKind::Release,
                    self.site.clone(),
                    Some(migration_id.clone()),
                    now,
                )
                .with_count(
                    "dispatched",
                    i64::try_from(report.dispatched.len()).unwrap_or(i64::MAX),
                ),
            )
            .await?;
        report.completed = true;
        Ok(report)
    }

    /// Discards a held site: every held effect is marked `Discarded` and never dispatched; the
    /// class' discard policy runs (drop, or drop and raise a task for a person); a summary event
    /// is recorded.
    ///
    /// # Errors
    /// [`Error::NotHeld`] or [`Error::WrongMigration`].
    pub async fn discard(
        &self,
        migration_id: impl Into<String>,
        reason: impl Into<String>,
        now: i64,
    ) -> Result<DiscardReport> {
        let migration_id = MigrationId::new(migration_id)?;
        let reason = reason.into();
        let _lease = self.acquire_release_lease().await?;
        let current = self.ledger.get_hold(&self.site).await?;
        let held = match &current {
            HoldState::Held {
                migration_id: live, ..
            } if *live == migration_id => current.clone(),
            HoldState::Held {
                migration_id: live, ..
            } => {
                return Err(Error::WrongMigration {
                    site: self.site.to_string(),
                    held: live.to_string(),
                    requested: migration_id.to_string(),
                });
            }
            _ => {
                return Err(Error::NotHeld {
                    site: self.site.to_string(),
                });
            }
        };

        let mut effects = self
            .ledger
            .list_effects(
                &self.site,
                &[
                    EffectState::Held,
                    EffectState::Releasing,
                    EffectState::NeedsConfirmation,
                ],
            )
            .await?;
        effects.retain(|effect| effect.migration_id == migration_id);

        let mut discarded = 0u64;
        let mut notified = 0u64;
        let mut stale = 0u64;
        for effect in &effects {
            // Only an effect still in the state read is dropped; a stale one was released (or
            // discarded) by another node, so it is neither dropped nor reported as discarded.
            if !self
                .transition(
                    effect,
                    effect.state,
                    EffectState::Discarded,
                    Some(reason.clone()),
                )
                .await?
            {
                stale += 1;
                continue;
            }
            discarded += 1;
            if self.policies.policy_for(&effect.class).on_discard
                == crate::class::OnDiscard::DropAndNotify
            {
                self.notifier
                    .notify(Notice {
                        site: self.site.clone(),
                        class: effect.class.key(),
                        kind: effect.kind.clone(),
                        key: effect.key.clone(),
                        reason: reason.clone(),
                    })
                    .await?;
                notified += 1;
            }
        }

        let discarded_state = HoldState::Discarded {
            migration_id: migration_id.clone(),
            at: now,
            reason: reason.clone(),
        };
        if !self
            .ledger
            .compare_and_set_hold(&self.site, &held, discarded_state)
            .await?
        {
            return Err(Error::NotHeld {
                site: self.site.to_string(),
            });
        }
        self.gate.open(now);

        let summary = DiscardSummary {
            site: self.site.clone(),
            migration_id: migration_id.to_string(),
            discarded,
            notified,
            reason,
        };
        self.audit
            .record(
                AuditEvent::new(
                    AuditKind::Discard,
                    self.site.clone(),
                    Some(migration_id.clone()),
                    now,
                )
                .with_count("discarded", i64::try_from(discarded).unwrap_or(i64::MAX))
                .with_count("notified", i64::try_from(notified).unwrap_or(i64::MAX))
                .with_count("stale", i64::try_from(stale).unwrap_or(i64::MAX)),
            )
            .await?;
        Ok(DiscardReport {
            site: self.site.clone(),
            migration_id,
            discarded,
            notified,
            stale,
            summary,
        })
    }

    /// The original order of a site's held effects per source, as a release walks it. Exposed for
    /// reporting.
    ///
    /// # Errors
    /// [`Error::LedgerUnavailable`].
    pub async fn held_in_order(&self) -> Result<Vec<(String, Sequence)>> {
        let mut effects = self
            .ledger
            .list_effects(
                &self.site,
                &[EffectState::Held, EffectState::NeedsConfirmation],
            )
            .await?;
        // Request scopes are reported by their own listing, not as migration effects.
        effects.retain(|effect| !effect.migration_id.is_request_key());
        effects.sort_by(|a, b| a.source.cmp(&b.source).then(a.sequence.cmp(&b.sequence)));
        Ok(effects
            .into_iter()
            .map(|effect| (effect.source, effect.sequence))
            .collect())
    }

    pub(crate) async fn acquire_release_lease(
        &self,
    ) -> Result<Option<spot_coordinator::LockGuard>> {
        let Some(coordinator) = &self.coordinator else {
            return Ok(None);
        };
        let name = format!("hold-release:{}", self.site);
        let holder = format!("hold:{}", self.site);
        match coordinator.lock(&name, &holder, Duration::ZERO).await {
            Ok(guard) => Ok(Some(guard)),
            Err(_) => Err(Error::ReleaseInProgress {
                site: self.site.to_string(),
            }),
        }
    }
}

pub(crate) fn micros(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}
