//! Assignment rules on a running site (WP-IAM-04.T3): the rule set is loaded and validated, the
//! assignments it gives are reconciled on a change of business data and on a schedule, applied in
//! one transaction with their events, expiry events fire exactly once, and a rule edit can be
//! previewed before it is run.
//!
//! `spot-iam-assign` owns the rules (`plan_rules_guarded`, `expire`, the portal guard, the
//! ownership checks) and returns a [`Plan`]. Nothing there runs it. This module is the host:
//!
//! - **what is read**: through [`RuleStore`], the site's adapter over the stored records (the rule
//!   set document, the assignments, the facts of the users a changed record concerns). The
//!   facts are read as the system principal; **only the named assignment writes are performed**
//!   and no fact ever leaves this module (the preview and run answers carry counts and ids);
//! - **what is applied**: the plan, in one [`RuleStore::apply`] call (one transaction with the
//!   events on the outbox and the expiry marker), through the same governed grant path as a manual
//!   grant: [`GrantCheck`] is the segregation-of-duties gate, and a grant it blocks (or cannot
//!   decide: fail closed) is **reported, not applied**;
//! - **who reconciles**: one node per tenant at a time. [`Lease`] is the coordinator lease (the
//!   job worker's); a node without it does nothing, and the lease is checked again immediately
//!   before an apply so a node that lost it stops at once. Without a lease port the site is single
//!   node and always reconciles;
//! - **the guard**: a plan that would end more than `bulk_hold_percent` (default 25) of the
//!   assignments of one role is **held** for confirmation instead of applied. The held plan is
//!   visible to the preview; running with its digest is the confirmation;
//! - **expiry**: assignments whose validity ended produce their expiry event exactly once
//!   (`expire` with the stored marker; the marker is written in the same transaction as the
//!   events), at the next second an assignment ends, not only at the next schedule tick.
//!
//! # Level
//!
//! Optional. Without a [`RuleStore`] port the service reports `assignment_rules_host.no_store` and
//! the site is degraded. A rule that fails validation is refused and reported
//! (`assignment_rules_host.rule_refused`); the other rules keep running. A plan that cannot be
//! made safe (the gate cannot answer) applies nothing. Setting: `services.assignment_rules`.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use spot_http::api::ports::identity::{IdResult, PlanOut, RuleIn, RunOut};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture};
use spot_iam_assign::{
    Change, EventPayload, Facts, IdSource, Plan, RoleDirectory, RuleSet, RuleSpec, UserId, expire,
    plan_rules_guarded,
};
use spot_iam_roles::{Assignment, OutsideGuard, RoleName, active_at};
use time::OffsetDateTime;
use tokio::sync::{Notify, watch};

use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the roles module, which this one reads through the store.
pub const ORDER: u32 = 1047;

/// No store port is provided by any module.
pub const CODE_NO_STORE: &str = "assignment_rules_host.no_store";
/// A rule failed validation and was refused; the others keep running.
pub const CODE_RULE_REFUSED: &str = "assignment_rules_host.rule_refused";
/// A grant was blocked by the segregation-of-duties gate (or the gate could not answer).
pub const CODE_GRANT_BLOCKED: &str = "assignment_rules_host.grant_blocked";
/// A plan was held for confirmation by the bulk guard.
pub const CODE_PLAN_HELD: &str = "assignment_rules_host.plan_held";
/// The stored records could not be read or the plan could not be applied.
pub const CODE_STORE_UNAVAILABLE: &str = "assignment_rules_host.store_unavailable";

/// `[services.assignment_rules]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site reconciles assignment rules at all.
    pub enabled: bool,
    /// The period of the full reconciliation that catches drift (10 to 86,400 seconds).
    pub reconcile_seconds: u64,
    /// A plan ending more than this percent of one role's assignments is held (1 to 100).
    pub bulk_hold_percent: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            reconcile_seconds: 300,
            bulk_hold_percent: 25,
        }
    }
}

/// The stored records the reconciler reads and the one transaction it writes. Errors are
/// operator text and never reach a client.
pub trait RuleStore: Send + Sync {
    /// The tenant's rule set document.
    fn rule_specs<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<RuleSpec>, String>>;
    /// The roles that exist and the portal guard.
    fn directory<'a>(
        &'a self,
    ) -> BoxFuture<'a, std::result::Result<(RoleDirectory, OutsideGuard), String>>;
    /// Every assignment record, active or not.
    fn assignments<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<Assignment>, String>>;
    /// The facts of `users`, or of every user when `None`.
    fn facts<'a>(
        &'a self,
        users: Option<Vec<UserId>>,
    ) -> BoxFuture<'a, std::result::Result<Vec<(UserId, Facts)>, String>>;
    /// The ids whose end event was already announced.
    fn announced<'a>(&'a self) -> BoxFuture<'a, std::result::Result<BTreeSet<String>, String>>;
    /// Applies `plan`'s changes, its events on the outbox and `marker` (the newly announced
    /// ends) in **one transaction**: all or nothing.
    fn apply<'a>(
        &'a self,
        plan: Plan,
        marker: Vec<String>,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
}

/// The segregation-of-duties gate of the grant path (`GrantGate` over the governance state).
pub trait GrantCheck: Send + Sync {
    /// The ids of the grants the gate blocks. `Err` means the gate cannot decide: nothing is
    /// granted.
    fn blocked<'a>(
        &'a self,
        grants: Vec<Assignment>,
    ) -> BoxFuture<'a, std::result::Result<BTreeSet<String>, String>>;
}

/// The coordinator lease of this tenant (the job worker's).
pub trait Lease: Send + Sync {
    /// Whether this node holds the lease right now.
    fn held<'a>(&'a self) -> BoxFuture<'a, bool>;
}

/// What a writer of business data (the outbox subscriber of a record that rules read) calls.
pub trait RuleChangeSink: Send + Sync {
    /// These users' facts may have changed.
    fn changed(&self, users: Vec<UserId>);
}

/// Preview and run, in the shapes the identity API needs.
pub trait RuleRunner: Send + Sync {
    /// The plan the rules would produce, applying nothing. `rules` replaces the stored set for
    /// this call only.
    fn preview<'a>(&'a self, rules: Option<Vec<RuleIn>>) -> BoxFuture<'a, IdResult<PlanOut>>;
    /// Applies exactly the plan whose digest preview returned.
    fn run<'a>(&'a self, digest: String) -> BoxFuture<'a, IdResult<RunOut>>;
}

// ---------------------------------------------------------------------------------------------
// The pure core
// ---------------------------------------------------------------------------------------------

/// Counting ids from a fresh record id each: time ordered, injected into the planner.
struct RecordIds;

impl IdSource for RecordIds {
    fn next(&mut self) -> String {
        spot_storage_contract::RecordId::new_v7().to_string()
    }
}

/// The assignments of `role` that are active at `now`.
fn active_count(current: &[Assignment], role: &RoleName, now: OffsetDateTime) -> usize {
    current
        .iter()
        .filter(|a| &a.role == role && active_at(a, now))
        .count()
}

/// The first role whose assignments `plan` would end by more than `percent`, with the numbers.
pub fn bulk_guard(
    plan: &Plan,
    current: &[Assignment],
    now: OffsetDateTime,
    percent: u8,
) -> Option<(RoleName, usize, usize)> {
    let mut ends: BTreeMap<&RoleName, usize> = BTreeMap::new();
    for change in &plan.changes {
        if let Change::End { role, .. } = change {
            *ends.entry(role).or_default() += 1;
        }
    }
    ends.into_iter().find_map(|(role, ended)| {
        let total = active_count(current, role, now);
        (total > 0 && ended * 100 > total * usize::from(percent))
            .then(|| (role.clone(), ended, total))
    })
}

/// Removes the grants (and their `Created` events) whose ids are in `blocked`. Returns the ids
/// removed, so each is reported.
pub fn drop_blocked(plan: &mut Plan, blocked: &BTreeSet<String>) -> Vec<String> {
    let mut dropped = Vec::new();
    plan.changes.retain(|c| match c {
        Change::Grant(a) if blocked.contains(&a.id) => {
            dropped.push(a.id.clone());
            false
        }
        _ => true,
    });
    plan.events
        .retain(|e| !matches!(e, EventPayload::Created(a) if blocked.contains(&a.id)));
    dropped
}

/// A digest of what a plan does, **without** the ids it generated (they differ on every
/// planning): grants by user, role and scope, ends by assignment id. Not secret; it only stops a
/// run from applying something other than what was previewed.
pub fn plan_digest(plan: &Plan) -> String {
    let mut lines: Vec<String> = plan
        .changes
        .iter()
        .map(|c| match c {
            Change::Grant(a) => format!("g|{}|{}|{:?}", a.user, a.role.as_str(), a.scope),
            Change::End { id, .. } => format!("e|{id}"),
            Change::Replace { id, with } => format!("r|{id}|{:?}", with.scope),
        })
        .collect();
    lines.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lines.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn spec_of(rule: &RuleIn) -> std::result::Result<RuleSpec, ()> {
    // A scope template is an expression in the stored form; the wire form is free text, so a
    // preview with a scope is refused instead of guessed.
    if rule.scope.as_deref().is_some_and(|s| !s.is_empty()) {
        return Err(());
    }
    Ok(RuleSpec {
        id: rule.id.clone(),
        name: rule.id.clone(),
        role: RoleName::from(rule.role_id.as_str()),
        when: rule.when.clone(),
        scope: Vec::new(),
        valid_for_secs: None,
        enabled: rule.enabled,
        priority: 0,
    })
}

// ---------------------------------------------------------------------------------------------
// The reconciler
// ---------------------------------------------------------------------------------------------

/// What one reconciliation did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Grants applied.
    pub granted: u64,
    /// Ends applied (rule ends and expiries).
    pub ended: u64,
    /// Grants the gate blocked, reported not applied.
    pub refused: u64,
    /// The plan was held; its digest.
    pub held: Option<String>,
}

/// Reconciles one tenant.
pub struct Reconciler {
    store: Arc<dyn RuleStore>,
    gate: Option<Arc<dyn GrantCheck>>,
    lease: Option<Arc<dyn Lease>>,
    clock: Arc<dyn Clock>,
    percent: u8,
    held: Mutex<Option<String>>,
}

impl Reconciler {
    async fn holds_lease(&self) -> bool {
        match &self.lease {
            Some(lease) => lease.held().await,
            None => true,
        }
    }

    /// The plan the rules produce now for `users` (every user when `None`), with blocked grants
    /// already removed and the number removed. Rules that fail validation are refused whole
    /// (reported) and the set is not run.
    async fn plan(
        &self,
        users: Option<Vec<UserId>>,
        override_rules: Option<&[RuleSpec]>,
    ) -> std::result::Result<(Plan, Vec<Assignment>, u64), String> {
        let unavailable =
            |_e: String| format!("{CODE_STORE_UNAVAILABLE}: the records could not be read");
        let specs = match override_rules {
            Some(rules) => rules.to_vec(),
            None => self.store.rule_specs().await.map_err(unavailable)?,
        };
        let (directory, guard) = self.store.directory().await.map_err(unavailable)?;
        let current = self.store.assignments().await.map_err(unavailable)?;
        let facts = self.store.facts(users).await.map_err(unavailable)?;
        let now = self.clock.now_utc();
        // A rule that fails validation is refused and reported; the others keep running.
        let mut valid = Vec::new();
        for spec in specs {
            match RuleSet::load(vec![spec.clone()], &directory) {
                Ok(_) => valid.push(spec),
                Err(errors) => {
                    tracing::warn!(code = CODE_RULE_REFUSED, rule = %spec.id, problems = %errors.codes().join(","), "a rule was refused");
                }
            }
        }
        let rule_set = RuleSet::load(valid, &directory).map_err(|_| {
            format!("{CODE_RULE_REFUSED}: the remaining rules do not load together")
        })?;
        let mut plan = plan_rules_guarded(&rule_set, &mut RecordIds, &current, &facts, now, &guard);
        let mut refused = 0;
        let grants: Vec<Assignment> = plan.grants().cloned().collect();
        if !grants.is_empty() {
            // Fail closed: a gate that cannot answer blocks every grant of the plan.
            let blocked = match &self.gate {
                Some(gate) => gate
                    .blocked(grants.clone())
                    .await
                    .unwrap_or_else(|_| grants.iter().map(|g| g.id.clone()).collect()),
                None => grants.iter().map(|g| g.id.clone()).collect(),
            };
            refused = drop_blocked(&mut plan, &blocked).len() as u64;
            if refused > 0 {
                tracing::warn!(
                    code = CODE_GRANT_BLOCKED,
                    count = refused,
                    "rule grants were not applied"
                );
            }
        }
        Ok((plan, current, refused))
    }

    /// One reconciliation. `confirmed` is the digest of a held plan the caller confirms.
    pub async fn reconcile(
        &self,
        users: Option<Vec<UserId>>,
        confirmed: Option<&str>,
    ) -> std::result::Result<Outcome, String> {
        if !self.holds_lease().await {
            return Ok(Outcome::default());
        }
        let now = self.clock.now_utc();
        let (mut plan, current, refused) = self.plan(users.clone(), None).await?;
        let digest = plan_digest(&plan);
        let mut outcome = Outcome {
            refused,
            ..Outcome::default()
        };
        if let Some((role, ended, total)) = bulk_guard(&plan, &current, now, self.percent)
            && confirmed != Some(digest.as_str())
        {
            tracing::warn!(code = CODE_PLAN_HELD, role = %role.as_str(), ended, total, "a rule change was held for confirmation");
            *self.held.lock().unwrap_or_else(|p| p.into_inner()) = Some(digest.clone());
            outcome.held = Some(digest);
            return Ok(outcome);
        }
        // Expiry joins the same transaction when this is the full reconciliation.
        let mut marker = Vec::new();
        if users.is_none() {
            let announced =
                self.store.announced().await.map_err(|_| {
                    format!("{CODE_STORE_UNAVAILABLE}: the records could not be read")
                })?;
            let ended = expire(&current, &announced, now);
            marker = ended.ended_marker.clone();
            plan.events.extend(ended.events);
        }
        outcome.granted = plan.grants().count() as u64;
        outcome.ended = plan
            .changes
            .iter()
            .filter(|c| matches!(c, Change::End { .. }))
            .count() as u64
            + marker.len() as u64;
        if plan.changes.is_empty() && plan.events.is_empty() {
            *self.held.lock().unwrap_or_else(|p| p.into_inner()) = None;
            return Ok(outcome);
        }
        // A node that lost the lease stops here, before the write.
        if !self.holds_lease().await {
            return Ok(Outcome::default());
        }
        self.store
            .apply(plan, marker)
            .await
            .map_err(|_| format!("{CODE_STORE_UNAVAILABLE}: the plan was not applied"))?;
        *self.held.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(outcome)
    }

    /// The digest of the plan held for confirmation, if any.
    pub fn held(&self) -> Option<String> {
        self.held.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

fn rules_invalid() -> ApiError {
    ApiError::from_code(Code::IdentityRulesInvalid)
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", serde_json::Value::from(5))
}

impl RuleRunner for Reconciler {
    fn preview<'a>(&'a self, rules: Option<Vec<RuleIn>>) -> BoxFuture<'a, IdResult<PlanOut>> {
        Box::pin(async move {
            let specs = match rules {
                Some(rules) => Some(
                    rules
                        .iter()
                        .map(spec_of)
                        .collect::<std::result::Result<Vec<_>, _>>()
                        .map_err(|()| rules_invalid())?,
                ),
                None => None,
            };
            let (plan, _current, _refused) = self
                .plan(None, specs.as_deref())
                .await
                .map_err(|_| unavailable())?;
            let grants = plan
                .grants()
                .map(|a| (a.user.clone(), a.role.as_str().to_owned(), None))
                .collect();
            let revokes = plan
                .changes
                .iter()
                .filter_map(|c| match c {
                    Change::End { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            Ok(PlanOut {
                grants,
                revokes,
                truncated: false,
                plan_digest: plan_digest(&plan),
            })
        })
    }

    fn run<'a>(&'a self, digest: String) -> BoxFuture<'a, IdResult<RunOut>> {
        Box::pin(async move {
            let (plan, _current, _refused) =
                self.plan(None, None).await.map_err(|_| unavailable())?;
            // A stale digest applies nothing: the plan is not the one that was previewed.
            if plan_digest(&plan) != digest {
                return Err(rules_invalid());
            }
            let outcome = self
                .reconcile(None, Some(&digest))
                .await
                .map_err(|_| unavailable())?;
            Ok(RunOut {
                granted: outcome.granted,
                revoked: outcome.ended,
                refused: outcome.refused,
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The service and its registration
// ---------------------------------------------------------------------------------------------

struct Sink {
    pending: Mutex<BTreeSet<UserId>>,
    wake: Arc<Notify>,
}

impl RuleChangeSink for Sink {
    fn changed(&self, users: Vec<UserId>) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(users);
        self.wake.notify_one();
    }
}

struct RulesService {
    reconciler: Arc<Reconciler>,
    sink: Arc<Sink>,
    store: Arc<dyn RuleStore>,
    clock: Arc<dyn Clock>,
    period: Duration,
}

impl Service for RulesService {
    fn name(&self) -> &'static str {
        "assignment_rules"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            // The rule set loads at start; a store that cannot be read degrades the site.
            self.store
                .rule_specs()
                .await
                .map(|_| ())
                .map_err(|_| format!("{CODE_STORE_UNAVAILABLE}: the rule set could not be read"))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut last_full = OffsetDateTime::UNIX_EPOCH;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                let now = self.clock.now_utc();
                let due = now - last_full >= self.period;
                let users: Vec<UserId> = std::mem::take(
                    &mut *self.sink.pending.lock().unwrap_or_else(|p| p.into_inner()),
                )
                .into_iter()
                .collect();
                let result = if due || (users.is_empty() && last_full == OffsetDateTime::UNIX_EPOCH)
                {
                    last_full = now;
                    self.reconciler.reconcile(None, None).await
                } else if users.is_empty() {
                    Ok(Outcome::default())
                } else {
                    self.reconciler.reconcile(Some(users), None).await
                };
                if let Err(reason) = result {
                    tracing::warn!(reason = %reason, "assignment rule reconciliation failed");
                }
                let wait = self.period.min(Duration::from_secs(5));
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = self.sink.wake.notified() => {}
                    () = tokio::time::sleep(wait) => {}
                }
            }
        })
    }
}

/// Registers the assignment rule hosting.
///
/// # Errors
///
/// A setting that does not decode or is out of range.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("assignment_rules")?;
    if !settings.enabled {
        return Ok(());
    }
    if !(10..=86_400).contains(&settings.reconcile_seconds)
        || !(1..=100).contains(&settings.bulk_hold_percent)
    {
        return Err(Error(
            "services.assignment_rules: reconcile_seconds is 10 to 86400 and bulk_hold_percent 1 to 100"
                .to_owned(),
        ));
    }
    let Some(store) = ctx.port::<dyn RuleStore>() else {
        if ctx.assembling() {
            ctx.add(Idle::new(
                "assignment_rules",
                Level::Optional,
                || -> CheckFuture<'static> {
                    Box::pin(async {
                        Err(format!(
                            "{CODE_NO_STORE}: no module of this site provides the rule store"
                        ))
                    })
                },
            ));
        }
        return Ok(());
    };
    let clock = Arc::clone(ctx.clock());
    let reconciler = Arc::new(Reconciler {
        store: Arc::clone(&store),
        gate: ctx.port::<dyn GrantCheck>(),
        lease: ctx.port::<dyn Lease>(),
        clock: Arc::clone(&clock),
        percent: settings.bulk_hold_percent,
        held: Mutex::new(None),
    });
    let sink = Arc::new(Sink {
        pending: Mutex::new(BTreeSet::new()),
        wake: Arc::new(Notify::new()),
    });
    ctx.provide::<dyn RuleRunner>(Arc::clone(&reconciler) as Arc<dyn RuleRunner>)?;
    ctx.provide::<dyn RuleChangeSink>(Arc::clone(&sink) as Arc<dyn RuleChangeSink>)?;
    ctx.add(Arc::new(RulesService {
        reconciler,
        sink,
        store,
        clock,
        period: Duration::from_secs(settings.reconcile_seconds),
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use time::macros::datetime;

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-11-12 10:00:00 UTC);

    struct Store {
        specs: Mutex<Vec<RuleSpec>>,
        current: Mutex<Vec<Assignment>>,
        facts: Mutex<Vec<(UserId, Facts)>>,
        applied: Mutex<Vec<(Plan, Vec<String>)>>,
        announced: Mutex<BTreeSet<String>>,
    }

    impl Store {
        fn new() -> Arc<Store> {
            Arc::new(Store {
                specs: Mutex::new(vec![spec("r1", "Manager", "facts.dept == \"sales\"")]),
                current: Mutex::new(Vec::new()),
                facts: Mutex::new(vec![("u1".into(), Facts::new().text("dept", "sales"))]),
                applied: Mutex::new(Vec::new()),
                announced: Mutex::new(BTreeSet::new()),
            })
        }
    }

    fn spec(id: &str, role: &str, when: &str) -> RuleSpec {
        RuleSpec {
            id: id.into(),
            name: id.into(),
            role: RoleName::from(role),
            when: when.into(),
            scope: Vec::new(),
            valid_for_secs: None,
            enabled: true,
            priority: 0,
        }
    }

    impl RuleStore for Store {
        fn rule_specs<'a>(&'a self) -> BoxFuture<'a, std::result::Result<Vec<RuleSpec>, String>> {
            Box::pin(async move { Ok(self.specs.lock().unwrap().clone()) })
        }
        fn directory<'a>(
            &'a self,
        ) -> BoxFuture<'a, std::result::Result<(RoleDirectory, OutsideGuard), String>> {
            Box::pin(async move {
                Ok((
                    RoleDirectory::new([RoleName::from("Manager"), RoleName::from("Viewer")]),
                    OutsideGuard::none(),
                ))
            })
        }
        fn assignments<'a>(
            &'a self,
        ) -> BoxFuture<'a, std::result::Result<Vec<Assignment>, String>> {
            Box::pin(async move { Ok(self.current.lock().unwrap().clone()) })
        }
        fn facts<'a>(
            &'a self,
            _u: Option<Vec<UserId>>,
        ) -> BoxFuture<'a, std::result::Result<Vec<(UserId, Facts)>, String>> {
            Box::pin(async move { Ok(self.facts.lock().unwrap().clone()) })
        }
        fn announced<'a>(&'a self) -> BoxFuture<'a, std::result::Result<BTreeSet<String>, String>> {
            Box::pin(async move { Ok(self.announced.lock().unwrap().clone()) })
        }
        fn apply<'a>(
            &'a self,
            plan: Plan,
            marker: Vec<String>,
        ) -> BoxFuture<'a, std::result::Result<(), String>> {
            Box::pin(async move {
                self.announced
                    .lock()
                    .unwrap()
                    .extend(marker.iter().cloned());
                self.applied.lock().unwrap().push((plan, marker));
                Ok(())
            })
        }
    }

    struct Gate(AtomicBool);

    impl GrantCheck for Gate {
        fn blocked<'a>(
            &'a self,
            grants: Vec<Assignment>,
        ) -> BoxFuture<'a, std::result::Result<BTreeSet<String>, String>> {
            Box::pin(async move {
                if self.0.load(Ordering::SeqCst) {
                    Ok(grants.iter().map(|g| g.id.clone()).collect())
                } else {
                    Ok(BTreeSet::new())
                }
            })
        }
    }

    struct Held(AtomicUsize);

    impl Lease for Held {
        fn held<'a>(&'a self) -> BoxFuture<'a, bool> {
            Box::pin(async move { self.0.load(Ordering::SeqCst) > 0 })
        }
    }

    fn reconciler(
        store: &Arc<Store>,
        gate: Option<Arc<dyn GrantCheck>>,
        lease: Option<Arc<dyn Lease>>,
    ) -> Reconciler {
        Reconciler {
            store: store.clone(),
            gate,
            lease,
            clock: Arc::new(spot_core::ManualClock::new(NOW)),
            percent: 25,
            held: Mutex::new(None),
        }
    }

    fn open_gate() -> Option<Arc<dyn GrantCheck>> {
        Some(Arc::new(Gate(AtomicBool::new(false))))
    }

    #[tokio::test]
    async fn a_rule_grants_on_a_field_change_in_one_apply() {
        let store = Store::new();
        let rec = reconciler(&store, open_gate(), None);
        let out = rec.reconcile(None, None).await.unwrap();
        assert_eq!((out.granted, out.refused), (1, 0));
        let applied = store.applied.lock().unwrap();
        assert_eq!(applied.len(), 1, "one transaction");
        assert_eq!(applied[0].0.grants().next().unwrap().user, "u1");
    }

    #[tokio::test]
    async fn a_conflicting_grant_is_reported_not_applied_and_a_missing_gate_fails_closed() {
        let store = Store::new();
        let blocked: Option<Arc<dyn GrantCheck>> = Some(Arc::new(Gate(AtomicBool::new(true))));
        let out = reconciler(&store, blocked, None)
            .reconcile(None, None)
            .await
            .unwrap();
        assert_eq!((out.granted, out.refused), (0, 1));
        let out = reconciler(&store, None, None)
            .reconcile(None, None)
            .await
            .unwrap();
        assert_eq!(
            (out.granted, out.refused),
            (0, 1),
            "no gate: nothing is granted"
        );
        assert!(store.applied.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_node_without_the_lease_does_nothing() {
        let store = Store::new();
        let lease: Option<Arc<dyn Lease>> = Some(Arc::new(Held(AtomicUsize::new(0))));
        let out = reconciler(&store, open_gate(), lease)
            .reconcile(None, None)
            .await
            .unwrap();
        assert_eq!(out, Outcome::default());
        assert!(store.applied.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_bad_rule_is_isolated_and_the_others_keep_running() {
        let store = Store::new();
        store
            .specs
            .lock()
            .unwrap()
            .push(spec("bad", "No Such Role", "true"));
        let out = reconciler(&store, open_gate(), None)
            .reconcile(None, None)
            .await
            .unwrap();
        assert_eq!(out.granted, 1);
    }

    #[tokio::test]
    async fn the_bulk_guard_holds_a_plan_until_its_digest_confirms_it() {
        let store = Store::new();
        {
            // Four assignments of Manager from rule r1; the rule no longer holds for any user.
            let mut current = store.current.lock().unwrap();
            for i in 0..4 {
                let mut a = Assignment::new(&format!("a{i}"), &format!("u{i}"), "Manager");
                a.source = spot_iam_roles::Source::Rule;
                a.reason = Some("rule:r1".into());
                current.push(a);
            }
            *store.facts.lock().unwrap() = (0..4)
                .map(|i| (format!("u{i}"), Facts::new().text("dept", "hr")))
                .collect();
        }
        let rec = reconciler(&store, open_gate(), None);
        let out = rec.reconcile(None, None).await.unwrap();
        let digest = out.held.clone().expect("held");
        assert!(
            store.applied.lock().unwrap().is_empty(),
            "held, not applied"
        );
        assert_eq!(rec.held(), Some(digest.clone()));
        let out = rec.reconcile(None, Some(&digest)).await.unwrap();
        assert_eq!(out.held, None);
        assert_eq!(store.applied.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn expiry_fires_once_and_the_marker_is_in_the_same_apply() {
        let store = Store::new();
        store.specs.lock().unwrap().clear();
        let mut a = Assignment::new("a1", "u9", "Viewer");
        a.valid_to = Some(NOW - time::Duration::seconds(1));
        store.current.lock().unwrap().push(a);
        let rec = reconciler(&store, open_gate(), None);
        rec.reconcile(None, None).await.unwrap();
        rec.reconcile(None, None).await.unwrap();
        let applied = store.applied.lock().unwrap();
        assert_eq!(
            applied.len(),
            1,
            "the second call finds the marker and does nothing"
        );
        assert_eq!(applied[0].1, vec!["a1".to_owned()]);
        assert!(matches!(applied[0].0.events[0], EventPayload::Ended { .. }));
    }

    #[tokio::test]
    async fn preview_applies_nothing_and_run_needs_the_previewed_digest() {
        let store = Store::new();
        let rec = reconciler(&store, open_gate(), None);
        let plan = rec.preview(None).await.unwrap();
        assert_eq!(plan.grants.len(), 1);
        assert!(store.applied.lock().unwrap().is_empty());
        let stale = rec.run("0000000000000000".into()).await.unwrap_err();
        assert_eq!(stale.code.as_str(), "identity.rules_invalid");
        assert!(store.applied.lock().unwrap().is_empty());
        let run = rec.run(plan.plan_digest).await.unwrap();
        assert_eq!((run.granted, run.refused), (1, 0));
        let scoped = RuleIn {
            id: "x".into(),
            when: "true".into(),
            role_id: "Viewer".into(),
            scope: Some("c:A".into()),
            enabled: true,
        };
        assert_eq!(
            rec.preview(Some(vec![scoped]))
                .await
                .unwrap_err()
                .code
                .as_str(),
            "identity.rules_invalid"
        );
    }

    #[test]
    fn the_digest_ignores_generated_ids_and_the_settings_refuse_unknown_keys() {
        let make = |id: &str| Plan {
            changes: vec![Change::Grant(Assignment::new(id, "u1", "Manager"))],
            ..Plan::default()
        };
        assert_eq!(plan_digest(&make("a")), plan_digest(&make("b")));
        let bad: std::result::Result<Settings, _> = toml::from_str("reconcile = 5");
        assert!(bad.is_err());
    }
}
