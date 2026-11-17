//! The journal and sync operations execute on a running site (WP-RTB-04.T3, ADR-061, ADR-062,
//! ADR-44). Operator guide: `docs/running-spot/mgmt_exec.md`.
//!
//! `spot-mgmt-journal` and `spot-mgmt-sync` define the operations and the typed executor traits
//! ([`JournalOps`], [`SyncOps`]); the hosted services do the work. This module is the **site-side
//! executor in between**: [`JournalExec`] and [`SyncExec`] implement the same traits over the
//! service-side implementations and add what every operation needs and no service should
//! re-implement:
//!
//! 1. **The site is the signed command's audience**, never a parameter: [`ExecRegistry::executors`]
//!    is keyed by the audience, so a command for site A can only reach site A's services.
//! 2. **Permission per operation family**, default deny ([`Permissions`]): `journal.admin`,
//!    `journal.erase_subject`, `sync.admin`, `sync.node.enrol` and the read-only `journal.view` /
//!    `sync.view`. The signed command proves *who*; the permission says what that principal may do.
//!    Families are separate: `sync.admin` cannot grant `journal_reader`.
//! 3. **Confirmation**: `journal.rebuild.run` re-plans and refuses (`Conflict`, `plan_hash_differs`)
//!    when the plan's hash is not the one the person confirmed.
//! 4. **Offline operations** (`journal.rebuild.run`, `sync.role.change`) are refused with
//!    `mgmt_exec.site_running` while the site runs: this executor never stops the site.
//! 5. **One destructive operation per site at a time** (`mgmt_exec.busy`) and an **idempotent
//!    retry**: the same command id answers the first result for the window; an enrolment code is
//!    delivered once and the stored copy never holds it.
//! 6. **An audit record for every outcome** of a state-changing operation, refusals included
//!    (`sys_journal_admin`, `sys_sync_node_admin`, `sys_sync_decision`): who (the principal of the
//!    signed command, never `system`), which operation, the outcome and a code, never content. For
//!    a destructive operation the record is written **first**: if it cannot be written, nothing is
//!    done.
//! 7. A flood of refused commands is rate limited per principal (default 30 a minute); the
//!    limiter's refusals are **counted**, not audited one by one.
//!
//! Replay protection (the nonce window) and the transport are the management API's, before this
//! executor is reached: a replayed command never gets here.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value as Json, json};
use spot_mgmt_journal::args::{
    PolicySetting, ReaderGrantArgs, ReaderRevokeArgs, RebuildPlanArgs, RebuildRunArgs,
    SnapshotListArgs, SubjectEraseArgs,
};
use spot_mgmt_journal::results::{
    AuditResult, PolicyResult, ReaderGrantResult, ReaderListResult, ReaderRevokeResult,
    RebuildPlanResult, RebuildRunResult, RecoveryReportResult, ReshipResult, SnapshotListResult,
    SnapshotTakeResult, StatusResult, SubjectEraseResult, VerifyResult,
};
use spot_mgmt_journal::{ExecError as JournalError, ExecKind as JournalKind, JournalOps};
use spot_mgmt_sync::args::{
    EnrolArgs, NodeArgs, PolicyShowArgs, QuarantineDiscardArgs, QuarantineRetryArgs, ResendArgs,
    RoleChangeArgs,
};
use spot_sync_health::HealthView;
use spot_mgmt_sync::results::{
    BootstrapStatusResult, ConditionListResult, EnrolResult, NodeListResult,
    NodeShowResult, NodeStateResult, NotifyContactResult, PolicyLintResult, PolicyShowResult,
    QuarantineDiscardResult, QuarantineListResult, QuarantineRetryResult, ResendResult,
    RoleChangeResult, RotateCertificateResult,
};
use spot_mgmt_sync::{ExecError as SyncError, ExecKind as SyncKind, SyncOps};

use crate::services::SiteCtx;

/// Start order: after the services whose operations it executes.
pub const ORDER: u32 = 1230;

/// The role-membership pseudo permission (`Tenant Administrator`); the host resolves it.
pub const ADMIN_ROLE: &str = "role:Tenant Administrator";

/// The longest an external reader's grant may be unless the caller is an administrator.
pub const EXTERNAL_GRANT_DAYS: u16 = 7;

/// The longest list a result may carry through this executor.
pub const MAX_LIST: usize = 500;

/// `[services.mgmt_exec]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` leaves the executors unregistered.
    pub enabled: bool,
    /// Commands a principal may send per minute before the limiter refuses (default 30).
    pub rate_per_minute: u32,
    /// How long the result of a command id is kept for an idempotent retry, in seconds.
    pub result_window_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            rate_per_minute: 30,
            result_window_seconds: 600,
        }
    }
}

/// A boxed future.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Whether a principal holds a named permission on this site. Default deny: `None` (it cannot be
/// decided) is a denial.
pub trait Permissions: Send + Sync {
    /// The answer for `principal` and `permission` (or [`ADMIN_ROLE`]).
    fn holds<'a>(&'a self, principal: &'a str, permission: &'a str) -> BoxFut<'a, Option<bool>>;
}

/// Whether the site is serving (offline operations refuse while it is).
pub trait SiteState: Send + Sync {
    /// `true` while the site serves.
    fn running(&self) -> bool;
}

/// Which audit topic a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topic {
    /// `sys_journal_admin`.
    JournalAdmin,
    /// `sys_sync_node_admin`.
    SyncNodeAdmin,
    /// `sys_sync_decision`.
    SyncDecision,
}

impl Topic {
    /// The registered topic name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::JournalAdmin => "sys_journal_admin",
            Self::SyncNodeAdmin => "sys_sync_node_admin",
            Self::SyncDecision => "sys_sync_decision",
        }
    }
}

/// One audit record of an operation's outcome. Never content, a key, a code word or a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRecord {
    /// The topic.
    pub topic: Topic,
    /// The registered action.
    pub action: &'static str,
    /// The principal of the signed command (never `system`).
    pub actor: String,
    /// A position, policy name or snapshot id (journal admin), never a record id.
    pub target: Option<String>,
    /// The node, for the sync topics.
    pub node: Option<String>,
    /// `ok`, `refused` or `failed`.
    pub outcome: &'static str,
    /// A bounded catalogue code, not for `ok`.
    pub code: Option<String>,
}

impl ExecRecord {
    /// The payload of the registered topic.
    #[must_use]
    pub fn to_payload(&self) -> Json {
        let mut map = serde_json::Map::new();
        map.insert("action".to_owned(), json!(self.action));
        map.insert("actor".to_owned(), json!(self.actor));
        map.insert("outcome".to_owned(), json!(self.outcome));
        match self.topic {
            Topic::JournalAdmin => {
                map.insert("dry_run".to_owned(), json!(false));
                if let Some(target) = &self.target {
                    map.insert("target".to_owned(), json!(target));
                }
            }
            Topic::SyncNodeAdmin => {
                map.insert(
                    "node".to_owned(),
                    json!(self.node.clone().unwrap_or_default()),
                );
            }
            Topic::SyncDecision => {
                if let Some(node) = &self.node {
                    map.insert("node".to_owned(), json!(node));
                }
            }
        }
        if let Some(code) = &self.code {
            map.insert("code".to_owned(), json!(code));
        }
        Json::Object(map)
    }
}

/// Writes the audit record. A failure means the record does not exist.
pub trait ExecAudit: Send + Sync {
    /// Records one outcome.
    fn record<'a>(&'a self, record: &'a ExecRecord) -> BoxFut<'a, std::result::Result<(), ()>>;
}

/// The command the executors act for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The principal that signed the command.
    pub principal: String,
    /// The signed command's id (the idempotent-retry key).
    pub command_id: String,
}

struct Window {
    count: u32,
    minute: i64,
}

/// What every operation of one site shares.
pub struct Shared {
    permissions: Arc<dyn Permissions>,
    state: Arc<dyn SiteState>,
    audit: Arc<dyn ExecAudit>,
    clock: Arc<dyn spot_core::Clock>,
    settings: Settings,
    busy: Mutex<bool>,
    windows: Mutex<HashMap<String, Window>>,
    results: Mutex<Vec<(String, i64, Json)>>,
    limited: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// What an operation needs and records.
#[derive(Clone, Copy)]
struct Spec {
    op: &'static str,
    /// Any one of these permissions allows the operation.
    needs: &'static [&'static str],
    audit: Option<(Topic, &'static str)>,
    destructive: bool,
    offline: bool,
}

/// Why an operation did not run, in the executors' own terms.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Why {
    Denied,
    NotFound,
    Conflict(&'static str),
    Unavailable(&'static str),
}

impl Shared {
    /// The shared state of one site.
    #[must_use]
    pub fn new(
        permissions: Arc<dyn Permissions>,
        state: Arc<dyn SiteState>,
        audit: Arc<dyn ExecAudit>,
        clock: Arc<dyn spot_core::Clock>,
        settings: Settings,
    ) -> Arc<Self> {
        Arc::new(Self {
            permissions,
            state,
            audit,
            clock,
            settings,
            busy: Mutex::new(false),
            windows: Mutex::new(HashMap::new()),
            results: Mutex::new(Vec::new()),
            limited: AtomicU64::new(0),
        })
    }

    /// How many commands the limiter refused (counted, never audited one by one).
    #[must_use]
    pub fn rate_limited(&self) -> u64 {
        self.limited.load(Ordering::SeqCst)
    }

    fn now_millis(&self) -> i64 {
        i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
    }

    fn allow_rate(&self, principal: &str) -> bool {
        let minute = self.now_millis().div_euclid(60_000);
        let mut windows = lock(&self.windows);
        let window = windows
            .entry(principal.to_owned())
            .or_insert(Window { count: 0, minute });
        if window.minute != minute {
            *window = Window { count: 0, minute };
        }
        if window.count >= self.settings.rate_per_minute {
            self.limited.fetch_add(1, Ordering::SeqCst);
            return false;
        }
        window.count += 1;
        true
    }

    fn cached(&self, key: &str) -> Option<Json> {
        let now = self.now_millis();
        let window = i64::try_from(self.settings.result_window_seconds).unwrap_or(0) * 1000;
        let mut results = lock(&self.results);
        results.retain(|(_, at, _)| now - at <= window);
        results
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, _, v)| v.clone())
    }

    fn remember(&self, key: String, value: Json) {
        let now = self.now_millis();
        lock(&self.results).push((key, now, value));
    }

    async fn allowed(&self, principal: &str, needs: &[&str]) -> bool {
        for permission in needs {
            if matches!(
                self.permissions.holds(principal, permission).await,
                Some(true)
            ) {
                return true;
            }
        }
        false
    }

    async fn audit(
        &self,
        spec: &Spec,
        caller: &Caller,
        node: &Option<String>,
        target: &Option<String>,
        outcome: &'static str,
        code: Option<&str>,
    ) -> std::result::Result<(), ()> {
        let Some((topic, action)) = spec.audit else {
            // A read: refusals and failures are logged and counted, successes are not recorded.
            if outcome != "ok" {
                tracing::info!(
                    target: "spot::audit::mgmt_exec",
                    op = spec.op,
                    outcome,
                    code = code.unwrap_or(""),
                    "management operation"
                );
            }
            return Ok(());
        };
        self.audit
            .record(&ExecRecord {
                topic,
                action,
                actor: caller.principal.clone(),
                target: target.clone(),
                node: node.clone(),
                outcome,
                code: code.map(str::to_owned),
            })
            .await
    }

    /// The one path every operation takes. `call` runs the service; it is built only after every
    /// check passed.
    async fn run<R, F, Fut>(
        &self,
        spec: Spec,
        caller: &Caller,
        node: Option<String>,
        target: Option<String>,
        call: F,
    ) -> std::result::Result<R, Why>
    where
        R: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = std::result::Result<R, Why>>,
    {
        if !self.allow_rate(&caller.principal) {
            return Err(Why::Conflict("rate_limited"));
        }
        let key = format!("{}|{}", caller.command_id, spec.op);
        if let Some(stored) = self.cached(&key)
            && let Ok(result) = serde_json::from_value::<R>(stored)
        {
            return Ok(result);
        }
        if !self.allowed(&caller.principal, spec.needs).await {
            let _ = self
                .audit(
                    &spec,
                    caller,
                    &node,
                    &target,
                    "refused",
                    Some("mgmt_exec.denied"),
                )
                .await;
            return Err(Why::Denied);
        }
        if spec.offline && self.state.running() {
            let _ = self
                .audit(
                    &spec,
                    caller,
                    &node,
                    &target,
                    "refused",
                    Some("mgmt_exec.site_running"),
                )
                .await;
            return Err(Why::Conflict("site_running"));
        }
        let lease = if spec.destructive {
            // The flag is taken in its own block: no guard is alive across the audit `await`.
            let was_busy = {
                let mut busy = lock(&self.busy);
                let was = *busy;
                *busy = true;
                was
            };
            if was_busy {
                let _ = self
                    .audit(
                        &spec,
                        caller,
                        &node,
                        &target,
                        "refused",
                        Some("mgmt_exec.busy"),
                    )
                    .await;
                return Err(Why::Conflict("busy"));
            }
            Some(BusyGuard(self))
        } else {
            None
        };
        // Audit first for a destructive operation: no record, no action.
        if spec.destructive
            && self
                .audit(&spec, caller, &node, &target, "ok", None)
                .await
                .is_err()
        {
            drop(lease);
            return Err(Why::Unavailable("audit_unwritable"));
        }
        let outcome = call().await;
        let finished = match &outcome {
            Ok(_) => {
                if spec.destructive {
                    // The intent record stands as the success record.
                    Ok(())
                } else {
                    self.audit(&spec, caller, &node, &target, "ok", None).await
                }
            }
            Err(why) => {
                let code = code_of(*why);
                self.audit(&spec, caller, &node, &target, "failed", Some(code))
                    .await
            }
        };
        drop(lease);
        if finished.is_err() && outcome.is_ok() && !spec.destructive {
            // The action happened but its record could not be written: say so, never succeed
            // silently without the audit.
            return Err(Why::Unavailable("audit_unwritable"));
        }
        if let Ok(result) = &outcome
            && let Ok(mut stored) = serde_json::to_value(result)
        {
            // A one-time code is delivered once; the stored copy never holds it.
            if let Some(map) = stored.as_object_mut() {
                map.remove("code");
            }
            self.remember(key, stored);
        }
        outcome
    }
}

struct BusyGuard<'a>(&'a Shared);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        *lock(&self.0.busy) = false;
    }
}

/// The catalogue code of a failure, from its class and reason.
fn code_of(why: Why) -> &'static str {
    match why {
        Why::Denied => "mgmt_exec.denied",
        Why::NotFound => "mgmt_exec.not_found",
        Why::Conflict("busy") => "mgmt_exec.busy",
        Why::Conflict("site_running") => "mgmt_exec.site_running",
        Why::Conflict("plan_hash_differs") => "mgmt_exec.plan_hash_differs",
        Why::Conflict("rate_limited") => "mgmt_exec.rate_limited",
        Why::Conflict(_) => "mgmt_exec.conflict",
        Why::Unavailable("audit_unwritable") => "mgmt_exec.audit_unwritable",
        Why::Unavailable("service_failed") => "mgmt_exec.service_failed",
        Why::Unavailable(_) => "mgmt_exec.unavailable",
    }
}

fn journal_error(why: Why) -> JournalError {
    match why {
        Why::Denied => JournalError::new(JournalKind::Denied, "permission_denied"),
        Why::NotFound => JournalError::new(JournalKind::NotFound, "not_found"),
        Why::Conflict(reason) => JournalError::new(JournalKind::Conflict, reason),
        Why::Unavailable(reason) => JournalError::new(JournalKind::Unavailable, reason),
    }
}

fn sync_error(why: Why) -> SyncError {
    match why {
        Why::Denied => SyncError::new(SyncKind::Denied, "permission_denied"),
        Why::NotFound => SyncError::new(SyncKind::NotFound, "not_found"),
        Why::Conflict(reason) => SyncError::new(SyncKind::Conflict, reason),
        Why::Unavailable(reason) => SyncError::new(SyncKind::Unavailable, reason),
    }
}

fn from_journal(error: JournalError) -> Why {
    match error.kind {
        JournalKind::Denied => Why::Denied,
        JournalKind::NotFound => Why::NotFound,
        JournalKind::Conflict => Why::Conflict(static_reason(&error.reason)),
        JournalKind::Unavailable | JournalKind::Failed => {
            Why::Unavailable(static_reason(&error.reason))
        }
    }
}

fn from_sync(error: SyncError) -> Why {
    match error.kind {
        SyncKind::Denied => Why::Denied,
        SyncKind::NotFound => Why::NotFound,
        SyncKind::Conflict => Why::Conflict(static_reason(&error.reason)),
        SyncKind::Unavailable | SyncKind::Failed => Why::Unavailable(static_reason(&error.reason)),
    }
}

/// A service's reason is a short code of the `snake_case` alphabet; map it onto the few static
/// reasons the executor reports so a free-form reason never travels through.
fn static_reason(reason: &str) -> &'static str {
    match reason {
        "plan_hash_differs" => "plan_hash_differs",
        "busy" => "busy",
        "site_running" => "site_running",
        "not_found" => "not_found",
        "unavailable" => "unavailable",
        _ => "service_failed",
    }
}

const JOURNAL_VIEW: &[&str] = &["journal.view", "journal.admin", ADMIN_ROLE];
const JOURNAL_ADMIN: &[&str] = &["journal.admin", ADMIN_ROLE];
const JOURNAL_ERASE: &[&str] = &["journal.erase_subject"];
const SYNC_VIEW: &[&str] = &["sync.view", "sync.admin", ADMIN_ROLE];
const SYNC_ADMIN: &[&str] = &["sync.admin", ADMIN_ROLE];
const SYNC_ENROL: &[&str] = &["sync.node.enrol", "sync.admin"];

fn read(op: &'static str, needs: &'static [&'static str]) -> Spec {
    Spec {
        op,
        needs,
        audit: None,
        destructive: false,
        offline: false,
    }
}

fn change(
    op: &'static str,
    needs: &'static [&'static str],
    topic: Topic,
    action: &'static str,
) -> Spec {
    Spec {
        op,
        needs,
        audit: Some((topic, action)),
        destructive: false,
        offline: false,
    }
}

fn destructive(
    op: &'static str,
    needs: &'static [&'static str],
    topic: Topic,
    action: &'static str,
) -> Spec {
    Spec {
        destructive: true,
        ..change(op, needs, topic, action)
    }
}

/// The journal operations of one site for one signed command.
pub struct JournalExec {
    shared: Arc<Shared>,
    caller: Caller,
    inner: Arc<dyn JournalOps>,
}

/// The sync operations of one site for one signed command.
pub struct SyncExec {
    shared: Arc<Shared>,
    caller: Caller,
    inner: Arc<dyn SyncOps>,
}

impl JournalExec {
    async fn go<R, F, Fut>(&self, spec: Spec, target: Option<String>, call: F) -> Result2<R>
    where
        R: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = std::result::Result<R, JournalError>>,
    {
        self.shared
            .run(spec, &self.caller, None, target, || async move {
                call().await.map_err(from_journal)
            })
            .await
            .map_err(journal_error)
    }
}

type Result2<R> = std::result::Result<R, JournalError>;

#[async_trait]
impl JournalOps for JournalExec {
    async fn status(&self) -> Result<StatusResult, JournalError> {
        self.go(read("journal.status", JOURNAL_VIEW), None, || {
            self.inner.status()
        })
        .await
    }
    async fn recovery_report(&self) -> Result<RecoveryReportResult, JournalError> {
        self.go(read("journal.recovery.report", JOURNAL_VIEW), None, || {
            self.inner.recovery_report()
        })
        .await
    }
    async fn snapshot_list(
        &self,
        args: SnapshotListArgs,
    ) -> Result<SnapshotListResult, JournalError> {
        let args = SnapshotListArgs {
            limit: args.limit.min(MAX_LIST as u32),
        };
        self.go(read("journal.snapshot.list", JOURNAL_VIEW), None, || {
            self.inner.snapshot_list(args)
        })
        .await
    }
    async fn snapshot_take(&self) -> Result<SnapshotTakeResult, JournalError> {
        self.go(
            change(
                "journal.snapshot.take",
                JOURNAL_ADMIN,
                Topic::JournalAdmin,
                "snapshot_taken",
            ),
            None,
            || self.inner.snapshot_take(),
        )
        .await
    }
    async fn verify(&self) -> Result<VerifyResult, JournalError> {
        self.go(read("journal.verify", JOURNAL_VIEW), None, || {
            self.inner.verify()
        })
        .await
    }
    async fn audit_last(&self) -> Result<AuditResult, JournalError> {
        self.go(read("journal.audit.last", JOURNAL_VIEW), None, || {
            self.inner.audit_last()
        })
        .await
    }
    async fn audit_run(&self) -> Result<AuditResult, JournalError> {
        self.go(
            change(
                "journal.audit.run",
                JOURNAL_ADMIN,
                Topic::JournalAdmin,
                "audit_run",
            ),
            None,
            || self.inner.audit_run(),
        )
        .await
    }
    async fn policy_get(&self) -> Result<PolicyResult, JournalError> {
        self.go(read("journal.policy.get", JOURNAL_VIEW), None, || {
            self.inner.policy_get()
        })
        .await
    }
    async fn policy_set(&self, policy: PolicySetting) -> Result<PolicyResult, JournalError> {
        self.go(
            destructive(
                "journal.policy.set",
                JOURNAL_ADMIN,
                Topic::JournalAdmin,
                "policy_changed",
            ),
            None,
            || self.inner.policy_set(policy),
        )
        .await
    }
    async fn reship(&self) -> Result<ReshipResult, JournalError> {
        self.go(
            destructive(
                "journal.reship",
                JOURNAL_ADMIN,
                Topic::JournalAdmin,
                "reship_started",
            ),
            None,
            || self.inner.reship(),
        )
        .await
    }
    async fn rebuild_plan(&self, args: RebuildPlanArgs) -> Result<RebuildPlanResult, JournalError> {
        self.go(read("journal.rebuild.plan", JOURNAL_VIEW), None, || {
            self.inner.rebuild_plan(args)
        })
        .await
    }
    async fn rebuild_run(&self, args: RebuildRunArgs) -> Result<RebuildRunResult, JournalError> {
        let spec = Spec {
            offline: true,
            ..destructive(
                "journal.rebuild.run",
                JOURNAL_ADMIN,
                Topic::JournalAdmin,
                "rebuild_started",
            )
        };
        let inner = Arc::clone(&self.inner);
        self.go(spec, None, move || async move {
            // Re-plan and refuse unless this is exactly the plan a person confirmed.
            let planned = inner.rebuild_plan(args.plan.clone()).await?;
            if planned.plan_hash != args.confirm_plan_hash {
                return Err(JournalError::new(
                    JournalKind::Conflict,
                    "plan_hash_differs",
                ));
            }
            inner.rebuild_run(args).await
        })
        .await
    }
    async fn reader_list(&self) -> Result<ReaderListResult, JournalError> {
        self.go(read("journal.reader.list", JOURNAL_ADMIN), None, || {
            self.inner.reader_list()
        })
        .await
    }
    async fn reader_grant(&self, args: ReaderGrantArgs) -> Result<ReaderGrantResult, JournalError> {
        // An external reader's grant is limited to 7 days unless the caller is an administrator
        // (WP-JRN-09.T4's rule, checked again here).
        if args.external
            && args.days > EXTERNAL_GRANT_DAYS
            && !matches!(
                self.shared
                    .permissions
                    .holds(&self.caller.principal, ADMIN_ROLE)
                    .await,
                Some(true)
            )
        {
            // The grant service writes `sys_journal_grant`; a refusal made here has no grant
            // to name, so it is logged and counted.
            tracing::info!(
                target: "spot::audit::mgmt_exec",
                op = "journal.reader.grant",
                outcome = "refused",
                code = "mgmt_exec.external_grant_too_long",
                "management operation"
            );
            return Err(JournalError::new(
                JournalKind::Denied,
                "external_grant_too_long",
            ));
        }
        // The grant service writes `sys_journal_grant` itself, in the grant's transaction.
        self.go(read("journal.reader.grant", JOURNAL_ADMIN), None, || {
            self.inner.reader_grant(args)
        })
        .await
    }
    async fn reader_revoke(
        &self,
        args: ReaderRevokeArgs,
    ) -> Result<ReaderRevokeResult, JournalError> {
        self.go(read("journal.reader.revoke", JOURNAL_ADMIN), None, || {
            self.inner.reader_revoke(args)
        })
        .await
    }
    async fn subject_erase(
        &self,
        args: SubjectEraseArgs,
    ) -> Result<SubjectEraseResult, JournalError> {
        self.go(
            destructive(
                "journal.subject.erase",
                JOURNAL_ERASE,
                Topic::JournalAdmin,
                "subject_erased",
            ),
            None,
            || self.inner.subject_erase(args),
        )
        .await
    }
}

impl SyncExec {
    async fn go<R, F, Fut>(
        &self,
        spec: Spec,
        node: Option<String>,
        call: F,
    ) -> std::result::Result<R, SyncError>
    where
        R: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = std::result::Result<R, SyncError>>,
    {
        self.shared
            .run(spec, &self.caller, node, None, || async move {
                call().await.map_err(from_sync)
            })
            .await
            .map_err(sync_error)
    }
}

#[async_trait]
impl SyncOps for SyncExec {
    async fn health(&self) -> Result<HealthView, SyncError> {
        self.go(read("sync.health", SYNC_VIEW), None, || self.inner.health())
            .await
    }
    async fn node_list(&self) -> Result<NodeListResult, SyncError> {
        self.go(read("sync.node.list", SYNC_VIEW), None, || {
            self.inner.node_list()
        })
        .await
    }
    async fn node_show(&self, args: NodeArgs) -> Result<NodeShowResult, SyncError> {
        self.go(read("sync.node.show", SYNC_VIEW), None, || {
            self.inner.node_show(args)
        })
        .await
    }
    async fn node_enrol(&self, args: EnrolArgs) -> Result<EnrolResult, SyncError> {
        let node = Some(args.node.clone());
        self.go(
            change(
                "sync.node.enrol",
                SYNC_ENROL,
                Topic::SyncNodeAdmin,
                "enrol_started",
            ),
            node,
            || self.inner.node_enrol(args),
        )
        .await
    }
    async fn node_suspend(&self, args: NodeArgs) -> Result<NodeStateResult, SyncError> {
        let node = Some(args.node.clone());
        self.go(
            destructive(
                "sync.node.suspend",
                SYNC_ADMIN,
                Topic::SyncNodeAdmin,
                "suspended",
            ),
            node,
            || self.inner.node_suspend(args),
        )
        .await
    }
    async fn node_resume(&self, args: NodeArgs) -> Result<NodeStateResult, SyncError> {
        let node = Some(args.node.clone());
        self.go(
            change(
                "sync.node.resume",
                SYNC_ADMIN,
                Topic::SyncNodeAdmin,
                "resumed",
            ),
            node,
            || self.inner.node_resume(args),
        )
        .await
    }
    async fn node_revoke(&self, args: NodeArgs) -> Result<NodeStateResult, SyncError> {
        let node = Some(args.node.clone());
        self.go(
            destructive(
                "sync.node.revoke",
                SYNC_ADMIN,
                Topic::SyncNodeAdmin,
                "revoked",
            ),
            node,
            || self.inner.node_revoke(args),
        )
        .await
    }
    async fn node_rotate_certificate(
        &self,
        args: NodeArgs,
    ) -> Result<RotateCertificateResult, SyncError> {
        let node = Some(args.node.clone());
        self.go(
            change(
                "sync.node.rotate_certificate",
                SYNC_ADMIN,
                Topic::SyncNodeAdmin,
                "certificate_rotated",
            ),
            node,
            || self.inner.node_rotate_certificate(args),
        )
        .await
    }
    async fn node_resend(&self, args: ResendArgs) -> Result<ResendResult, SyncError> {
        let node = Some(args.node.clone());
        self.go(
            change(
                "sync.node.resend",
                SYNC_ADMIN,
                Topic::SyncDecision,
                "resend_requested",
            ),
            node,
            || self.inner.node_resend(args),
        )
        .await
    }
    async fn node_notify_contact(&self, args: NodeArgs) -> Result<NotifyContactResult, SyncError> {
        let node = Some(args.node.clone());
        self.go(
            change(
                "sync.node.notify_contact",
                SYNC_ADMIN,
                Topic::SyncDecision,
                "contact_notified",
            ),
            node,
            || self.inner.node_notify_contact(args),
        )
        .await
    }
    async fn condition_list(&self) -> Result<ConditionListResult, SyncError> {
        self.go(read("sync.condition.list", SYNC_VIEW), None, || {
            self.inner.condition_list()
        })
        .await
    }
    async fn quarantine_list(&self) -> Result<QuarantineListResult, SyncError> {
        self.go(read("sync.quarantine.list", SYNC_VIEW), None, || {
            self.inner.quarantine_list()
        })
        .await
    }
    async fn quarantine_retry(
        &self,
        args: QuarantineRetryArgs,
    ) -> Result<QuarantineRetryResult, SyncError> {
        self.go(
            change(
                "sync.quarantine.retry",
                SYNC_ADMIN,
                Topic::SyncDecision,
                "quarantine_released",
            ),
            None,
            || self.inner.quarantine_retry(args),
        )
        .await
    }
    async fn quarantine_discard(
        &self,
        args: QuarantineDiscardArgs,
    ) -> Result<QuarantineDiscardResult, SyncError> {
        self.go(
            destructive(
                "sync.quarantine.discard",
                SYNC_ADMIN,
                Topic::SyncDecision,
                "quarantine_discarded",
            ),
            None,
            || self.inner.quarantine_discard(args),
        )
        .await
    }
    async fn policy_show(&self, args: PolicyShowArgs) -> Result<PolicyShowResult, SyncError> {
        self.go(read("sync.policy.show", SYNC_VIEW), None, || {
            self.inner.policy_show(args)
        })
        .await
    }
    async fn policy_lint(&self) -> Result<PolicyLintResult, SyncError> {
        self.go(read("sync.policy.lint", SYNC_VIEW), None, || {
            self.inner.policy_lint()
        })
        .await
    }
    async fn role_change(&self, args: RoleChangeArgs) -> Result<RoleChangeResult, SyncError> {
        let spec = Spec {
            offline: true,
            ..destructive(
                "sync.role.change",
                SYNC_ADMIN,
                Topic::SyncNodeAdmin,
                "role_changed",
            )
        };
        self.go(spec, Some("this".to_owned()), || {
            self.inner.role_change(args)
        })
        .await
    }
    async fn bootstrap_status(&self) -> Result<BootstrapStatusResult, SyncError> {
        self.go(read("sync.bootstrap.status", SYNC_VIEW), None, || {
            self.inner.bootstrap_status()
        })
        .await
    }
}

/// The service-side implementations of one site, as the registry holds them.
pub struct SiteExecutors {
    /// What every operation shares.
    pub shared: Arc<Shared>,
    /// The journal services' implementation.
    pub journal: Arc<dyn JournalOps>,
    /// The sync services' implementation.
    pub sync: Arc<dyn SyncOps>,
}

/// The executors by audience: the site comes from the signed command's audience and nowhere else.
#[derive(Default)]
pub struct ExecRegistry {
    sites: Mutex<HashMap<String, Arc<SiteExecutors>>>,
}

impl ExecRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the executors of `audience` (the site).
    pub fn register(&self, audience: &str, executors: SiteExecutors) {
        lock(&self.sites).insert(audience.to_owned(), Arc::new(executors));
    }

    /// The executors for one signed command: bound to the audience's site and to the command's
    /// principal and id.
    ///
    /// # Errors
    /// [`JournalError`] (`NotFound`) for an audience that is not a site of this host: the answer
    /// does not say whether another site exists.
    pub fn executors(
        &self,
        audience: &str,
        caller: Caller,
    ) -> std::result::Result<(JournalExec, SyncExec), JournalError> {
        let site = lock(&self.sites)
            .get(audience)
            .cloned()
            .ok_or_else(|| JournalError::new(JournalKind::NotFound, "not_found"))?;
        Ok((
            JournalExec {
                shared: Arc::clone(&site.shared),
                caller: caller.clone(),
                inner: Arc::clone(&site.journal),
            },
            SyncExec {
                shared: Arc::clone(&site.shared),
                caller,
                inner: Arc::clone(&site.sync),
            },
        ))
    }
}

/// The default [`Permissions`]: the Tenant Administrator holds every permission except
/// `journal.erase_subject`, which is its own (a deployment can provide a finer one as a port).
#[allow(dead_code)]
struct AdministratorOnly {
    live: Arc<crate::live::SiteLive>,
}

impl Permissions for AdministratorOnly {
    fn holds<'a>(&'a self, principal: &'a str, permission: &'a str) -> BoxFut<'a, Option<bool>> {
        Box::pin(async move {
            if permission == "journal.erase_subject" {
                return Some(false);
            }
            let user = spot_http::api::codec::parse_record_id(principal)?;
            let principals = self.live.principals()?;
            let resolved =
                spot_document::engine::PrincipalSource::resolve(&*principals, &user).await;
            Some(
                !resolved.is_system
                    && resolved
                        .assignments
                        .iter()
                        .flat_map(|a| a.roles.iter())
                        .any(|role| role.as_str() == spot_permissions::TENANT_ADMINISTRATOR),
            )
        })
    }
}

/// The default [`SiteState`]: this process is the site, so it is running.
#[allow(dead_code)]
struct AlwaysRunning;

impl SiteState for AlwaysRunning {
    fn running(&self) -> bool {
        true
    }
}

/// The default [`ExecAudit`]: an outbox event in its own transaction.
#[allow(dead_code)]
struct OutboxAudit {
    backend: spot_postgres::PgBackend,
    catalog: Arc<spot_storage_contract::Catalog>,
}

impl ExecAudit for OutboxAudit {
    fn record<'a>(&'a self, record: &'a ExecRecord) -> BoxFut<'a, std::result::Result<(), ()>> {
        use spot_storage_contract::{
            Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId, StorageBackend, Transaction,
            TxOptions,
        };
        Box::pin(async move {
            let options = TxOptions::new(
                Name::new("mgmt_exec_audit").map_err(|_| ())?,
                Isolation::ReadCommitted,
            );
            let mut tx = self
                .backend
                .begin(Arc::clone(&self.catalog), options)
                .await
                .map_err(|_| ())?;
            tx.enqueue(&OutboxEvent {
                id: RecordId::new_v7(),
                topic: Name::new(record.topic.name()).map_err(|_| ())?,
                subject: NodeRef {
                    entity: Name::new("mgmt_exec").map_err(|_| ())?,
                    id: RecordId::new_v7(),
                },
                subject_version: None,
                schema_version: 1,
                payload: record.to_payload().to_string(),
            })
            .await
            .map_err(|_| ())?;
            tx.commit().await.map_err(|_| ())
        })
    }
}

/// Registers the executors when the services' implementations exist as ports.
pub fn register(ctx: &mut SiteCtx<'_>) -> crate::Result<()> {
    let settings: Settings = ctx.settings_or_default("mgmt_exec")?;
    if !settings.enabled {
        return Ok(());
    }
    let (Some(journal), Some(sync)) = (ctx.port::<dyn JournalOps>(), ctx.port::<dyn SyncOps>())
    else {
        tracing::debug!("mgmt_exec: the journal or sync operations are not hosted on this site");
        return Ok(());
    };
    let registry = ctx
        .port::<ExecRegistry>()
        .unwrap_or_else(|| Arc::new(ExecRegistry::new()));
    let (Some(permissions), Some(state), Some(audit)) = (
        ctx.port::<dyn Permissions>(),
        ctx.port::<dyn SiteState>(),
        ctx.port::<dyn ExecAudit>(),
    ) else {
        tracing::warn!(
            "mgmt_exec: the permission, site-state or audit port is missing; no executors"
        );
        return Ok(());
    };
    let shared = Shared::new(permissions, state, audit, Arc::clone(ctx.clock()), settings);
    registry.register(
        ctx.label(),
        SiteExecutors {
            shared,
            journal,
            sync,
        },
    );
    if ctx.port::<ExecRegistry>().is_none() {
        ctx.provide::<ExecRegistry>(registry)?;
    }
    Ok(())
}
