//! The grant gate on a running server (WP-IAM-08.T7, wired by WP-IAM-08.T12): `spot-iam-governance`'s
//! `GrantGate` as the one function every role-grant path calls, provided as the port [`GrantGuard`].
//!
//! - **State** is rebuilt for every decision from the live directory ([`DirectorySource`]: the active
//!   assignments in one read-only transaction and the portal guard of WP-IAM-11.T16), the SoD rules and
//!   role catalogue the installed apps ship (`<app>/sod_rules.toml`, `<app>/role_catalogue.toml`, read at
//!   start), the delegation and approval policy from settings, and the recorded SoD overrides from the
//!   [`GovernanceStore`]. If any of it cannot be read the answer is `Unavailable`: **every grant is
//!   refused**, none is allowed by default.
//! - **Outcomes**: `Allowed`; `Blocked` (codes and rule ids only, never other users); `NeedsOverride`
//!   (the rule ids the grantor may override with a reason and a mitigating control); `Pending` (an
//!   approval Case was launched). When approval is required and no launcher is provided, the outcome
//!   is `Unavailable("governance_approval_host.unavailable")`: never granted, never auto-approved.
//! - **Overrides** are recorded in the store when the gate allows a grant on the strength of a complete
//!   submission (`record_overrides`), so the same (rule, user, scope) is covered afterwards and the record
//!   survives a restart. A record that cannot be stored refuses the grant.
//! - **Approval** lives in the store as one record per grant request (the request, the crate's
//!   [`GrantApproval`] and a status `pending -> approved -> granted`, or `ended`). It is decided only
//!   through [`GrantGuard::decide_approval`], by the authenticated user the caller passes
//!   (`decide_with_roles` enforces the named approver and excludes the grantee), with a compare-and-swap,
//!   so two nodes cannot both decide it. `approved` stays until the grant is made and
//!   [`GrantGuard::mark_granted`] ends it, so a crash between the decision and the grant is resumed, never
//!   lost, and a repeat of a finished completion is [`ApprovalResult::AlreadyGranted`]. A request retried
//!   while pending answers the same Case and launches nothing.
//! - **After approval** [`GrantGuard::recheck_approved`] runs the gate again without the approval step, so
//!   an approval never overrides a conflict, and [`GrantExecutor`] (provided by the module that makes
//!   assignments) makes the grant, idempotently by the request id.
//! - Every decision writes one line to `spot::audit::governance` (request id, outcome, codes; no names).
//!
//! Settings `[services.governance]`: `enabled` (true), `delegation` and `approval` as the crate's
//! policies. Level: optional; without readable SoD files and a catalogue, no rule applies and only the
//! delegation and approval policies decide.
//!
//! **The store.** [`GovernanceStore`] is a port: a module that provides `dyn GovernanceStore` before this
//! one (a durable table, ORDER below 1070) is used; otherwise the process-local [`MemoryGovernanceStore`]
//! is, and the module says so in the log: with it a restart loses pending approvals (the grant is never
//! made: fail closed) and recorded overrides.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use spot_core::Clock;
use spot_http::api::service::BoxFuture;
use spot_iam_governance::approval::{ApprovalDecision, ApprovalState};
use spot_iam_governance::{
    ApprovalPolicy, DelegationPolicy, GovernanceState, GrantApproval, GrantDecision, GrantGate,
    GrantRequest, ProcessLauncher, SodOverrideRecord,
};
use spot_iam_roles::{Assignment, OutsideGuard};
use spot_sod::{RoleCatalogue, RuleSet};
use spot_storage_contract::{Catalog, Isolation, StorageBackend, Transaction, TxOptions};

use crate::Result;
use crate::live::SiteLive;
use crate::services::scim_store::{load, nm};
use crate::services::{Idle, Level, SiteCtx};

/// After the identity modules (1050, 1060), before the question service (1080).
pub const ORDER: u32 = 1070;

const AUDIT: &str = "spot::audit::governance";
const RULES_FILE: &str = "sod_rules.toml";
const CATALOGUE_FILE: &str = "role_catalogue.toml";

/// The directory, the rules or the store could not be read: every grant is refused.
const STATE_UNAVAILABLE: &str = "governance_host.state_unavailable";
/// A grant needs approval and the approval process cannot be started.
const APPROVAL_UNAVAILABLE: &str = "governance_approval_host.unavailable";
/// The request is not (or no longer) waiting for a decision.
const NOT_PENDING: &str = "iam_governance.approval_not_pending";
/// The request cannot be decided as given.
const REQUEST_INVALID: &str = "iam_governance.request_invalid";

const REQUEST_PREFIX: &str = "request:";
const OVERRIDE_PREFIX: &str = "override:";

/// The launcher the approval process is started through. Provided by the process hosting
/// (WP-IAM-08.T11); absent means approval cannot start.
pub type Launcher = dyn ProcessLauncher + Send + Sync;

/// Where the launcher is set once the process host (which starts later than this module) exists.
/// Provided as a port; the approval hosting (WP-IAM-08.T11) fills it. Empty means approval cannot
/// start (fail closed).
#[derive(Default)]
pub struct LauncherSlot(std::sync::OnceLock<Arc<Launcher>>);

impl LauncherSlot {
    /// Sets the launcher; a second call is ignored.
    pub fn set(&self, launcher: Arc<Launcher>) {
        let _ = self.0.set(launcher);
    }

    fn get(&self) -> Option<Arc<Launcher>> {
        self.0.get().cloned()
    }
}

/// What the gate decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantOutcome {
    /// Grant it.
    Allowed,
    /// Refused; the stable codes and SoD rule ids, in order.
    Blocked(Vec<String>),
    /// Refused unless the grantor submits a reason and a mitigating control; the rule ids.
    NeedsOverride(Vec<String>),
    /// An approval Case was launched; the grant waits for it.
    Pending {
        /// The grant request id.
        request: String,
        /// The Case id.
        case: String,
    },
    /// Nothing can be decided now: refused, fail closed. The stable code.
    Unavailable(&'static str),
}

/// A decided approval: the request to turn into an assignment, or why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalResult {
    /// Approved: the request now to be granted (the caller re-checks it with
    /// [`GrantGuard::recheck_approved`], makes the grant and calls [`GrantGuard::mark_granted`]).
    /// A repeat by the same approver while the grant is not yet made answers this again.
    Approved(Box<GrantRequest>),
    /// Rejected or expired: no grant.
    Ended,
    /// The grant was already made for this request: nothing to do.
    AlreadyGranted,
    /// Refused: the stable code (`iam_governance.not_the_approver`, `approver_is_grantee`, ...).
    Refused(String),
}

/// The one function every grant path calls.
pub trait GrantGuard: Send + Sync {
    /// Decides `request` against the live state.
    fn decide<'a>(&'a self, request: GrantRequest) -> BoxFuture<'a, GrantOutcome>;
    /// Decides the pending approval of `request` as the authenticated user `by`.
    fn decide_approval<'a>(
        &'a self,
        request: &'a str,
        by: &'a str,
        approve: bool,
        reason: Option<String>,
    ) -> BoxFuture<'a, ApprovalResult>;
    /// Decides an already approved `request` again: the same checks as [`decide`](Self::decide) except
    /// that the approval step is not asked for twice. An approval never overrides a conflict.
    fn recheck_approved<'a>(&'a self, request: GrantRequest) -> BoxFuture<'a, GrantOutcome>;
    /// Records that the grant of `request` was made, ending its approval. `false` when it could not be
    /// recorded (it stays approved; making the grant again is safe, it is idempotent).
    fn mark_granted<'a>(&'a self, request: &'a str) -> BoxFuture<'a, bool>;
}

/// Makes the assignment an allowed grant becomes. Provided by the module that owns the assignment
/// writes (identity administration); the approval completion calls it once the gate allowed the
/// approved request again.
pub trait GrantExecutor: Send + Sync {
    /// Grants `request` on behalf of `approver`, who decided it under a step-up. It must be idempotent:
    /// the assignment id is [`GrantRequestId::assignment_id`](spot_iam_governance::GrantRequestId), the
    /// same for every call, so a retry after a crash never writes a second assignment. `Err` is a stable
    /// code, and nothing was granted.
    fn execute<'a>(
        &'a self,
        request: &'a GrantRequest,
        approver: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
}

/// The store could not be read or written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreFailure;

/// What the store answers.
pub type StoreResult<T> = std::result::Result<T, StoreFailure>;

/// Where pending approvals and SoD overrides are kept: text values under text keys. The gate owns the
/// content (JSON); a store only keeps it, and its one write is a compare-and-swap.
pub trait GovernanceStore: Send + Sync {
    /// The value under `key`, if any.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>>;
    /// Writes `new` under `key` when the current value is `expected` (`None`: no value). `Ok(false)`
    /// when another writer got there first; nothing is written then.
    fn compare_put<'a>(
        &'a self,
        key: &'a str,
        expected: Option<&'a str>,
        new: &'a str,
    ) -> BoxFuture<'a, StoreResult<bool>>;
    /// Every value whose key starts with `prefix`, in key order.
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, StoreResult<Vec<String>>>;
}

/// The process-local [`GovernanceStore`]: lost at restart, not shared by nodes.
#[derive(Debug, Default)]
pub struct MemoryGovernanceStore {
    values: Mutex<BTreeMap<String, String>>,
}

impl GovernanceStore for MemoryGovernanceStore {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>> {
        Box::pin(async move {
            let values = self.values.lock().map_err(|_| StoreFailure)?;
            Ok(values.get(key).cloned())
        })
    }

    fn compare_put<'a>(
        &'a self,
        key: &'a str,
        expected: Option<&'a str>,
        new: &'a str,
    ) -> BoxFuture<'a, StoreResult<bool>> {
        Box::pin(async move {
            let mut values = self.values.lock().map_err(|_| StoreFailure)?;
            if values.get(key).map(String::as_str) != expected {
                return Ok(false);
            }
            values.insert(key.to_owned(), new.to_owned());
            Ok(true)
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, StoreResult<Vec<String>>> {
        Box::pin(async move {
            let values = self.values.lock().map_err(|_| StoreFailure)?;
            Ok(values
                .range(prefix.to_owned()..)
                .take_while(|(key, _)| key.starts_with(prefix))
                .map(|(_, value)| value.clone())
                .collect())
        })
    }
}

/// What the live directory says: the portal guard and the active assignments.
pub struct Directory {
    /// Who is a portal user (a portal user is never given a desk role).
    pub outside: OutsideGuard,
    /// The active assignments.
    pub assignments: Vec<Assignment>,
}

/// Reads the live directory. `None` when it cannot be read: the gate then refuses every grant.
pub trait DirectorySource: Send + Sync {
    /// Reads the directory now.
    fn read<'a>(&'a self) -> BoxFuture<'a, Option<Directory>>;
}

/// The directory of a running site: one read-only transaction over its database.
struct SiteDirectory {
    site: String,
    live: Arc<SiteLive>,
    catalog: Arc<Catalog>,
}

impl DirectorySource for SiteDirectory {
    fn read<'a>(&'a self) -> BoxFuture<'a, Option<Directory>> {
        Box::pin(async move {
            let engine = self.live.engine()?;
            let options =
                TxOptions::new(nm("governance_state").ok()?, Isolation::ReadCommitted).read_only();
            let mut tx = engine
                .backend()
                .begin(Arc::clone(&self.catalog), options)
                .await
                .ok()?;
            let guard = crate::users::outside::guard_for(&mut tx).await;
            let unit = load(&self.site, &mut tx).await;
            let _ = tx.rollback().await;
            Some(Directory {
                outside: guard.ok()?,
                assignments: unit.ok()?.assignments,
            })
        })
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: Option<bool>,
    delegation: DelegationPolicy,
    approval: ApprovalPolicy,
}

/// Where a grant request's approval is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    /// Waiting for the approver.
    Pending,
    /// Approved; the grant is not yet recorded as made.
    Approved,
    /// The grant was made.
    Granted,
    /// Rejected or expired.
    Ended,
}

/// One grant request's approval as the store keeps it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    status: Status,
    request: GrantRequest,
    approval: GrantApproval,
}

impl Record {
    fn parse(text: &str) -> Option<Record> {
        serde_json::from_str(text).ok()
    }

    fn encode(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    fn case(&self) -> String {
        self.approval
            .receipt
            .as_ref()
            .map(|receipt| receipt.case_id.clone())
            .unwrap_or_default()
    }
}

fn request_key(id: &str) -> String {
    format!("{REQUEST_PREFIX}{id}")
}

fn override_key(record: &SodOverrideRecord) -> String {
    format!(
        "{OVERRIDE_PREFIX}{}|{}|{}",
        record.rule_id,
        record.user,
        record.scope.as_deref().unwrap_or("-")
    )
}

/// Runs a synchronous call that may block (the crate's launcher is synchronous) without stalling the
/// runtime: off the worker on a multi-thread runtime, in place otherwise.
fn run_blocking<T>(call: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(call)
        }
        _ => call(),
    }
}

/// Everything a [`Governance`] gate is built from.
pub struct Parts {
    /// The site name, for the audit line.
    pub site: String,
    /// The live directory.
    pub source: Arc<dyn DirectorySource>,
    /// Pending approvals and overrides.
    pub store: Arc<dyn GovernanceStore>,
    /// The injected clock: nothing here reads the wall clock.
    pub clock: Arc<dyn Clock>,
    /// The SoD rules the installed apps ship, if any.
    pub rules: Option<RuleSet>,
    /// The role catalogue the installed apps ship, if any.
    pub catalogue: Option<RoleCatalogue>,
    /// Who may grant what.
    pub delegation: DelegationPolicy,
    /// Which grants need approval.
    pub approval: ApprovalPolicy,
    /// Where the approval launcher is set.
    pub launcher: Arc<LauncherSlot>,
    /// A SoD file did not parse: no state can be built, every grant is refused.
    pub broken: bool,
}

/// The governance gate of one site.
pub struct Governance {
    site: String,
    source: Arc<dyn DirectorySource>,
    store: Arc<dyn GovernanceStore>,
    clock: Arc<dyn Clock>,
    rules: Option<RuleSet>,
    catalogue: Option<RoleCatalogue>,
    delegation: DelegationPolicy,
    approval: ApprovalPolicy,
    launcher: Arc<LauncherSlot>,
    broken: bool,
}

impl Governance {
    /// A gate over `parts`.
    #[must_use]
    pub fn from_parts(parts: Parts) -> Governance {
        Governance {
            site: parts.site,
            source: parts.source,
            store: parts.store,
            clock: parts.clock,
            rules: parts.rules,
            catalogue: parts.catalogue,
            delegation: parts.delegation,
            approval: parts.approval,
            launcher: parts.launcher,
            broken: parts.broken,
        }
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// Builds the state from the live directory and the store, or the code of why it cannot. With
    /// `approved` the approval policy is empty: the request was already approved.
    async fn state(&self, approved: bool) -> std::result::Result<GovernanceState, &'static str> {
        if self.broken {
            return Err(STATE_UNAVAILABLE);
        }
        let directory = self.source.read().await.ok_or(STATE_UNAVAILABLE)?;
        let mut state = GovernanceState::new(directory.outside);
        if let Some(rules) = &self.rules {
            state.rules = rules.clone();
        }
        if let Some(catalogue) = &self.catalogue {
            state.catalogue = catalogue.clone();
        }
        state.assignments = directory.assignments;
        state.overrides = self.overrides().await?;
        state.delegation = self.delegation;
        state.approval = if approved {
            ApprovalPolicy::none()
        } else {
            self.approval.clone()
        };
        Ok(state)
    }

    /// The recorded overrides; a record that does not parse refuses everything (fail closed).
    async fn overrides(&self) -> std::result::Result<Vec<SodOverrideRecord>, &'static str> {
        let texts = self
            .store
            .list(OVERRIDE_PREFIX)
            .await
            .map_err(|_| STATE_UNAVAILABLE)?;
        texts
            .iter()
            .map(|text| serde_json::from_str(text).map_err(|_| STATE_UNAVAILABLE))
            .collect()
    }

    fn audit(&self, request: &str, outcome: &str, codes: &[String]) {
        tracing::info!(
            target: AUDIT,
            site = %self.site,
            request,
            outcome,
            codes = %codes.join(","),
            "grant decision"
        );
    }

    fn unavailable(&self, request: &str, code: &'static str) -> GrantOutcome {
        self.audit(request, "unavailable", &[code.to_owned()]);
        GrantOutcome::Unavailable(code)
    }

    /// The decision shared by [`GrantGuard::decide`] and [`GrantGuard::recheck_approved`].
    async fn decide_with(&self, request: GrantRequest, approved: bool) -> GrantOutcome {
        let id = request.id.as_str().to_owned();
        let state = match self.state(approved).await {
            Ok(state) => state,
            Err(code) => return self.unavailable(&id, code),
        };
        let decision = match GrantGate::new().evaluate(&request, &state) {
            Ok(decision) => decision,
            Err(_) => return self.unavailable(&id, REQUEST_INVALID),
        };
        match decision {
            GrantDecision::Allow => match self.record_overrides(&request, &state).await {
                Ok(()) => {
                    self.audit(
                        &id,
                        if approved {
                            "allowed_after_approval"
                        } else {
                            "allowed"
                        },
                        &[],
                    );
                    GrantOutcome::Allowed
                }
                Err(code) => self.unavailable(&id, code),
            },
            GrantDecision::Blocked(conflicts) => {
                let codes: Vec<String> = conflicts.iter().map(|c| c.code().to_owned()).collect();
                self.audit(&id, "blocked", &codes);
                GrantOutcome::Blocked(codes)
            }
            GrantDecision::NeedsOverride(conflicts) => {
                let codes: Vec<String> = conflicts.iter().map(|c| c.code().to_owned()).collect();
                self.audit(&id, "needs_override", &codes);
                GrantOutcome::NeedsOverride(codes)
            }
            GrantDecision::NeedsApproval(spec) => self.start_approval(id, request, spec).await,
        }
    }

    /// Records the overrides a complete submission covered: the conflicts the allowed grant would
    /// otherwise have had, one record per (rule, user, scope). A record that cannot be stored refuses
    /// the grant.
    async fn record_overrides(
        &self,
        request: &GrantRequest,
        state: &GovernanceState,
    ) -> std::result::Result<(), &'static str> {
        if request.override_submission.is_none() {
            return Ok(());
        }
        let gate = GrantGate::new();
        let conflicts = gate.sod_conflicts(request, state);
        if conflicts.is_empty() {
            return Ok(());
        }
        let records = gate
            .record_overrides(request, &conflicts, self.now())
            .map_err(|_| REQUEST_INVALID)?;
        for record in records {
            let text = serde_json::to_string(&record).map_err(|_| STATE_UNAVAILABLE)?;
            // `false`: the same triple is already recorded by a racing grant; that covers this one.
            self.store
                .compare_put(&override_key(&record), None, &text)
                .await
                .map_err(|_| STATE_UNAVAILABLE)?;
        }
        Ok(())
    }

    /// Launches the approval Case and stores the request, or answers the Case already started for it.
    async fn start_approval(
        &self,
        id: String,
        request: GrantRequest,
        spec: spot_iam_governance::approval::ApprovalSpec,
    ) -> GrantOutcome {
        let key = request_key(&id);
        // A retry of a request that already has an approval answers it and launches nothing.
        match self.store.get(&key).await {
            Err(_) => return self.unavailable(&id, STATE_UNAVAILABLE),
            Ok(Some(text)) => {
                return match Record::parse(&text) {
                    Some(record) if record.status == Status::Pending => {
                        self.audit(&id, "pending", &[]);
                        GrantOutcome::Pending {
                            request: id,
                            case: record.case(),
                        }
                    }
                    Some(_) => {
                        let codes = vec![NOT_PENDING.to_owned()];
                        self.audit(&id, "blocked", &codes);
                        GrantOutcome::Blocked(codes)
                    }
                    None => self.unavailable(&id, STATE_UNAVAILABLE),
                };
            }
            Ok(None) => {}
        }
        let Some(launcher) = self.launcher.get() else {
            return self.unavailable(&id, APPROVAL_UNAVAILABLE);
        };
        let mut approval = GrantApproval::new(spec);
        let now = self.now();
        // The launcher is synchronous (the trait of the crate); it must not block long.
        let launched = run_blocking(|| approval.begin(&*launcher, now).cloned());
        if launched.is_err() {
            return self.unavailable(&id, APPROVAL_UNAVAILABLE);
        }
        let record = Record {
            status: Status::Pending,
            request,
            approval,
        };
        let Some(text) = record.encode() else {
            return self.unavailable(&id, STATE_UNAVAILABLE);
        };
        match self.store.compare_put(&key, None, &text).await {
            Ok(true) => {
                self.audit(&id, "pending", &[]);
                GrantOutcome::Pending {
                    request: id,
                    case: record.case(),
                }
            }
            // Another node stored an approval for this request between our read and write (the launch is
            // keyed by the request id, so it is the same Case): answer theirs.
            Ok(false) => match self.store.get(&key).await {
                Ok(Some(other)) => match Record::parse(&other) {
                    Some(record) => GrantOutcome::Pending {
                        request: id,
                        case: record.case(),
                    },
                    None => self.unavailable(&id, STATE_UNAVAILABLE),
                },
                _ => self.unavailable(&id, STATE_UNAVAILABLE),
            },
            Err(_) => self.unavailable(&id, STATE_UNAVAILABLE),
        }
    }

    /// Sets the status of a stored record by compare-and-swap, `Ok(false)` when it is not as expected.
    async fn set_status(&self, request: &str, from: Status, to: Status) -> StoreResult<bool> {
        let key = request_key(request);
        for _ in 0..3 {
            let Some(text) = self.store.get(&key).await? else {
                return Ok(false);
            };
            let Some(mut record) = Record::parse(&text) else {
                return Err(StoreFailure);
            };
            if record.status != from {
                return Ok(false);
            }
            record.status = to;
            let new = record.encode().ok_or(StoreFailure)?;
            if self.store.compare_put(&key, Some(&text), &new).await? {
                return Ok(true);
            }
        }
        Err(StoreFailure)
    }
}

impl GrantGuard for Governance {
    fn decide<'a>(&'a self, request: GrantRequest) -> BoxFuture<'a, GrantOutcome> {
        Box::pin(async move { self.decide_with(request, false).await })
    }

    fn recheck_approved<'a>(&'a self, request: GrantRequest) -> BoxFuture<'a, GrantOutcome> {
        Box::pin(async move { self.decide_with(request, true).await })
    }

    fn mark_granted<'a>(&'a self, request: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let done = matches!(
                self.set_status(request, Status::Approved, Status::Granted)
                    .await,
                Ok(true)
            );
            self.audit(
                request,
                if done {
                    "granted"
                } else {
                    "grant_not_recorded"
                },
                &[],
            );
            done
        })
    }

    fn decide_approval<'a>(
        &'a self,
        request: &'a str,
        by: &'a str,
        approve: bool,
        reason: Option<String>,
    ) -> BoxFuture<'a, ApprovalResult> {
        Box::pin(async move {
            let state = match self.state(false).await {
                Ok(state) => state,
                Err(code) => return ApprovalResult::Refused(code.to_owned()),
            };
            let holds = |user: &str, role: &str| {
                state
                    .assignments
                    .iter()
                    .any(|a| a.user == user && a.role.as_str() == role)
            };
            let key = request_key(request);
            // The decision is a compare-and-swap on the stored record; a lost race reloads and re-decides.
            for _ in 0..3 {
                let text = match self.store.get(&key).await {
                    Ok(Some(text)) => text,
                    Ok(None) => return ApprovalResult::Refused(NOT_PENDING.to_owned()),
                    Err(_) => return ApprovalResult::Refused(STATE_UNAVAILABLE.to_owned()),
                };
                let Some(mut record) = Record::parse(&text) else {
                    return ApprovalResult::Refused(STATE_UNAVAILABLE.to_owned());
                };
                match record.status {
                    Status::Granted => return ApprovalResult::AlreadyGranted,
                    Status::Ended => return ApprovalResult::Refused(NOT_PENDING.to_owned()),
                    // Decided already, grant not recorded: the same approver resumes it.
                    Status::Approved => {
                        return match &record.approval.state {
                            ApprovalState::Approved { by: decided_by, .. }
                                if approve && decided_by == by =>
                            {
                                ApprovalResult::Approved(Box::new(record.request))
                            }
                            _ => ApprovalResult::Refused(NOT_PENDING.to_owned()),
                        };
                    }
                    Status::Pending => {}
                }
                let decision = if approve {
                    ApprovalDecision::Approve
                } else {
                    ApprovalDecision::Reject {
                        reason: reason.clone().unwrap_or_else(|| "rejected".to_owned()),
                    }
                };
                if let Err(error) =
                    record
                        .approval
                        .decide_with_roles(decision, by, self.now(), &holds)
                {
                    let code = error.code().to_owned();
                    self.audit(request, "approval_refused", std::slice::from_ref(&code));
                    return ApprovalResult::Refused(code);
                }
                record.status = if record.approval.is_granted() {
                    Status::Approved
                } else {
                    Status::Ended
                };
                let Some(new) = record.encode() else {
                    return ApprovalResult::Refused(STATE_UNAVAILABLE.to_owned());
                };
                match self.store.compare_put(&key, Some(&text), &new).await {
                    Ok(true) => {
                        return if record.status == Status::Approved {
                            self.audit(request, "approved", &[]);
                            ApprovalResult::Approved(Box::new(record.request))
                        } else {
                            self.audit(request, "rejected", &[]);
                            ApprovalResult::Ended
                        };
                    }
                    Ok(false) => {}
                    Err(_) => return ApprovalResult::Refused(STATE_UNAVAILABLE.to_owned()),
                }
            }
            ApprovalResult::Refused(STATE_UNAVAILABLE.to_owned())
        })
    }
}

/// The text of the app files `name`, concatenated; `None` when no app ships one.
fn app_text(ctx: &SiteCtx<'_>, name: &str) -> Result<Option<String>> {
    let env = *ctx.env()?;
    let mut text = String::new();
    for app in super::controllers::installed(env.root, env.loaded, env.site)? {
        if let Ok(part) = std::fs::read_to_string(app.dir.join(name)) {
            text.push_str(&part);
            text.push('\n');
        }
    }
    Ok((!text.trim().is_empty()).then_some(text))
}

/// Joins the site unless `[services.governance] enabled = false`.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("governance")?;
    if settings.enabled == Some(false) {
        return Ok(());
    }
    let (site, live, catalog) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            Arc::clone(env.live),
            env.permission.catalog.clone(),
        )
    };
    let mut problem: Option<String> = None;
    let rules = match app_text(ctx, RULES_FILE)? {
        Some(text) => match RuleSet::from_toml(&text) {
            Ok(rules) => Some(rules),
            Err(_) => {
                problem = Some(format!("{RULES_FILE} does not parse"));
                None
            }
        },
        None => None,
    };
    let catalogue = match app_text(ctx, CATALOGUE_FILE)? {
        Some(text) => match RoleCatalogue::from_toml(&text) {
            Ok(c) => Some(c),
            Err(_) => {
                problem = Some(format!("{CATALOGUE_FILE} does not parse"));
                None
            }
        },
        None => None,
    };
    if ctx.assembling() {
        let report = problem.clone();
        ctx.add(Idle::new("governance", Level::Optional, move || {
            let report = report.clone();
            Box::pin(async move { report.map_or(Ok(()), Err) })
        }));
    }
    let slot = Arc::new(LauncherSlot::default());
    ctx.provide(Arc::clone(&slot))?;
    let store: Arc<dyn GovernanceStore> = match ctx.port::<dyn GovernanceStore>() {
        Some(store) => store,
        None => {
            tracing::warn!(
                target: AUDIT,
                site = %site,
                "no durable governance store is provided: pending grant approvals and SoD overrides are \
                 kept in memory and are lost at a restart"
            );
            Arc::new(MemoryGovernanceStore::default())
        }
    };
    let guard = Arc::new(Governance::from_parts(Parts {
        source: Arc::new(SiteDirectory {
            site: site.clone(),
            live,
            catalog,
        }),
        site,
        store,
        clock: Arc::clone(ctx.clock()),
        rules,
        catalogue,
        delegation: settings.delegation,
        approval: settings.approval,
        launcher: Arc::clone(&slot),
        broken: problem.is_some(),
    }));
    let port: Arc<dyn GrantGuard> = guard;
    ctx.provide(port)?;
    Ok(())
}
