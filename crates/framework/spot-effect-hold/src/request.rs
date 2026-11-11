//! Request-scoped holds (ADR-080 section 6.7): the migration machinery, keyed by a request.
//!
//! A process may mark commit points. External effects produced in the region before one are held
//! under the request's scope and released at the commit point; if the request is undone or
//! cancelled first they are discarded and nothing left the building.
//!
//! What differs from a migration hold:
//!
//! - There is **no unverified state**. An effect is held because its request is inside a held
//!   region, never because a state is unreadable. If the ledger cannot record the effect,
//!   admission **fails** ([`Error::LedgerUnavailable`]): the effect is neither sent nor recorded
//!   and the caller must not proceed as if it were held.
//! - Many requests hold the same class at once; the scope is the key, not the site.
//! - **A migration hold wins.** While the site is held for a migration a request release
//!   dispatches nothing and changes nothing ([`RequestReleaseReport::deferred_by`]); the commit
//!   point is retried after the migration is verified. An unreadable site state is an error, not
//!   an open site.
//! - A class that is never held (fiscal transmission) passes straight through and the admission
//!   says so ([`RequestAdmission::Proceed`] with `irreversible`): a point of no return.
//! - Release walks the **original order** of the request (sequence), not per source, because a
//!   request's effects are one story; a failed dispatch stops the walk so a later effect never
//!   overtakes an earlier one.
//! - A discard never claims an effect that may have left: one a crashed release left in
//!   `Releasing` is kept and counted as in flight. A dispatch error that may have sent also
//!   leaves the row `Releasing`, never `Held`.
//!
//! # Single flight
//!
//! Every effect transition is a compare-and-set against the state the caller read
//! ([`LedgerStore::transition_effect`](crate::LedgerStore::transition_effect)), so two nodes
//! cannot both release, or one release while another discards, the same effect: the loser gets
//! `effect_hold.stale`, counts it and does not act. On top of that a request's transitions are
//! serialised here: every admit, release and discard of one request takes that
//! request's **in-process async mutex, always** (and, when a coordinator is set, the site's
//! release lease as well). Two commits of one request, or a commit racing `end`, therefore run
//! one after the other and each re-reads the rows after taking the mutex: an effect is either
//! dispatched or discarded, never both, and dispatched once per commit. **This holds inside
//! one process only.** With several nodes serving one site, a coordinator MUST be configured
//! ([`HoldService::with_coordinator`]); without one a second node can still race.
//!
//! # Host obligations
//!
//! This layer has no principal and no authority check: a [`RequestId`] is the whole key. The
//! host must authorise who may commit or end a request before calling in.
//!
//! No payload, message body or dispatcher text is stored: a failed dispatch records the error's
//! stable code only.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use spot_core::SiteId;

use crate::audit::{AuditEvent, AuditKind};
use crate::class::OnDiscard;
use crate::dispatch::Notice;
use crate::effect::{EffectId, EffectRecord, EffectRef, EffectState, NewEffect};
use crate::error::{Error, Result};
use crate::hold::{HoldState, MigrationId};
use crate::scope::{HoldScope, RequestId};
use crate::service::{HoldService, ReleasePlan, micros};

/// The longest effect kind, key or source accepted for a request hold.
pub const MAX_FIELD_LEN: usize = 256;
/// The longest discard reason accepted.
pub const MAX_REASON_LEN: usize = 256;
/// The most effects one request may hold at once. The next admission is refused with
/// `effect_hold.invalid_input`.
pub const MAX_HELD_PER_REQUEST: usize = 1_000;
/// How many ended requests are remembered (oldest forgotten first) to refuse a late admission.
const MAX_REMEMBERED_ENDED: usize = 10_000;
/// When the lock map grows past this, entries nobody holds are dropped.
const LOCK_PRUNE_AT: usize = 1_024;

/// The per-request in-process state of a [`HoldService`]: the single-flight mutexes and the
/// requests that have ended.
#[derive(Default)]
pub(crate) struct RequestGuards {
    locks: Mutex<HashMap<RequestId, Arc<tokio::sync::Mutex<()>>>>,
    ended: Mutex<(HashSet<RequestId>, VecDeque<RequestId>)>,
}

impl RequestGuards {
    fn is_ended(&self, request: &RequestId) -> bool {
        self.ended
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
            .contains(request)
    }

    fn mark_ended(&self, request: &RequestId) {
        let mut ended = self
            .ended
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ended.0.insert(request.clone()) {
            ended.1.push_back(request.clone());
            while ended.1.len() > MAX_REMEMBERED_ENDED {
                if let Some(old) = ended.1.pop_front() {
                    ended.0.remove(&old);
                }
            }
        }
    }

    async fn lock(&self, request: &RequestId) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if locks.len() > LOCK_PRUNE_AT {
                locks.retain(|_, mutex| Arc::strong_count(mutex) > 1);
            }
            Arc::clone(locks.entry(request.clone()).or_default())
        };
        mutex.lock_owned().await
    }
}

/// The ticket of an effect held under a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestTicket {
    /// The site.
    pub site: SiteId,
    /// The request.
    pub request: RequestId,
    /// The ledger id of the recorded effect.
    pub effect_id: EffectId,
}

/// What [`HoldService::admit_request`] decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestAdmission {
    /// The effect may go out now.
    Proceed {
        /// The class is never held, so the step that causes it is a point of no return.
        irreversible: bool,
    },
    /// The effect was recorded and deferred to the request's commit point.
    Held(RequestTicket),
}

/// What a request release did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestReleaseReport {
    /// The request.
    pub request: RequestId,
    /// Effects dispatched, in dispatch order.
    pub dispatched: Vec<EffectId>,
    /// Stale effects that wait for confirmation.
    pub needs_confirmation: Vec<EffectId>,
    /// The migration whose hold deferred the release, when one is live. Nothing was dispatched.
    pub deferred_by: Option<MigrationId>,
    /// Effects another node acted on first (`effect_hold.stale`): not dispatched here.
    pub stale: Vec<EffectId>,
    /// Whether every selected effect is dispatched.
    pub completed: bool,
}

/// What a request discard did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestDiscardReport {
    /// The request.
    pub request: RequestId,
    /// How many effects were discarded.
    pub discarded: u64,
    /// How many raised a task for a person.
    pub notified: u64,
    /// Effects another node acted on first (`effect_hold.stale`): not discarded here.
    pub stale: u64,
    /// Effects a crashed release left in flight: they may have left, so they are kept and the
    /// caller treats them as sent.
    pub in_flight: Vec<EffectId>,
}

fn bounded(what: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(Error::InvalidInput {
            what: what.to_owned(),
            reason: format!("must be 1 to {max} printable characters"),
        });
    }
    Ok(())
}

const PENDING: [EffectState; 3] = [
    EffectState::Held,
    EffectState::Releasing,
    EffectState::NeedsConfirmation,
];

impl HoldService {
    /// Admits an effect produced inside a held region of `request`.
    ///
    /// A never-held class proceeds at once with `irreversible` set. Any other class is recorded
    /// under the request's scope and deferred, keeping its idempotency key and its place in the
    /// original order.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] for an effect of another site, an unbounded kind, key or source,
    /// a request that has already ended, or a request already holding
    /// [`MAX_HELD_PER_REQUEST`] effects; [`Error::LedgerUnavailable`] (or the ledger's own error)
    /// when the effect could not be recorded: it is then neither sent nor held.
    pub async fn admit_request(
        &self,
        request: &RequestId,
        effect: &EffectRef,
    ) -> Result<RequestAdmission> {
        if effect.site != self.site {
            return Err(Error::InvalidInput {
                what: "effect site".to_owned(),
                reason: "is not this service's site".to_owned(),
            });
        }
        bounded("effect kind", &effect.kind, MAX_FIELD_LEN)?;
        bounded("effect key", &effect.key, MAX_FIELD_LEN)?;
        bounded("effect source", &effect.source, MAX_FIELD_LEN)?;
        if self.policies.policy_for(&effect.class).never_hold {
            return Ok(RequestAdmission::Proceed { irreversible: true });
        }
        let _single = self.requests.lock(request).await;
        if self.requests.is_ended(request) {
            return Err(Error::InvalidInput {
                what: "request".to_owned(),
                reason: "has ended; no effect can be held for it".to_owned(),
            });
        }
        if self.request_pending(request).await?.len() >= MAX_HELD_PER_REQUEST {
            return Err(Error::InvalidInput {
                what: "held effects".to_owned(),
                reason: format!("a request may hold at most {MAX_HELD_PER_REQUEST} effects"),
            });
        }
        let scope = HoldScope::Request(request.clone());
        let new = NewEffect {
            effect: effect.clone(),
            migration_id: scope.ledger_key(),
        };
        let effect_id = self.ledger.append_effect(new).await?;
        Ok(RequestAdmission::Held(RequestTicket {
            site: self.site.clone(),
            request: request.clone(),
            effect_id,
        }))
    }

    /// The effects `request` still holds, in their original order.
    ///
    /// # Errors
    /// [`Error::LedgerUnavailable`].
    pub async fn request_pending(&self, request: &RequestId) -> Result<Vec<EffectRecord>> {
        let key = HoldScope::Request(request.clone()).ledger_key();
        let mut effects = self.ledger.list_effects(&self.site, &PENDING).await?;
        effects.retain(|effect| effect.migration_id == key);
        effects.sort_by_key(|effect| effect.sequence);
        Ok(effects)
    }

    /// Releases the effects `request` holds, at a commit point, in the original order, each with
    /// its original idempotency key. `sources` limits the release to the effects of those
    /// ordering sources (the region the commit point ends); `None` selects every pending effect.
    ///
    /// Idempotent: a repeat after a crash dispatches only what is not yet `Dispatched`, and an
    /// effect left `Releasing` is re-dispatched under the same key.
    ///
    /// # Errors
    /// [`Error::StateUnknown`] when the site's hold state cannot be read,
    /// [`Error::ReleaseInProgress`], [`Error::DispatchFailed`] (the walk stops there; nothing
    /// later is dispatched) and [`Error::AuditFailed`].
    pub async fn release_request(
        &self,
        request: &RequestId,
        plan: ReleasePlan,
        sources: Option<&BTreeSet<String>>,
        now: i64,
    ) -> Result<RequestReleaseReport> {
        let _single = self.requests.lock(request).await;
        let _lease = self.acquire_release_lease().await?;
        let mut report = RequestReleaseReport {
            request: request.clone(),
            dispatched: Vec::new(),
            needs_confirmation: Vec::new(),
            deferred_by: None,
            stale: Vec::new(),
            completed: false,
        };
        let scope_key = HoldScope::Request(request.clone()).ledger_key();
        if let Some(migration) = self.live_migration().await? {
            report.deferred_by = Some(migration);
            self.record_release(&scope_key, &report, 0, now).await?;
            return Ok(report);
        }
        let mut effects = self.request_pending(request).await?;
        if let Some(sources) = sources {
            effects.retain(|effect| sources.contains(&effect.source));
        }
        for effect in effects {
            // A migration can begin mid-release: look again before every effect.
            if let Some(migration) = self.live_migration().await? {
                report.deferred_by = Some(migration);
                self.record_release(&scope_key, &report, 0, now).await?;
                return Ok(report);
            }
            let policy = self.policies.policy_for(&effect.class);
            let stale = policy
                .max_hold
                .is_some_and(|max| now.saturating_sub(effect.created_at) > micros(max));
            let confirmed = plan.confirmation.contains(&effect.key);
            if (stale || effect.state == EffectState::NeedsConfirmation) && !confirmed {
                if effect.state != EffectState::NeedsConfirmation
                    && !self
                        .transition(
                            &effect,
                            effect.state,
                            EffectState::NeedsConfirmation,
                            Some("older than max_hold; needs confirmation".to_owned()),
                        )
                        .await?
                {
                    report.stale.push(effect.id);
                    continue;
                }
                report.needs_confirmation.push(effect.id);
                continue;
            }
            // Take the effect before dispatching it; a stale result means another node acted.
            if !self
                .transition(&effect, effect.state, EffectState::Releasing, None)
                .await?
            {
                report.stale.push(effect.id);
                continue;
            }
            if let Err(error) = self.dispatcher.dispatch(&effect).await {
                // The send may have happened: the row stays `Releasing` (re-driven under the same
                // key by the next release, never discarded by `end`).
                self.transition(
                    &effect,
                    EffectState::Releasing,
                    EffectState::Releasing,
                    Some(format!("dispatch failed: {}", error.code())),
                )
                .await?;
                // The dispatch error is the one to report; a failed audit write must not hide it.
                let _ = self.record_release(&scope_key, &report, 1, now).await;
                return Err(Error::DispatchFailed {
                    key: effect.key,
                    class: effect.class.key(),
                    reason: error.code().to_owned(),
                });
            }
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
            } else {
                report.stale.push(effect.id);
            }
        }
        report.completed = report.needs_confirmation.is_empty() && report.stale.is_empty();
        self.record_release(&scope_key, &report, 0, now).await?;
        Ok(report)
    }

    /// The migration whose hold is live on the site, if any.
    ///
    /// # Errors
    /// [`Error::StateUnknown`] when the state cannot be read: the site is then treated as held.
    async fn live_migration(&self) -> Result<Option<MigrationId>> {
        match self.ledger.get_hold(&self.site).await {
            Ok(HoldState::Held { migration_id, .. }) => Ok(Some(migration_id)),
            Ok(_) => Ok(None),
            Err(_) => Err(Error::StateUnknown {
                site: self.site.to_string(),
            }),
        }
    }

    /// Records a release attempt: counts only, including failures and deferrals.
    async fn record_release(
        &self,
        scope_key: &MigrationId,
        report: &RequestReleaseReport,
        failed: i64,
        now: i64,
    ) -> Result<()> {
        self.audit
            .record(
                AuditEvent::new(
                    AuditKind::Release,
                    self.site.clone(),
                    Some(scope_key.clone()),
                    now,
                )
                .with_count(
                    "dispatched",
                    i64::try_from(report.dispatched.len()).unwrap_or(i64::MAX),
                )
                .with_count(
                    "needs_confirmation",
                    i64::try_from(report.needs_confirmation.len()).unwrap_or(i64::MAX),
                )
                .with_count(
                    "stale",
                    i64::try_from(report.stale.len()).unwrap_or(i64::MAX),
                )
                .with_count("failed", failed)
                .with_count("deferred", i64::from(report.deferred_by.is_some())),
            )
            .await
    }

    /// Discards the effects `request` still holds, when the request ends by cancel or undo before
    /// the commit point: each is marked `Discarded` and never dispatched, and a class with
    /// [`OnDiscard::DropAndNotify`] raises a task for a person.
    ///
    /// An effect a crashed release left `Releasing` may have left the building, so it is neither
    /// discarded nor replayed here: it is reported in [`RequestDiscardReport::in_flight`].
    ///
    /// # Errors
    /// [`Error::InvalidInput`] for an unbounded reason, [`Error::ReleaseInProgress`],
    /// [`Error::LedgerUnavailable`] and [`Error::AuditFailed`].
    pub async fn discard_request(
        &self,
        request: &RequestId,
        reason: &str,
        now: i64,
    ) -> Result<RequestDiscardReport> {
        bounded("discard reason", reason, MAX_REASON_LEN)?;
        let _single = self.requests.lock(request).await;
        let _lease = self.acquire_release_lease().await?;
        // From here no admission for this request is accepted, even if the walk below fails.
        self.requests.mark_ended(request);
        let mut report = RequestDiscardReport {
            request: request.clone(),
            discarded: 0,
            notified: 0,
            stale: 0,
            in_flight: Vec::new(),
        };
        for effect in self.request_pending(request).await? {
            if effect.state == EffectState::Releasing {
                report.in_flight.push(effect.id);
                continue;
            }
            // Only an effect still in the state read is dropped: one a concurrent release took
            // is stale and left alone.
            if !self
                .transition(
                    &effect,
                    effect.state,
                    EffectState::Discarded,
                    Some(reason.to_owned()),
                )
                .await?
            {
                report.stale += 1;
                continue;
            }
            report.discarded += 1;
            if self.policies.policy_for(&effect.class).on_discard == OnDiscard::DropAndNotify {
                self.notifier
                    .notify(Notice {
                        site: self.site.clone(),
                        class: effect.class.key(),
                        kind: effect.kind.clone(),
                        key: effect.key.clone(),
                        reason: reason.to_owned(),
                    })
                    .await?;
                report.notified += 1;
            }
        }
        let scope_key = HoldScope::Request(request.clone()).ledger_key();
        self.audit
            .record(
                AuditEvent::new(AuditKind::Discard, self.site.clone(), Some(scope_key), now)
                    .with_count(
                        "discarded",
                        i64::try_from(report.discarded).unwrap_or(i64::MAX),
                    )
                    .with_count(
                        "notified",
                        i64::try_from(report.notified).unwrap_or(i64::MAX),
                    )
                    .with_count("stale", i64::try_from(report.stale).unwrap_or(i64::MAX)),
            )
            .await?;
        Ok(report)
    }
}
