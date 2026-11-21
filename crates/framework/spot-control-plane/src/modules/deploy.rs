//! The `deploy` module (`WP-ADM-13.T4`): `spot-deploy-wizard` hosted as control-plane operations.
//!
//! - `POST /cp/v1/deployments/plan`: topology in; the module reads signed facts from every
//!   target agent (read operations through [`AgentLink`]), verifies them against the enrolled
//!   agent keys, runs pre-flight and plans. Nothing is delivered to an agent, no token is made.
//!   A failing blocking check answers `cp.invalid` naming the check.
//! - `POST /cp/v1/deployments/{id}/approve`: the signed command binds the body, which carries the
//!   plan hash (the digest of the plan id and the facts it was built from). A hash that is not
//!   the stored one is refused. On success one one-time enrolment token per target is made
//!   (24 h, stored hashed) and shown in this answer only.
//! - `POST /cp/v1/deployments/{id}/run`: the plan is rebuilt from the stored facts (they must
//!   still be fresh), `authorise` checks administrator, step-up, an approver other than the
//!   runner and the exact plan hash, destructive steps must be named in `confirm_steps`, and the
//!   wizard's executor then signs each step and hands it to the agent. The journal and the
//!   audit are control-schema rows written before each step, so a restart resumes without
//!   repeating a completed step.
//! - `GET /cp/v1/deployments/{id}`: state, steps with their progress, approval.
//!
//! A verified signed command is the step-up evidence: the intake accepts only a fresh envelope
//! signed by an administrator key. Steps are signed with the deployment key in
//! [`Wiring::signer`] (a key the agents trust for `deploy.*` and nothing else), never with the
//! control plane's host CA key.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spot_core::Clock;
use spot_deploy_wizard::{
    AdminIdentity, Approval, AuditEntry, AuditSink, AuditUnavailable, CommandWindow,
    DEPLOY_PERMISSION, DeployExecutor, DeployPlan, EnrolledAgents, ExecutorFailure, FactsLedger,
    FactsPolicy, Journal, JournalEntry, PlanHash, PlanId, Prepared, Receipt, RunEnv, ServerFacts,
    SignedFacts, StepState, StepUp, Topology, WizardError, authorise, prepare, resume,
};
use spot_mgmt_api::ports::BoxFuture;
use spot_mgmt_ops::{Catalogue, CatalogueBuilder, OperationModule};
use spot_signed_command::{SignedCommand, Signer};

use super::tenants_kv::{Kv, MemoryKv, PgKv, from_json, to_json};
use super::tenants_state::fill_random;
use crate::auth::{OperatorCall, sha256_hex};
use crate::{CpCode, CpCtx, CpError};

/// Registration order: after `licensing`, clear of the other modules.
pub const ORDER: u32 = 170;

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
    "CREATE TABLE IF NOT EXISTS cp_tenants_lease (id TEXT PRIMARY KEY, holder TEXT NOT NULL, until BIGINT NOT NULL)",
];
/// How long an enrolment token lives, in seconds (24 h).
pub const TOKEN_TTL_SECS: i64 = 86_400;
const MAX_NODES: usize = 64;
const RUN_LEASE_MS: i64 = 600_000;
const COMMAND_LIFETIME_SECS: i64 = 120;
const FACTS_AGE_SECS: i64 = 900;

/// The agent side of the module: facts are a read operation, a step is a command.
pub trait AgentLink: Send + Sync {
    /// The signed facts report of the agent on `host` (a pre-flight read operation).
    ///
    /// # Errors
    /// A short secret-free reason when the agent cannot be reached.
    fn collect_facts<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Result<SignedFacts, String>>;
    /// Hands `command` (signed, addressed to `audience`) to the agent and waits for its answer.
    ///
    /// # Errors
    /// The agent's refusal or failure, secret-free.
    fn deliver<'a>(
        &'a self,
        audience: &'a str,
        command: &'a SignedCommand,
    ) -> BoxFuture<'a, Result<Receipt, String>>;
}

/// The collaborators of the module that live outside the control plane crate.
pub struct Wiring {
    /// The enrolled agents and their identity keys.
    pub directory: Arc<dyn EnrolledAgents + Send + Sync>,
    /// The agent side.
    pub link: Arc<dyn AgentLink>,
    /// The deployment key that signs each step (agents trust it for `deploy.*` only).
    pub signer: Arc<dyn Signer + Send + Sync>,
}

static WIRING: OnceLock<Wiring> = OnceLock::new();

/// Installs the production collaborators (called by `main` before the service is built); `false`
/// when they were already set. Without them the module reports not ready.
pub fn set_wiring(wiring: Wiring) -> bool {
    WIRING.set(wiring).is_ok()
}

/// The state of the module.
pub struct DeployState {
    kv: Arc<dyn Kv>,
    clock: Arc<dyn Clock>,
    directory: Arc<dyn EnrolledAgents + Send + Sync>,
    link: Arc<dyn AgentLink>,
    signer: Arc<dyn Signer + Send + Sync>,
    catalogue: Arc<Catalogue>,
    policy: FactsPolicy,
    ledger_lock: tokio::sync::Mutex<()>,
}

impl DeployState {
    /// A state over `kv` and the collaborators in `wiring`.
    ///
    /// # Errors
    /// `cp.internal` when the operation catalogue cannot be built.
    pub fn new(
        kv: Arc<dyn Kv>,
        clock: Arc<dyn Clock>,
        wiring: Wiring,
    ) -> Result<Arc<Self>, CpError> {
        let mut builder = CatalogueBuilder::new();
        spot_deploy_wizard::WizardCatalogue.register(&mut builder);
        spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
        let catalogue = builder
            .build()
            .map_err(|_| CpError::with_reason(CpCode::Internal, "deploy.catalogue"))?;
        let policy = FactsPolicy::new(FACTS_AGE_SECS)
            .map_err(|_| CpError::with_reason(CpCode::Internal, "deploy.policy"))?;
        Ok(Arc::new(Self {
            kv,
            clock,
            directory: wiring.directory,
            link: wiring.link,
            signer: wiring.signer,
            catalogue: Arc::new(catalogue),
            policy,
            ledger_lock: tokio::sync::Mutex::new(()),
        }))
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// A refusal: a registry error, or a code with a named, secret-free reason.
enum Fail {
    Cp(CpError),
    Named(CpCode, String),
}

impl From<CpError> for Fail {
    fn from(error: CpError) -> Self {
        Self::Cp(error)
    }
}

fn named(code: CpCode, detail: impl Into<String>) -> Fail {
    Fail::Named(code, detail.into())
}

fn wiz(error: &WizardError) -> Fail {
    use spot_deploy_wizard::error as c;
    let code = match error.code() {
        c::UNAUTHENTICATED => CpCode::Unauthenticated,
        c::FORBIDDEN => CpCode::Forbidden,
        c::STEP_UP_REQUIRED
        | c::APPROVAL_MISSING
        | c::APPROVAL_MISMATCH
        | c::APPROVAL_SELF
        | c::APPROVAL_EXPIRED
        | c::AUTHORISATION_EXPIRED => CpCode::ApprovalRequired,
        c::JOURNAL_MISMATCH => CpCode::Conflict,
        c::AUDIT_FAILED => CpCode::Internal,
        _ => CpCode::Invalid,
    };
    named(code, format!("{} ({})", error, error.code()))
}

fn render(call: &OperatorCall, result: Result<(StatusCode, Value), Fail>) -> Response {
    match result {
        Ok((status, value)) => (status, Json(value)).into_response(),
        Err(Fail::Cp(error)) => error.into_response_with(&call.request_id),
        Err(Fail::Named(code, detail)) => {
            let body = json!({ "error": {
                "code": code.as_str(), "message": detail,
                "retryable": code.retryable(), "request_id": call.request_id,
            }});
            let mut response = (code.status(), Json(body)).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            response
        }
    }
}

fn valid_tenant(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_plan_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

// ---------------------------------------------------------------------------------------------
// Rows: the plan record, the journal, the audit, the sequence ledger
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
struct ApprovalRec {
    approver: String,
    at: i64,
    command_id: String,
    hash: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    tenant: String,
    hash: String,
    topology: Topology,
    facts: Vec<SignedFacts>,
    requested_by: String,
    created_at: i64,
    approval: Option<ApprovalRec>,
    tokens: Vec<String>,
    /// `planned`, `approved`, `running`, `failed` or `done`.
    state: String,
}

#[derive(Default)]
struct SeqLedger {
    last: BTreeMap<String, u64>,
}

impl FactsLedger for SeqLedger {
    fn accept(&mut self, agent_id: &str, sequence: u64) -> bool {
        match self.last.get(agent_id) {
            Some(last) if *last >= sequence => false,
            _ => {
                self.last.insert(agent_id.to_owned(), sequence);
                true
            }
        }
    }
}

/// The wizard's journal over control-schema rows: every append is written before it returns.
struct KvJournal {
    handle: tokio::runtime::Handle,
    kv: Arc<dyn Kv>,
    plan: PlanId,
    entries: Vec<JournalEntry>,
}

impl KvJournal {
    async fn load(kv: &Arc<dyn Kv>, plan: &PlanId) -> Result<Self, CpError> {
        let prefix = format!("{plan}/");
        let mut entries = Vec::new();
        for (key, text) in kv.list("deploy_journal").await? {
            if key.starts_with(&prefix) {
                entries.push(from_json::<JournalEntry>(&text)?);
            }
        }
        Ok(Self {
            handle: tokio::runtime::Handle::current(),
            kv: Arc::clone(kv),
            plan: plan.clone(),
            entries,
        })
    }
}

impl Journal for KvJournal {
    fn append(&mut self, entry: JournalEntry) -> Result<(), WizardError> {
        let failed = |detail: &str| WizardError::JournalMismatch {
            detail: detail.to_owned(),
        };
        if entry.plan != self.plan {
            return Err(failed("the entry belongs to another plan"));
        }
        let text = to_json(&entry).map_err(|_| failed("the journal store failed"))?;
        let key = format!("{}/{:08}", self.plan, self.entries.len());
        self.handle
            .block_on(self.kv.put("deploy_journal", &key, &text))
            .map_err(|_| failed("the journal store failed"))?;
        self.entries.push(entry);
        Ok(())
    }

    fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }
}

/// The per-step audit over control-schema rows.
struct KvAudit {
    handle: tokio::runtime::Handle,
    kv: Arc<dyn Kv>,
    plan: String,
    next: usize,
}

impl KvAudit {
    async fn load(kv: &Arc<dyn Kv>, plan: &PlanId) -> Result<Self, CpError> {
        let prefix = format!("{plan}/");
        let next = kv
            .list("deploy_audit")
            .await?
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .count();
        Ok(Self {
            handle: tokio::runtime::Handle::current(),
            kv: Arc::clone(kv),
            plan: plan.as_str().to_owned(),
            next,
        })
    }
}

impl AuditSink for KvAudit {
    fn record(&mut self, entry: &AuditEntry) -> Result<(), AuditUnavailable> {
        let text = to_json(entry).map_err(|_| AuditUnavailable)?;
        let key = format!("{}/{:08}", self.plan, self.next);
        self.handle
            .block_on(self.kv.put("deploy_audit", &key, &text))
            .map_err(|_| AuditUnavailable)?;
        self.next += 1;
        Ok(())
    }
}

/// Carries each signed step to its agent through the [`AgentLink`].
struct LinkExecutor {
    handle: tokio::runtime::Handle,
    link: Arc<dyn AgentLink>,
}

impl DeployExecutor for LinkExecutor {
    fn submit(
        &mut self,
        _step: usize,
        command: &SignedCommand,
    ) -> Result<Receipt, ExecutorFailure> {
        let audience = command.command.audience().to_owned();
        self.handle
            .block_on(self.link.deliver(&audience, command))
            .map_err(ExecutorFailure::new)
    }
}

// ---------------------------------------------------------------------------------------------
// Enrolment tokens
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct TokenRow {
    plan: String,
    host: String,
    expires_at: i64,
}

/// Redeems a one-time enrolment token for `host` (called by the enrolment service, which has no
/// certificate to offer yet): the token is looked up by its digest, must be for this host, within
/// its 24 h and not used before. The use is recorded atomically, so two concurrent redemptions
/// cannot both succeed.
///
/// # Errors
/// `cp.unauthenticated` for an unknown or expired token, `cp.forbidden` for another host,
/// `cp.replayed` for a token already used.
pub async fn redeem_enrolment_token(
    kv: &dyn Kv,
    token: &str,
    host: &str,
    now: i64,
) -> Result<(), CpError> {
    let digest = sha256_hex(token.as_bytes());
    let text = kv
        .get("deploy_token", &digest)
        .await?
        .ok_or_else(|| CpError::with_reason(CpCode::Unauthenticated, "deploy.token_unknown"))?;
    let row: TokenRow = from_json(&text)?;
    if row.host != host {
        return Err(CpError::with_reason(
            CpCode::Forbidden,
            "deploy.token_other_host",
        ));
    }
    if now >= row.expires_at {
        return Err(CpError::with_reason(
            CpCode::Unauthenticated,
            "deploy.token_expired",
        ));
    }
    if !kv.put_new("deploy_token_used", &digest, "1").await? {
        return Err(CpError::with_reason(CpCode::Replayed, "deploy.token_used"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------------------------

impl DeployState {
    async fn verify_reports(
        &self,
        reports: &[SignedFacts],
        now: i64,
        durable: bool,
    ) -> Result<Vec<ServerFacts>, Fail> {
        let _guard = self.ledger_lock.lock().await;
        let mut ledger = SeqLedger::default();
        if durable {
            for report in reports {
                if let Some(text) = self.kv.get("facts_seq", &report.agent_id).await?
                    && let Ok(last) = text.parse::<u64>()
                {
                    ledger.last.insert(report.agent_id.clone(), last);
                }
            }
        }
        let mut facts = Vec::new();
        let mut failure = None;
        for report in reports {
            match report.verify(self.directory.as_ref(), &mut ledger, self.policy, now) {
                Ok(verified) => facts.push(verified),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        if durable {
            for (agent, sequence) in &ledger.last {
                self.kv
                    .put("facts_seq", agent, &sequence.to_string())
                    .await?;
            }
        }
        match failure {
            Some(error) => Err(wiz(&error)),
            None => Ok(facts),
        }
    }

    async fn load(&self, id: &str) -> Result<Record, Fail> {
        if !valid_plan_id(id) {
            return Err(CpError::new(CpCode::NotFound).into());
        }
        let text = self
            .kv
            .get("deploy_plan", id)
            .await?
            .ok_or_else(|| CpError::new(CpCode::NotFound))?;
        Ok(from_json(&text)?)
    }

    async fn save(&self, record: &Record) -> Result<(), Fail> {
        Ok(self
            .kv
            .put("deploy_plan", &record.id, &to_json(record)?)
            .await?)
    }

    /// Rebuilds the prepared plan from the stored facts (they are verified again, so a stale
    /// report or a since-revoked agent refuses) and proves it is the plan that was approved.
    async fn rebuild(&self, record: &Record, now: i64) -> Result<Prepared, Fail> {
        let facts = self.verify_reports(&record.facts, now, false).await?;
        let prepared = prepare(&record.topology, &facts, &self.catalogue).map_err(|e| wiz(&e))?;
        if prepared.plan_hash().as_str() != record.hash
            || prepared.plan().id().as_str() != record.id
        {
            return Err(named(
                CpCode::Conflict,
                "the stored plan no longer matches its hash; plan again",
            ));
        }
        Ok(prepared)
    }
}

fn steps_json(plan: &DeployPlan, states: &BTreeMap<usize, StepState>) -> Vec<Value> {
    plan.steps()
        .iter()
        .map(|step| {
            let state = states.get(&step.index).map_or("pending", |s| s.name());
            json!({
                "index": step.index, "kind": step.kind.name(), "title": step.title,
                "host": step.host.as_ref().map(|h| h.as_str().to_owned()),
                "operation": step.operation.id.as_str(), "depends_on": step.depends_on,
                "destructive": !step.idempotent, "state": state,
            })
        })
        .collect()
}

fn latest_states(journal: &dyn Journal) -> BTreeMap<usize, StepState> {
    let mut latest = BTreeMap::new();
    for entry in journal.entries() {
        latest.insert(entry.step, entry.state);
    }
    latest
}

fn record_json(record: &Record, plan: &DeployPlan, states: &BTreeMap<usize, StepState>) -> Value {
    json!({
        "id": record.id, "tenant_id": record.tenant, "plan_hash": record.hash,
        "state": record.state, "requested_by": record.requested_by,
        "approved_by": record.approval.as_ref().map(|a| a.approver.clone()),
        "steps": steps_json(plan, states),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanBody {
    tenant_id: String,
    topology: Topology,
}

async fn plan_handler(
    State(st): State<Arc<DeployState>>,
    Extension(call): Extension<OperatorCall>,
    body: Bytes,
) -> Response {
    let result = async {
        let req: PlanBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        if !valid_tenant(&req.tenant_id) || req.topology.nodes.len() > MAX_NODES {
            return Err(CpError::invalid(&["tenant_id", "topology"]).into());
        }
        call.scope.require(&req.tenant_id)?;
        req.topology.ensure_valid().map_err(|e| wiz(&e))?;
        let now = st.now();
        let mut reports = Vec::new();
        for host in req.topology.hosts() {
            // An unreachable agent leaves a hole the pre-flight names (`facts_present`).
            if let Ok(report) = st.link.collect_facts(host.as_str()).await {
                reports.push(report);
            }
        }
        let facts = st.verify_reports(&reports, now, true).await?;
        let prepared = prepare(&req.topology, &facts, &st.catalogue).map_err(|e| wiz(&e))?;
        let id = prepared.plan().id().as_str().to_owned();
        if let Some(text) = st.kv.get("deploy_plan", &id).await? {
            let old: Record = from_json(&text)?;
            if old.state == "done" {
                return Err(named(CpCode::Conflict, "this deployment is already done"));
            }
            if old.tenant != req.tenant_id {
                return Err(named(
                    CpCode::Conflict,
                    "the plan belongs to another tenant",
                ));
            }
            // A new plan of the same topology replaces the approval; its journal is kept, so
            // steps already done are never repeated.
            for hash in &old.tokens {
                st.kv.del("deploy_token", hash).await?;
            }
        }
        let record = Record {
            id,
            tenant: req.tenant_id,
            hash: prepared.plan_hash().as_str().to_owned(),
            topology: req.topology,
            facts: reports,
            requested_by: call.principal.clone(),
            created_at: now,
            approval: None,
            tokens: Vec::new(),
            state: "planned".to_owned(),
        };
        st.save(&record).await?;
        let journal = KvJournal::load(&st.kv, prepared.plan().id()).await?;
        let mut view = record_json(&record, prepared.plan(), &latest_states(&journal));
        view["preflight_ready"] = json!(prepared.preflight().is_ready());
        Ok::<_, Fail>((StatusCode::CREATED, json!({ "data": view })))
    }
    .await;
    render(&call, result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveBody {
    plan_hash: String,
}

async fn approve_handler(
    State(st): State<Arc<DeployState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let result = async {
        let req: ApproveBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        let mut record = st.load(&id).await?;
        call.scope.require(&record.tenant)?;
        if req.plan_hash != record.hash {
            return Err(named(
                CpCode::Conflict,
                "the plan hash is not the current one; the plan changed",
            ));
        }
        if record.state != "planned" {
            return Err(named(
                CpCode::Conflict,
                "the plan is not waiting for approval",
            ));
        }
        let now = st.now();
        let _prepared = st.rebuild(&record, now).await?;
        let command_id = call
            .command
            .as_ref()
            .map(|c| c.id.clone())
            .ok_or_else(|| CpError::new(CpCode::SignatureInvalid))?;
        let mut tokens = Vec::new();
        for host in record.topology.hosts() {
            let mut raw = [0u8; 32];
            fill_random(&mut raw);
            let token = format!("spot-enrol-{}", hex(&raw));
            let digest = sha256_hex(token.as_bytes());
            let row = TokenRow {
                plan: record.id.clone(),
                host: host.as_str().to_owned(),
                expires_at: now.saturating_add(TOKEN_TTL_SECS),
            };
            st.kv.put("deploy_token", &digest, &to_json(&row)?).await?;
            record.tokens.push(digest);
            tokens.push(
                json!({ "host": host.as_str(), "token": token, "expires_at": row.expires_at }),
            );
        }
        record.approval = Some(ApprovalRec {
            approver: call.principal.clone(),
            at: now,
            command_id,
            hash: record.hash.clone(),
        });
        record.state = "approved".to_owned();
        st.save(&record).await?;
        Ok::<_, Fail>((
            StatusCode::OK,
            json!({ "data": {
                "id": record.id, "state": record.state, "approved_by": call.principal,
                "enrolment_tokens": tokens, "tokens_shown_once": true,
            }}),
        ))
    }
    .await;
    render(&call, result)
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RunBody {
    #[serde(default)]
    confirm_steps: Vec<usize>,
}

async fn run_handler(
    State(st): State<Arc<DeployState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let result = run_inner(&st, &call, &id, &body).await;
    render(&call, result)
}

async fn run_inner(
    st: &Arc<DeployState>,
    call: &OperatorCall,
    id: &str,
    body: &[u8],
) -> Result<(StatusCode, Value), Fail> {
    let req: RunBody = if body.is_empty() {
        RunBody::default()
    } else {
        serde_json::from_slice(body).map_err(|_| CpError::invalid(&["body"]))?
    };
    let mut record = st.load(id).await?;
    call.scope.require(&record.tenant)?;
    if record.state == "done" {
        return Err(named(CpCode::Conflict, "this deployment is already done"));
    }
    let now = st.now();
    let prepared = st.rebuild(&record, now).await?;
    let identity = AdminIdentity::new(
        &call.principal,
        &[DEPLOY_PERMISSION],
        now.saturating_add(600),
    );
    let step_up = StepUp::new(&call.principal, now);
    let approval = record.approval.as_ref().map(|a| {
        Approval::new(
            PlanId::from(record.id.as_str()),
            PlanHash::from(a.hash.as_str()),
            &a.approver,
            a.at,
        )
    });
    let authorised = authorise(
        &prepared,
        Some(&identity),
        Some(&step_up),
        approval.as_ref(),
        now,
    )
    .map_err(|e| wiz(&e))?;

    let plan = prepared.plan().clone();
    let journal = KvJournal::load(&st.kv, plan.id()).await?;
    let done: BTreeSet<usize> = resume(&plan, &journal)
        .map_err(|e| wiz(&e))?
        .completed
        .into_iter()
        .collect();
    let confirmed: BTreeSet<usize> = req.confirm_steps.iter().copied().collect();
    let unconfirmed: Vec<String> = plan
        .steps()
        .iter()
        .filter(|s| !s.idempotent && !done.contains(&s.index) && !confirmed.contains(&s.index))
        .map(|s| s.index.to_string())
        .collect();
    if !unconfirmed.is_empty() {
        return Err(named(
            CpCode::ApprovalRequired,
            format!(
                "destructive steps need a per-step confirmation: {}",
                unconfirmed.join(",")
            ),
        ));
    }

    let lease_id = format!("deploy:{id}");
    let now_ms = now.saturating_mul(1000);
    if !st
        .kv
        .lease(&lease_id, &call.request_id, now_ms, RUN_LEASE_MS)
        .await?
    {
        return Err(named(CpCode::Conflict, "another run holds this deployment"));
    }
    record.state = "running".to_owned();
    st.save(&record).await?;

    let audit = KvAudit::load(&st.kv, plan.id()).await?;
    let outcome = {
        let st = Arc::clone(st);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let mut journal = journal;
            let mut audit = audit;
            let mut executor = LinkExecutor {
                handle,
                link: Arc::clone(&st.link),
            };
            let window = CommandWindow::new(COMMAND_LIFETIME_SECS)?;
            let clock = Arc::clone(&st.clock);
            let mut tick = move || clock.now_utc().unix_timestamp();
            let mut env = RunEnv {
                catalogue: &st.catalogue,
                journal: &mut journal,
                executor: &mut executor,
                signer: st.signer.as_ref(),
                window,
                clock: &mut tick,
                audit: &mut audit,
            };
            prepared.run(&authorised, &mut env)
        })
        .await
        .map_err(|_| CpError::with_reason(CpCode::Internal, "deploy.worker"))
    };
    let outcome = match outcome {
        Ok(result) => result,
        Err(error) => {
            st.kv.unlease(&lease_id, &call.request_id).await?;
            return Err(error.into());
        }
    };
    record.state = match &outcome {
        Ok(report) if report.is_complete() => "done",
        _ => "failed",
    }
    .to_owned();
    st.save(&record).await?;
    st.kv.unlease(&lease_id, &call.request_id).await?;
    let report = outcome.map_err(|e| wiz(&e))?;
    let failed = report.failed.as_ref().map(|f| {
        json!({ "step": f.step, "title": f.title, "code": f.code, "detail": f.detail,
                "cleanup_required": f.cleanup_required })
    });
    Ok((
        StatusCode::OK,
        json!({ "data": {
            "id": record.id, "state": record.state, "completed": report.completed,
            "done": report.done, "failed": failed, "submitted": report.submitted,
            "plan_len": report.plan_len,
        }}),
    ))
}

async fn status_handler(
    State(st): State<Arc<DeployState>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = async {
        let record = st.load(&id).await?;
        call.scope.require(&record.tenant)?;
        let plan = spot_deploy_wizard::plan(&record.topology).map_err(|e| wiz(&e))?;
        let journal = KvJournal::load(&st.kv, plan.id()).await?;
        let point = resume(&plan, &journal).map_err(|e| wiz(&e))?;
        let mut view = record_json(&record, &plan, &latest_states(&journal));
        view["next_step"] = json!(point.next);
        view["interrupted_step"] = json!(point.interrupted);
        Ok::<_, Fail>((StatusCode::OK, json!({ "data": view })))
    }
    .await;
    render(&call, result)
}

/// The routes of the module.
pub fn routes(state: Arc<DeployState>) -> Router {
    Router::new()
        .route("/cp/v1/deployments/plan", post(plan_handler))
        .route("/cp/v1/deployments/{id}", get(status_handler))
        .route("/cp/v1/deployments/{id}/approve", post(approve_handler))
        .route("/cp/v1/deployments/{id}/run", post(run_handler))
        .with_state(state)
}

/// Mounts the module over a prepared state.
pub fn register_with(ctx: &mut CpCtx, state: Arc<DeployState>) {
    ctx.operator_routes(routes(state));
    ctx.add_migration(DDL);
}

/// Registers the module with the collaborators installed by [`set_wiring`]. Without them the
/// module is not mounted and readiness reports it as not ready.
///
/// # Errors
/// `cp.internal` when there is no control database or the catalogue cannot be built.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let Some(wiring) = WIRING.get() else {
        ctx.add_readiness(Arc::new(|| -> BoxFuture<'static, bool> {
            Box::pin(async { false })
        }));
        return Ok(());
    };
    let pool = ctx
        .pool
        .clone()
        .ok_or_else(|| CpError::with_reason(CpCode::Internal, "deploy.no_control_database"))?;
    let kv: Arc<dyn Kv> = Arc::new(PgKv::new(pool));
    let state = DeployState::new(
        kv,
        Arc::clone(&ctx.clock),
        Wiring {
            directory: Arc::clone(&wiring.directory),
            link: Arc::clone(&wiring.link),
            signer: Arc::clone(&wiring.signer),
        },
    )?;
    register_with(ctx, state);
    Ok(())
}

/// An in-memory state for tests and the development profile.
///
/// # Errors
/// As [`DeployState::new`].
pub fn memory_state(clock: Arc<dyn Clock>, wiring: Wiring) -> Result<Arc<DeployState>, CpError> {
    DeployState::new(Arc::new(MemoryKv::new()), clock, wiring)
}

impl DeployState {
    /// The rows the module keeps, for the enrolment service and tests.
    #[must_use]
    pub fn kv(&self) -> Arc<dyn Kv> {
        Arc::clone(&self.kv)
    }
}
