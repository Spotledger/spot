//! Separation-of-duties analysis and periodic access reviews on a running site (WP-SEC-02.T4).
//!
//! `spot-sod` owns the conflict analysis, `spot-access-review` the campaign state machine and its
//! stores (`CampaignStore`, `SodStore`). Nothing there runs on a schedule or touches an
//! assignment. This module is the host:
//!
//! - **analysis**: the rule set and role catalogue are read through [`ReviewEnv`], `analyze` runs
//!   after every assignment change ([`ReviewChangeSink::assignments_changed`]) and nightly
//!   (`analyze_seconds`), the conflicts become the read model of the env and **new unaccepted**
//!   conflicts raise one alert document (the env returns how many). The overrides fed to the
//!   analysis are read only through `SodStore::active_overrides(tenant, now)` with the host
//!   clock, never `list_overrides`;
//! - **campaigns**: opened by the env from a spec (scope, reviewers resolved from the org data,
//!   deadline policy) by snapshotting assignments into items; a scheduler task ticks every open
//!   campaign (`tick_seconds`): escalation, auto-remove or leave open exactly as the deadline
//!   policy says, never anything else. `CampaignStore::tick` is idempotent across nodes and
//!   restarts;
//! - **decisions**: [`ReviewApi::decide`]. A reviewer decides only their own items (an escalated
//!   item only by its escalation target), never an item about themselves, never after the campaign
//!   is completed, and the campaign owner cannot decide others' items;
//! - **removals**: a `remove` decision (and a deadline removal) becomes a
//!   [`spot_access_review::ReviewAction::RemoveAssignment`]. It is first recorded as **pending**
//!   in the env's ledger, then applied through the same governed revoke path as a manual revoke
//!   ([`ReviewEnv::revoke`]: audit, snapshot rebuild, realtime revalidation), and only after the
//!   assignment is **read back as gone** is the pending record cleared. A revoke that fails
//!   leaves the item "decided, not applied": visible in [`ReviewApi::pending_removals`] and retried
//!   at every tick, never silently lost. The revoke is idempotent by the item's key.
//!
//! The permission-engine call, the throttling of refusal records and the route table's guard are
//! the host guard of WP-SEC-02.T6 (`access_review_guard.rs`): [`ReviewApi`] takes the already
//! authenticated caller id and every operation goes through [`Operation`], the list the guard
//! covers.
//!
//! # Level
//!
//! Optional. Without a [`ReviewEnv`] port the service reports `access_review_host.no_env` and the
//! site is degraded; a failing analysis or tick is logged, keeps the previous read model and
//! retries at the next period. Settings: `services.access_review`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use spot_access_review::{
    CampaignStore, Decision, Outcome, ReviewAction, ReviewItemState, ReviewReport, StoreError,
};
use spot_core::Clock;
use spot_http::api::service::BoxFuture;
use spot_sod::{
    Assignment as SodAssignment, Conflict, RoleCatalogue, RuleSet, SodOverride, SodStore,
};
use spot_tenancy::TenantId;
use tokio::sync::{Notify, watch};

use crate::services::access_review_guard::{
    AccessGuard, GuardRefusal, RefusalAudit, ReviewPermissions, active_override_records,
};
use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the assignment rules.
pub const ORDER: u32 = 1048;

/// No env port is provided by any module.
pub const CODE_NO_ENV: &str = "access_review_host.no_env";
/// The analysis or a tick could not run; the previous read model stays.
pub const CODE_RUN_FAILED: &str = "access_review_host.run_failed";
/// A removal was decided but could not be applied yet.
pub const CODE_REMOVAL_PENDING: &str = "access_review_host.removal_pending";

/// `[services.access_review]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site hosts access reviews at all.
    pub enabled: bool,
    /// The period of the nightly analysis (60 to 604,800 seconds, default one day).
    pub analyze_seconds: u64,
    /// The period of the campaign tick (10 to 86,400 seconds, default 300).
    pub tick_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            analyze_seconds: 86_400,
            tick_seconds: 300,
        }
    }
}

/// What the analysis reads.
pub struct SodInput {
    /// The tenant's SoD rules.
    pub rules: RuleSet,
    /// The role catalogue that expands roles to duties.
    pub catalogue: RoleCatalogue,
    /// Every active assignment.
    pub assignments: Vec<SodAssignment>,
}

/// A removal decided but not yet confirmed gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRemoval {
    /// The ledger key: `<campaign>:<assignment>`.
    pub key: String,
    /// The assignment to remove.
    pub assignment: String,
    /// The campaign that ordered it.
    pub campaign: String,
    /// Why, audited verbatim.
    pub reason: String,
}

/// Everything of the tenant that lives outside the review stores. Errors are operator text.
pub trait ReviewEnv: Send + Sync {
    /// The tenant this site is.
    fn tenant(&self) -> TenantId;
    /// The rules, catalogue and assignments the analysis runs over.
    fn sod_input<'a>(&'a self) -> BoxFuture<'a, std::result::Result<SodInput, String>>;
    /// Stores the conflicts as the read model and raises one alert document for the new
    /// unaccepted ones; answers how many were new.
    fn record_conflicts<'a>(
        &'a self,
        conflicts: Vec<Conflict>,
    ) -> BoxFuture<'a, std::result::Result<usize, String>>;
    /// The ids of the campaigns still open.
    fn open_campaigns<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<String>, String>>;
    /// Whether the assignment still exists (read afterwards to verify a removal).
    fn assignment_exists<'a>(
        &'a self,
        assignment: &'a str,
    ) -> BoxFuture<'a, std::result::Result<bool, String>>;
    /// Removes the assignment through the governed revoke path (audit, session snapshot
    /// rebuild, realtime revalidation). Idempotent by `key`.
    fn revoke<'a>(
        &'a self,
        key: &'a str,
        assignment: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
    /// Durably records a decided removal before it is applied.
    fn record_pending<'a>(
        &'a self,
        removal: PendingRemoval,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
    /// Every removal still pending.
    fn pending<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<PendingRemoval>, String>>;
    /// Clears a pending removal once the assignment is confirmed gone.
    fn clear_pending<'a>(&'a self, key: &'a str) -> BoxFuture<'a, std::result::Result<(), String>>;
}

/// What a writer of assignments calls so the analysis runs now.
pub trait ReviewChangeSink: Send + Sync {
    /// An assignment was granted, changed or ended.
    fn assignments_changed(&self);
}

/// Why a review operation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewRefusal {
    /// No such campaign or item for this tenant.
    NotFound,
    /// The caller is not the item's reviewer (or its escalation target).
    NotReviewer,
    /// The item is about the caller.
    SelfReview,
    /// The campaign is completed.
    Closed,
    /// The item's version moved; read it again.
    Conflict,
    /// The state machine refused (already decided, a note is owed).
    Rejected,
    /// A store could not answer.
    Unavailable,
    /// The host guard refused (engine error or timeout, no role snapshot, permission not held).
    Guarded(GuardRefusal),
}

/// The operations of the review API. Each is guarded by name (see the guard of T6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// [`ReviewApi::decide`].
    Decide,
    /// [`ReviewApi::report`].
    Report,
    /// [`ReviewApi::pending_removals`].
    PendingRemovals,
}

/// Every operation, for the guard's route-table test.
pub const OPERATIONS: [Operation; 3] = [
    Operation::Decide,
    Operation::Report,
    Operation::PendingRemovals,
];

/// The review operations over the stores.
pub trait ReviewApi: Send + Sync {
    /// `caller` decides the item. `expected_version` is the item's version the caller read.
    fn decide<'a>(
        &'a self,
        caller: &'a str,
        campaign: &'a str,
        item: &'a str,
        decision: Decision,
        expected_version: u64,
    ) -> BoxFuture<'a, std::result::Result<Outcome, ReviewRefusal>>;
    /// The campaign's report.
    fn report<'a>(
        &'a self,
        caller: &'a str,
        campaign: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ReviewReport, ReviewRefusal>>;
    /// The removals decided but not yet confirmed gone.
    fn pending_removals<'a>(
        &'a self,
        caller: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Vec<PendingRemoval>, ReviewRefusal>>;
}

/// Maps a store error to a refusal without carrying its text.
fn refusal_of(error: &StoreError) -> ReviewRefusal {
    match error {
        StoreError::NotFound => ReviewRefusal::NotFound,
        StoreError::VersionConflict => ReviewRefusal::Conflict,
        StoreError::Review(_) | StoreError::InputInvalid(_) | StoreError::AlreadyExists => {
            ReviewRefusal::Rejected
        }
        _ => ReviewRefusal::Unavailable,
    }
}

/// The host.
pub struct ReviewHost {
    env: Arc<dyn ReviewEnv>,
    campaigns: Arc<dyn CampaignStore>,
    sod: Arc<dyn SodStore>,
    guard: Arc<AccessGuard>,
    clock: Arc<dyn Clock>,
    wake: Arc<Notify>,
    /// Set when a change notice woke the loop: the next pass analyses at once.
    woken: AtomicBool,
}

impl ReviewHost {
    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// The overrides active now, only through `active_overrides(now)` (see the guard module).
    async fn active_overrides(
        &self,
        tenant: &TenantId,
    ) -> std::result::Result<Vec<SodOverride>, String> {
        active_override_records(&*self.sod, tenant, self.now())
            .await
            .map_err(|code| format!("{CODE_RUN_FAILED}: overrides unreadable ({code})"))
    }

    /// Runs the analysis and records the conflicts. Returns the number of new unaccepted ones.
    pub async fn analyse(&self) -> std::result::Result<usize, String> {
        let input = self
            .env
            .sod_input()
            .await
            .map_err(|_| format!("{CODE_RUN_FAILED}: the analysis input could not be read"))?;
        let tenant = self.env.tenant();
        let overrides = self.active_overrides(&tenant).await?;
        let conflicts = spot_sod::analyze(
            &input.rules,
            &input.catalogue,
            &input.assignments,
            &overrides,
        );
        self.env
            .record_conflicts(conflicts)
            .await
            .map_err(|_| format!("{CODE_RUN_FAILED}: the conflicts could not be recorded"))
    }

    /// Applies the removals of an outcome: record pending first, revoke, verify gone, clear. A
    /// failure leaves the removal pending and visible.
    pub async fn apply(&self, actions: &[ReviewAction]) -> usize {
        let mut pending = 0;
        for action in actions {
            let ReviewAction::RemoveAssignment {
                assignment,
                campaign,
                reason,
            } = action;
            let removal = PendingRemoval {
                key: format!("{campaign}:{}", assignment.as_str()),
                assignment: assignment.as_str().to_owned(),
                campaign: campaign.clone(),
                reason: reason.clone(),
            };
            if self.env.record_pending(removal.clone()).await.is_err() {
                tracing::error!(
                    code = CODE_REMOVAL_PENDING,
                    "a removal could not be recorded"
                );
                pending += 1;
                continue;
            }
            if !self.finish(&removal).await {
                pending += 1;
            }
        }
        pending
    }

    /// Revokes and verifies one removal; `true` when it is confirmed gone and cleared.
    async fn finish(&self, removal: &PendingRemoval) -> bool {
        let revoked = self
            .env
            .revoke(&removal.key, &removal.assignment, &removal.reason)
            .await;
        // Applied removals are verified by reading the assignment afterwards, not trusted.
        let gone = match revoked {
            Ok(()) => matches!(
                self.env.assignment_exists(&removal.assignment).await,
                Ok(false)
            ),
            Err(_) => false,
        };
        if gone && self.env.clear_pending(&removal.key).await.is_ok() {
            return true;
        }
        tracing::warn!(code = CODE_REMOVAL_PENDING, campaign = %removal.campaign, "a removal is decided, not applied");
        false
    }

    /// Ticks every open campaign and retries the pending removals.
    pub async fn tick(&self) -> std::result::Result<(), String> {
        self.guard.flush().await;
        let tenant = self.env.tenant();
        let open =
            self.env.open_campaigns().await.map_err(|_| {
                format!("{CODE_RUN_FAILED}: the open campaigns could not be listed")
            })?;
        for campaign in open {
            match self.campaigns.tick(&tenant, &campaign, self.now()).await {
                Ok(outcome) => {
                    self.apply(&outcome.actions).await;
                }
                Err(_) => tracing::warn!(code = CODE_RUN_FAILED, "a campaign tick failed"),
            }
        }
        if let Ok(pending) = self.env.pending().await {
            for removal in pending {
                self.finish(&removal).await;
            }
        }
        Ok(())
    }
}

impl ReviewApi for ReviewHost {
    fn decide<'a>(
        &'a self,
        caller: &'a str,
        campaign: &'a str,
        item: &'a str,
        decision: Decision,
        expected_version: u64,
    ) -> BoxFuture<'a, std::result::Result<Outcome, ReviewRefusal>> {
        Box::pin(async move {
            self.guard
                .authorise(caller, Operation::Decide)
                .await
                .map_err(ReviewRefusal::Guarded)?;
            let tenant = self.env.tenant();
            let stored = self
                .campaigns
                .load(&tenant, campaign)
                .await
                .map_err(|e| refusal_of(&e))?;
            let c = &stored.campaign;
            if c.is_completed() {
                return Err(ReviewRefusal::Closed);
            }
            let index = c
                .items()
                .iter()
                .position(|i| i.id == item)
                .ok_or(ReviewRefusal::NotFound)?;
            let row = &c.items()[index];
            if row.user == caller {
                return Err(ReviewRefusal::SelfReview);
            }
            // Whose item it is: the reviewer, or only the escalation target once escalated.
            let allowed = match c.item_states().get(index) {
                Some(ReviewItemState::Pending {
                    escalated_to: Some(to),
                }) => to == caller,
                Some(ReviewItemState::Pending { escalated_to: None }) => row.reviewer == caller,
                // Already decided: the store answers with its own refusal.
                _ => row.reviewer == caller,
            };
            if !allowed {
                return Err(ReviewRefusal::NotReviewer);
            }
            let outcome = self
                .campaigns
                .decide(
                    &tenant,
                    campaign,
                    item,
                    caller,
                    decision,
                    expected_version,
                    self.now(),
                )
                .await
                .map_err(|e| refusal_of(&e))?;
            // The decision is durable; the removal is applied or stays visible as pending.
            self.apply(&outcome.actions).await;
            Ok(outcome)
        })
    }

    fn report<'a>(
        &'a self,
        caller: &'a str,
        campaign: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ReviewReport, ReviewRefusal>> {
        Box::pin(async move {
            self.guard
                .authorise(caller, Operation::Report)
                .await
                .map_err(ReviewRefusal::Guarded)?;
            let tenant = self.env.tenant();
            let stored = self
                .campaigns
                .load(&tenant, campaign)
                .await
                .map_err(|e| refusal_of(&e))?;
            Ok(stored.campaign.report())
        })
    }

    fn pending_removals<'a>(
        &'a self,
        caller: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Vec<PendingRemoval>, ReviewRefusal>> {
        Box::pin(async move {
            self.guard
                .authorise(caller, Operation::PendingRemovals)
                .await
                .map_err(ReviewRefusal::Guarded)?;
            self.env
                .pending()
                .await
                .map_err(|_| ReviewRefusal::Unavailable)
        })
    }
}

struct Sink(Arc<Notify>);

impl ReviewChangeSink for Sink {
    fn assignments_changed(&self) {
        self.0.notify_one();
    }
}

struct ReviewService {
    host: Arc<ReviewHost>,
    clock: Arc<dyn Clock>,
    analyze: Duration,
    tick: Duration,
}

impl Service for ReviewService {
    fn name(&self) -> &'static str {
        "access_review"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.host
                .env
                .sod_input()
                .await
                .map(|_| ())
                .map_err(|_| format!("{CODE_RUN_FAILED}: the SoD rules could not be read"))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut last_analysis = i64::MIN;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                let now = self.clock.now_utc().unix_timestamp();
                let changed = self.host.woken.swap(false, Ordering::SeqCst);
                if changed || now.saturating_sub(last_analysis) >= self.analyze.as_secs() as i64 {
                    last_analysis = now;
                    if let Err(reason) = self.host.analyse().await {
                        tracing::warn!(reason = %reason, "SoD analysis failed");
                    }
                }
                if let Err(reason) = self.host.tick().await {
                    tracing::warn!(reason = %reason, "access review tick failed");
                }
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = self.host.wake.notified() => self.host.woken.store(true, Ordering::SeqCst),
                    () = tokio::time::sleep(self.tick) => {}
                }
            }
        })
    }
}

/// Registers the access review hosting.
///
/// # Errors
///
/// A setting that does not decode or is out of range.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("access_review")?;
    if !settings.enabled {
        return Ok(());
    }
    if !(60..=604_800).contains(&settings.analyze_seconds)
        || !(10..=86_400).contains(&settings.tick_seconds)
    {
        return Err(Error(
            "services.access_review: analyze_seconds is 60 to 604800 and tick_seconds 10 to 86400"
                .to_owned(),
        ));
    }
    let (Some(env), Some(campaigns), Some(sod), Some(engine), Some(audit)) = (
        ctx.port::<dyn ReviewEnv>(),
        ctx.port::<dyn CampaignStore>(),
        ctx.port::<dyn SodStore>(),
        ctx.port::<dyn ReviewPermissions>(),
        ctx.port::<dyn RefusalAudit>(),
    ) else {
        if ctx.assembling() {
            ctx.add(Idle::new("access_review", Level::Optional, || -> CheckFuture<'static> {
                Box::pin(async {
                    Err(format!("{CODE_NO_ENV}: no module of this site provides the review env, the stores, the permission engine and the refusal audit"))
                })
            }));
        }
        return Ok(());
    };
    let clock = Arc::clone(ctx.clock());
    let wake = Arc::new(Notify::new());
    let guard = Arc::new(AccessGuard::new(
        engine,
        audit,
        Arc::clone(&clock),
        &env.tenant(),
    ));
    let host = Arc::new(ReviewHost {
        env,
        campaigns,
        sod,
        guard,
        clock: Arc::clone(&clock),
        wake: Arc::clone(&wake),
        woken: AtomicBool::new(false),
    });
    ctx.provide::<dyn ReviewApi>(Arc::clone(&host) as Arc<dyn ReviewApi>)?;
    ctx.provide::<dyn ReviewChangeSink>(Arc::new(Sink(wake)))?;
    ctx.add(Arc::new(ReviewService {
        host,
        clock,
        analyze: Duration::from_secs(settings.analyze_seconds),
        tick: Duration::from_secs(settings.tick_seconds),
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_setting_refuses_unknown_keys_and_every_operation_is_listed() {
        let bad: std::result::Result<Settings, _> = toml::from_str("tick = 5");
        assert!(bad.is_err());
        let ok: std::result::Result<Settings, _> = toml::from_str("tick_seconds = 30");
        assert_eq!(ok.unwrap().tick_seconds, 30);
        assert_eq!(OPERATIONS.len(), 3);
    }

    use std::sync::atomic::AtomicUsize;

    use spot_access_review::{AssignmentRef, MemoryCampaignStore};
    use spot_sod::MemorySodStore;

    /// An env whose revoke can fail and whose assignment disappears only when revoked.
    struct Env {
        fail_revoke: AtomicBool,
        exists: AtomicBool,
        pending: std::sync::Mutex<Vec<PendingRemoval>>,
        revokes: AtomicUsize,
    }

    #[rustfmt::skip]
    impl ReviewEnv for Env {
        fn tenant(&self) -> TenantId { TenantId::new_v7(1_800_000_000_000, [7; 10]) }
        fn sod_input<'a>(&'a self) -> BoxFuture<'a, std::result::Result<SodInput, String>> { Box::pin(async { Err("n/a".into()) }) }
        fn record_conflicts<'a>(&'a self, _c: Vec<Conflict>) -> BoxFuture<'a, std::result::Result<usize, String>> { Box::pin(async { Ok(0) }) }
        fn open_campaigns<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<String>, String>> { Box::pin(async { Ok(vec![]) }) }
        fn assignment_exists<'a>(&'a self, _a: &'a str) -> BoxFuture<'a, std::result::Result<bool, String>> {
            Box::pin(async move { Ok(self.exists.load(Ordering::SeqCst)) })
        }
        fn revoke<'a>(&'a self, _k: &'a str, _a: &'a str, _r: &'a str) -> BoxFuture<'a, std::result::Result<(), String>> {
            Box::pin(async move {
                self.revokes.fetch_add(1, Ordering::SeqCst);
                if self.fail_revoke.load(Ordering::SeqCst) { return Err("down".into()); }
                self.exists.store(false, Ordering::SeqCst);
                Ok(())
            })
        }
        fn record_pending<'a>(&'a self, r: PendingRemoval) -> BoxFuture<'a, std::result::Result<(), String>> {
            Box::pin(async move { self.pending.lock().unwrap().push(r); Ok(()) })
        }
        fn pending<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<PendingRemoval>, String>> {
            Box::pin(async move { Ok(self.pending.lock().unwrap().clone()) })
        }
        fn clear_pending<'a>(&'a self, k: &'a str) -> BoxFuture<'a, std::result::Result<(), String>> {
            Box::pin(async move { self.pending.lock().unwrap().retain(|p| p.key != k); Ok(()) })
        }
    }

    fn host(env: &Arc<Env>) -> ReviewHost {
        ReviewHost {
            env: env.clone(),
            campaigns: Arc::new(MemoryCampaignStore::new()),
            sod: Arc::new(MemorySodStore::new()),
            guard: Arc::new(crate::services::access_review_guard::tests_support::allow_all()),
            clock: Arc::new(spot_core::ManualClock::new(
                time::macros::datetime!(2026-11-12 10:00 UTC),
            )),
            wake: Arc::new(Notify::new()),
            woken: AtomicBool::new(false),
        }
    }

    #[tokio::test]
    async fn a_removal_is_pending_until_the_assignment_is_read_back_as_gone() {
        let env = Arc::new(Env {
            fail_revoke: AtomicBool::new(true),
            exists: AtomicBool::new(true),
            pending: Default::default(),
            revokes: AtomicUsize::new(0),
        });
        let host = host(&env);
        let action = ReviewAction::RemoveAssignment {
            assignment: AssignmentRef::new("a1"),
            campaign: "c1".into(),
            reason: "not needed".into(),
        };
        // The revoke fails: the removal stays pending and visible, never lost.
        assert_eq!(host.apply(std::slice::from_ref(&action)).await, 1);
        assert_eq!(host.pending_removals("admin").await.unwrap().len(), 1);
        // The next tick retries it; once the revoke works and the read-back says gone, it clears.
        env.fail_revoke.store(false, Ordering::SeqCst);
        host.tick().await.unwrap();
        assert!(host.pending_removals("admin").await.unwrap().is_empty());
        assert!(!env.exists.load(Ordering::SeqCst));
    }

    #[test]
    fn a_store_error_maps_to_a_refusal_without_text() {
        assert_eq!(refusal_of(&StoreError::NotFound), ReviewRefusal::NotFound);
        assert_eq!(
            refusal_of(&StoreError::VersionConflict),
            ReviewRefusal::Conflict
        );
        assert_eq!(
            refusal_of(&StoreError::Unavailable),
            ReviewRefusal::Unavailable
        );
    }
}
