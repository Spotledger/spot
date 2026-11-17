//! The `desired_state` module (`WP-ADM-08.T3`): `spot-desired-state` hosted by the control plane.
//!
//! - `PUT /cp/v1/groups/{group}` stores the desired state of a group (layers in the typed
//!   settings schema, validated by `DesiredFleet::new`) with the tenants the group belongs to.
//!   An operator sees a group only when every one of its tenants is in scope.
//! - Agents report their observed state at `POST /cp/v1/host/desired-state/report` (server =
//!   the host id of the certificate, tenant from the certificate). Each report, and a timer,
//!   evaluates the group: drift per server, a server silent past the deadline is `Silent`
//!   (unknown), and a pending remediation plan is stored for the drift.
//! - A plan is never executed on its own. `approve` takes the commands **already signed by the
//!   administrator** (one envelope per operation of each step) and stores them; the control
//!   plane has no signing key and cannot make or alter a command. `rehearse` and `run` follow;
//!   `run` drives `spot-desired-state`'s `BulkRun` (canary wave first, a failed canary or step
//!   halts the fleet, no later wave starts) and queues each step's stored commands for the
//!   agents' poll (`agent_cmd` rows); the agents' answers arrive as `agent_result` rows.
//! - Time units follow the library: microseconds for approvals and silence, milliseconds for
//!   the wave machine.
//!
//! The pre-signed commands live only as long as the command lifetime of the management API
//! (at most 300 s): a step whose command has expired when its wave starts fails and halts the
//! run, so the administrator approves again.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spot_core::Clock;
use spot_desired_state::{
    Approval, ApprovalDecision, ApprovalRules, ApprovedPlan, BulkPlan, BulkRun, DesiredFleet,
    DesiredState, Drift, Epoch, FailureKind, Fleet, FleetNode, FleetRollout, InMemoryObserved,
    Moment, NodeId, ObservedSnapshot, ObservedState, PlanStep, ReleaseId, RemediationPolicy,
    RolloutPlan, RunAction, RunGuards, Seed, Selector, Soak, StepFailure, StepRunner, Wave,
    WaveSize, detect_drift, remediate, risk_name,
};
use spot_mgmt_api::ports::BoxFuture;
use spot_settings::{RegistryBuilder, SettingsRegistry, load_seed_catalogue};

use super::tenants_kv::{Kv, PgKv, from_json, store_failed, to_json};
use super::tenants_state::finish;
use crate::auth::{HostIdentity, OperatorCall, RequestId};
use crate::{CpCode, CpCtx, CpError};

/// Registration order: clear of the other modules.
pub const ORDER: u32 = 180;

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
];
/// A server silent longer than this (microseconds) is unknown: five minutes.
pub const SILENCE_DEADLINE_MICROS: i64 = 300_000_000;
/// How old an approval may be when a run starts (microseconds): one hour.
pub const APPROVAL_MAX_AGE_MICROS: i64 = 3_600_000_000;
const MAX_REPORT_BYTES: usize = 256 * 1024;
const MAX_ENVELOPE_BYTES: usize = 16 * 1024;
const EVALUATE_EVERY_SECS: u64 = 60;
const MAX_TICKS: u64 = 100_000;

/// The tuning of the module (tests shorten the waits).
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// Milliseconds between wave-machine ticks of a run.
    pub tick_ms: u64,
    /// How often a step's verification polls the agent's answer.
    pub verify_attempts: u32,
    /// Milliseconds between those polls.
    pub verify_interval_ms: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            tick_ms: 1_000,
            verify_attempts: 120,
            verify_interval_ms: 1_000,
        }
    }
}

/// The state of the module.
pub struct DesiredStateState {
    kv: Arc<dyn Kv>,
    clock: Arc<dyn Clock>,
    registry: SettingsRegistry,
    tuning: Tuning,
}

impl DesiredStateState {
    /// A state over `kv`, validating desired state against `registry`.
    #[must_use]
    pub fn new(
        kv: Arc<dyn Kv>,
        clock: Arc<dyn Clock>,
        registry: SettingsRegistry,
        tuning: Tuning,
    ) -> Arc<Self> {
        Arc::new(Self {
            kv,
            clock,
            registry,
            tuning,
        })
    }

    fn now_secs(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
struct GroupRec {
    name: String,
    tenants: Vec<String>,
    layers: Vec<DesiredState>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StepCmd {
    id: String,
    expires_at: i64,
    text: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct ApprovalRec {
    approver: String,
    at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct PlanRec {
    group: String,
    requested_by: String,
    plan: BulkPlan,
    approval: Option<ApprovalRec>,
    commands: BTreeMap<String, Vec<StepCmd>>,
    rehearsed: bool,
    /// `pending`, `approved`, `rehearsed`, `running`, `complete`, `halted` or `aborted`.
    state: String,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn valid_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn visible(call: &OperatorCall, group: &GroupRec) -> bool {
    if group.tenants.is_empty() {
        call.scope.is_all()
    } else {
        group.tenants.iter().all(|t| call.scope.allows(t))
    }
}

impl DesiredStateState {
    async fn group(&self, call: &OperatorCall, name: &str) -> Result<GroupRec, CpError> {
        if !valid_name(name) {
            return Err(CpError::new(CpCode::NotFound));
        }
        let text = self
            .kv
            .get("ds_group", name)
            .await?
            .ok_or_else(|| CpError::new(CpCode::NotFound))?;
        let group: GroupRec = from_json(&text)?;
        if visible(call, &group) {
            Ok(group)
        } else {
            Err(CpError::new(CpCode::NotFound))
        }
    }

    async fn plan_rec(&self, group: &str, digest: &str) -> Result<PlanRec, CpError> {
        if !valid_digest(digest) {
            return Err(CpError::new(CpCode::NotFound));
        }
        let text = self
            .kv
            .get("ds_plan", digest)
            .await?
            .ok_or_else(|| CpError::new(CpCode::NotFound))?;
        let rec: PlanRec = from_json(&text)?;
        if rec.group == group {
            Ok(rec)
        } else {
            Err(CpError::new(CpCode::NotFound))
        }
    }

    async fn save_plan(&self, rec: &PlanRec) -> Result<(), CpError> {
        self.kv
            .put("ds_plan", rec.plan.digest().as_hex(), &to_json(rec)?)
            .await
    }

    fn fleet(&self, group: &GroupRec) -> Result<DesiredFleet, CpError> {
        DesiredFleet::new(self.registry.clone(), group.layers.clone())
            .map_err(|_| CpError::with_reason(CpCode::Invalid, "desired_state.invalid_layers"))
    }

    async fn observed(&self, fleet: &DesiredFleet) -> Result<InMemoryObserved, CpError> {
        let mut observed = InMemoryObserved::new();
        for server in fleet.servers() {
            let Some(seen) = self.kv.get("ds_seen", server.as_str()).await? else {
                continue;
            };
            let Ok(seen) = seen.parse::<i64>() else {
                continue;
            };
            match self.kv.get("ds_obs", server.as_str()).await? {
                Some(text) => observed.record_seen(server, from_json(&text)?, seen),
                None => observed.record_heartbeat(server, seen),
            }
        }
        Ok(observed)
    }

    /// Detects the drift of a group now.
    async fn drift(&self, group: &GroupRec) -> Result<Vec<Drift>, CpError> {
        let fleet = self.fleet(group)?;
        let observed = self.observed(&fleet).await?;
        Ok(detect_drift(
            &fleet,
            &observed,
            self.now_micros(),
            SILENCE_DEADLINE_MICROS,
        ))
    }

    /// Detects drift, stores it and a pending remediation plan for it (never executed).
    async fn evaluate(&self, group: &GroupRec, requested_by: &str) -> Result<Vec<Drift>, CpError> {
        let drifts = self.drift(group).await?;
        let plan = remediate(&drifts, &RemediationPolicy::standard());
        let keep = (!plan.is_empty()).then(|| plan.digest().as_hex().to_owned());
        // A pending plan that no longer matches the drift is stale: drop it.
        for (key, text) in self.kv.list("ds_plan").await? {
            let rec: PlanRec = from_json(&text)?;
            if rec.group == group.name && rec.state == "pending" && keep.as_ref() != Some(&key) {
                self.kv.del("ds_plan", &key).await?;
            }
        }
        if let Some(digest) = keep {
            let fresh = match self.kv.get("ds_plan", &digest).await? {
                None => true,
                Some(text) => matches!(
                    from_json::<PlanRec>(&text)?.state.as_str(),
                    "complete" | "halted" | "aborted"
                ),
            };
            if fresh {
                self.save_plan(&PlanRec {
                    group: group.name.clone(),
                    requested_by: requested_by.to_owned(),
                    plan,
                    approval: None,
                    commands: BTreeMap::new(),
                    rehearsed: false,
                    state: "pending".to_owned(),
                })
                .await?;
            }
        }
        Ok(drifts)
    }

    async fn evaluate_all(&self) -> Result<(), CpError> {
        for (_, text) in self.kv.list("ds_group").await? {
            let group: GroupRec = from_json(&text)?;
            let _ = self.evaluate(&group, "system").await;
        }
        Ok(())
    }
}

fn plan_json(rec: &PlanRec) -> Value {
    json!({
        "digest": rec.plan.digest().as_hex(), "group": rec.group, "state": rec.state,
        "requested_by": rec.requested_by, "approved_by": rec.approval.as_ref().map(|a| a.approver.clone()),
        "size": rec.plan.size(), "risk": risk_name(rec.plan.risk), "rehearsed": rec.rehearsed,
        "steps": serde_json::to_value(&rec.plan.steps).unwrap_or(Value::Null),
        "skipped": serde_json::to_value(&rec.plan.skipped).unwrap_or(Value::Null),
    })
}

// ---------------------------------------------------------------------------------------------
// Operator handlers
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupBody {
    #[serde(default)]
    tenants: Vec<String>,
    layers: Vec<DesiredState>,
}

async fn put_group(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let result = async {
        let req: GroupBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        if !valid_name(&name) || req.layers.iter().any(|l| l.group != name) {
            return Err(CpError::invalid(&["group", "layers"]));
        }
        let group = GroupRec {
            name: name.clone(),
            tenants: req.tenants,
            layers: req.layers,
        };
        if !visible(&call, &group) {
            return Err(CpError::new(CpCode::NotFound));
        }
        if let Some(text) = st.kv.get("ds_group", &name).await?
            && !visible(&call, &from_json::<GroupRec>(&text)?)
        {
            return Err(CpError::new(CpCode::NotFound));
        }
        st.fleet(&group)?;
        st.kv.put("ds_group", &name, &to_json(&group)?).await?;
        Ok((StatusCode::OK, json!({ "data": { "group": name } })))
    }
    .await;
    finish(&call, result)
}

async fn list_groups(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
) -> Response {
    let result = async {
        let mut groups = Vec::new();
        for (_, text) in st.kv.list("ds_group").await? {
            let group: GroupRec = from_json(&text)?;
            if visible(&call, &group) {
                groups.push(json!({ "group": group.name, "tenants": group.tenants }));
            }
        }
        Ok((StatusCode::OK, json!({ "data": groups })))
    }
    .await;
    finish(&call, result)
}

async fn get_group(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path(name): Path<String>,
) -> Response {
    let result = async {
        let group = st.group(&call, &name).await?;
        let drifts = st.drift(&group).await?;
        let silent = drifts.iter().filter(|d| d.is_silent()).count();
        Ok((
            StatusCode::OK,
            json!({ "data": {
                "group": group.name, "tenants": group.tenants, "layers": group.layers,
                "drift_count": drifts.len(), "silent_count": silent,
            }}),
        ))
    }
    .await;
    finish(&call, result)
}

async fn get_drift(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path(name): Path<String>,
) -> Response {
    let result = async {
        let group = st.group(&call, &name).await?;
        let drifts = st.drift(&group).await?;
        let view: Vec<Value> = drifts
            .iter()
            .map(|d| {
                json!({
                    "server": d.server.as_str(), "subject": d.subject.label(),
                    "state": if d.is_silent() { "unknown" } else { "drifted" },
                    "risk": risk_name(d.risk), "description": d.describe(),
                })
            })
            .collect();
        Ok((StatusCode::OK, json!({ "data": view })))
    }
    .await;
    finish(&call, result)
}

async fn create_plan(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path(name): Path<String>,
) -> Response {
    let result = async {
        let group = st.group(&call, &name).await?;
        let drifts = st.evaluate(&group, &call.principal).await?;
        let plan = remediate(&drifts, &RemediationPolicy::standard());
        if plan.is_empty() {
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "desired_state.nothing_to_do",
            ));
        }
        let mut rec = st.plan_rec(&name, plan.digest().as_hex()).await?;
        if rec.state == "pending" && rec.requested_by != call.principal {
            rec.requested_by.clone_from(&call.principal);
            st.save_plan(&rec).await?;
        }
        Ok((StatusCode::OK, json!({ "data": plan_json(&rec) })))
    }
    .await;
    finish(&call, result)
}

async fn list_plans(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path(name): Path<String>,
) -> Response {
    let result = async {
        st.group(&call, &name).await?;
        let mut plans = Vec::new();
        for (_, text) in st.kv.list("ds_plan").await? {
            let rec: PlanRec = from_json(&text)?;
            if rec.group == name {
                plans.push(plan_json(&rec));
            }
        }
        Ok((StatusCode::OK, json!({ "data": plans })))
    }
    .await;
    finish(&call, result)
}

async fn get_plan(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path((name, digest)): Path<(String, String)>,
) -> Response {
    let result = async {
        st.group(&call, &name).await?;
        let rec = st.plan_rec(&name, &digest).await?;
        let mut view = plan_json(&rec);
        if let Some(text) = st.kv.get("ds_run", &digest).await? {
            view["run"] = from_json::<Value>(&text)?;
        }
        Ok((StatusCode::OK, json!({ "data": view })))
    }
    .await;
    finish(&call, result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveBody {
    commands: Vec<StepCommands>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepCommands {
    step: String,
    envelopes: Vec<String>,
}

/// Checks that each step has exactly the pre-signed commands it needs: one signed envelope per
/// operation, naming that operation, addressed to the step's server and not yet expired.
fn check_commands(
    plan: &BulkPlan,
    body: &ApproveBody,
    now: i64,
) -> Result<BTreeMap<String, Vec<StepCmd>>, CpError> {
    let bad = || CpError::invalid(&["commands"]);
    if body.commands.len() != plan.steps.len() {
        return Err(bad());
    }
    let mut out = BTreeMap::new();
    for step in &plan.steps {
        let entry = body
            .commands
            .iter()
            .find(|c| c.step == step.id)
            .ok_or_else(bad)?;
        if entry.envelopes.len() != step.operations.len() {
            return Err(bad());
        }
        let mut cmds = Vec::new();
        for (operation, text) in step.operations.iter().zip(&entry.envelopes) {
            let bytes = URL_SAFE_NO_PAD.decode(text.as_bytes()).map_err(|_| bad())?;
            if bytes.len() > MAX_ENVELOPE_BYTES {
                return Err(bad());
            }
            let envelope = spot_mgmt_api::wire::decode(&bytes).map_err(|_| bad())?;
            let command = &envelope.command;
            if command.op != operation.id.as_str()
                || command.audience != step.server.as_str()
                || envelope.signatures.is_empty()
                || command.expires_at <= now
            {
                return Err(bad());
            }
            cmds.push(StepCmd {
                id: command.id.clone(),
                expires_at: command.expires_at,
                text: text.clone(),
            });
        }
        out.insert(step.id.clone(), cmds);
    }
    Ok(out)
}

struct Decided {
    approvers: Vec<String>,
    at: i64,
}

impl Approval for Decided {
    fn decide(&self, _plan: &BulkPlan, _now: i64) -> ApprovalDecision {
        ApprovalDecision::Approved {
            approvers: self.approvers.clone(),
            decided_at: self.at,
        }
    }
}

fn rules() -> ApprovalRules {
    ApprovalRules::new(APPROVAL_MAX_AGE_MICROS, 1)
}

fn not_approved() -> CpError {
    CpError::with_reason(CpCode::ApprovalRequired, "desired_state.not_approved")
}

async fn approve(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path((name, digest)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let result = async {
        st.group(&call, &name).await?;
        let req: ApproveBody =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        let mut rec = st.plan_rec(&name, &digest).await?;
        if rec.state != "pending" {
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "desired_state.not_pending",
            ));
        }
        let commands = check_commands(&rec.plan, &req, st.now_secs())?;
        let now = st.now_micros();
        let decided = Decided {
            approvers: vec![call.principal.clone()],
            at: now,
        };
        // The library refuses the requester as approver and an approval that is not fresh.
        ApprovedPlan::authorise(&decided, &rec.plan, &rec.requested_by, &rules(), now)
            .map_err(|_| not_approved())?;
        rec.approval = Some(ApprovalRec {
            approver: call.principal.clone(),
            at: now,
        });
        rec.commands = commands;
        rec.state = "approved".to_owned();
        st.save_plan(&rec).await?;
        Ok((StatusCode::OK, json!({ "data": plan_json(&rec) })))
    }
    .await;
    finish(&call, result)
}

async fn rehearse_plan(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path((name, digest)): Path<(String, String)>,
) -> Response {
    let result = async {
        st.group(&call, &name).await?;
        let mut rec = st.plan_rec(&name, &digest).await?;
        if rec.approval.is_none() {
            return Err(not_approved());
        }
        let report = rec.plan.rehearse();
        rec.rehearsed = report.passed();
        if rec.rehearsed && rec.state == "approved" {
            "rehearsed".clone_into(&mut rec.state);
        }
        st.save_plan(&rec).await?;
        Ok((
            StatusCode::OK,
            json!({ "data": {
                "passed": report.passed(), "checked_steps": report.checked_steps(),
                "servers": report.servers(), "operations": report.operations(),
                "problems": report.problems().len(),
            }}),
        ))
    }
    .await;
    finish(&call, result)
}

async fn run_plan(
    State(st): State<Arc<DesiredStateState>>,
    Extension(call): Extension<OperatorCall>,
    Path((name, digest)): Path<(String, String)>,
) -> Response {
    let result = async {
        st.group(&call, &name).await?;
        let mut rec = st.plan_rec(&name, &digest).await?;
        let approval = rec.approval.clone().ok_or_else(not_approved)?;
        if rec.state != "rehearsed" || !rec.rehearsed {
            return Err(not_approved());
        }
        let now = st.now_micros();
        let decided = Decided {
            approvers: vec![approval.approver],
            at: approval.at,
        };
        let proof = ApprovedPlan::authorise(&decided, &rec.plan, &rec.requested_by, &rules(), now)
            .map_err(|_| not_approved())?;
        let rehearsal = rec.plan.rehearse();
        if !rehearsal.passed() {
            return Err(not_approved());
        }
        "running".clone_into(&mut rec.state);
        st.save_plan(&rec).await?;
        let handle = tokio::runtime::Handle::current();
        let worker = Arc::clone(&st);
        let started = rec.clone();
        // The run lives on one blocking thread: `BulkRun` is not `Send`.
        drop(tokio::task::spawn_blocking(move || {
            worker.drive(&handle, &started, &proof, &rehearsal);
        }));
        Ok((StatusCode::ACCEPTED, json!({ "data": plan_json(&rec) })))
    }
    .await;
    finish(&call, result)
}

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

struct KvObserved {
    handle: tokio::runtime::Handle,
    kv: Arc<dyn Kv>,
}

impl ObservedState for KvObserved {
    fn snapshot(&self, server: &NodeId) -> Option<ObservedSnapshot> {
        let text = self
            .handle
            .block_on(self.kv.get("ds_obs", server.as_str()))
            .ok()??;
        serde_json::from_str(&text).ok()
    }

    fn last_seen(&self, server: &NodeId) -> Option<i64> {
        self.handle
            .block_on(self.kv.get("ds_seen", server.as_str()))
            .ok()??
            .parse()
            .ok()
    }
}

/// Queues the pre-signed commands of a step for the agent's poll and reads the agent's answers.
struct QueueRunner {
    handle: tokio::runtime::Handle,
    kv: Arc<dyn Kv>,
    clock: Arc<dyn Clock>,
    commands: BTreeMap<String, Vec<StepCmd>>,
    tuning: Tuning,
}

impl StepRunner for QueueRunner {
    fn apply(&self, server: &NodeId, step: &PlanStep) -> Result<(), StepFailure> {
        let fail = |kind, detail: &str| StepFailure::new(kind, detail);
        let commands = self
            .commands
            .get(&step.id)
            .ok_or_else(|| fail(FailureKind::Timeout, "the step has no stored command"))?;
        let now = self.clock.now_utc().unix_timestamp();
        if commands.iter().any(|c| c.expires_at <= now) {
            return Err(fail(
                FailureKind::Timeout,
                "the signed command expired before its wave started; approve again",
            ));
        }
        for command in commands {
            let key = format!("{}/{}", server.as_str(), command.id);
            self.handle
                .block_on(self.kv.put("agent_cmd", &key, &command.text))
                .map_err(|_| fail(FailureKind::Timeout, "the command could not be queued"))?;
        }
        Ok(())
    }

    fn verify(&self, _server: &NodeId, step: &PlanStep) -> bool {
        let Some(commands) = self.commands.get(&step.id) else {
            return false;
        };
        for command in commands {
            let mut answered = None;
            for _ in 0..self.tuning.verify_attempts {
                if let Ok(Some(text)) = self
                    .handle
                    .block_on(self.kv.get("agent_result", &command.id))
                {
                    answered = Some(text);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(
                    self.tuning.verify_interval_ms,
                ));
            }
            if answered.as_deref() != Some("ok") {
                return false;
            }
        }
        true
    }
}

fn waves() -> Vec<Wave> {
    vec![
        Wave::new("canary", Selector::all(), WaveSize::Nodes(1), Soak::ZERO).as_canary(),
        Wave::new("fleet", Selector::all(), WaveSize::All, Soak::ZERO),
    ]
}

impl DesiredStateState {
    fn set_run(&self, handle: &tokio::runtime::Handle, digest: &str, view: &Value) {
        let _ = handle.block_on(self.kv.put("ds_run", digest, &view.to_string()));
    }

    fn drive(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        rec: &PlanRec,
        proof: &ApprovedPlan,
        rehearsal: &spot_desired_state::RehearsalReport,
    ) {
        let digest = rec.plan.digest().as_hex().to_owned();
        let outcome = self.drive_inner(handle, rec, proof, rehearsal, &digest);
        let (state, note) = match outcome {
            Ok(RunAction::Complete { .. }) => ("complete", "every wave finished"),
            Ok(RunAction::Halted { .. }) => (
                "halted",
                "a failure halted the fleet; no later wave started",
            ),
            Ok(_) => ("aborted", "the run was aborted"),
            Err(()) => ("aborted", "the run could not start"),
        };
        self.set_run(handle, &digest, &json!({ "state": state, "note": note }));
        let mut updated = rec.clone();
        state.clone_into(&mut updated.state);
        let _ = handle.block_on(self.save_plan(&updated));
    }

    fn drive_inner(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        rec: &PlanRec,
        proof: &ApprovedPlan,
        rehearsal: &spot_desired_state::RehearsalReport,
        digest: &str,
    ) -> Result<RunAction, ()> {
        let fleet = Fleet::new(
            rec.plan
                .servers()
                .into_iter()
                .map(|s| FleetNode::new(s, rec.group.clone()))
                .collect(),
        );
        let release = ReleaseId::new(format!("bulk-{}", digest.get(..12).unwrap_or(digest)))
            .map_err(|_| ())?;
        let rollout = FleetRollout::new(
            RolloutPlan::new(release, Epoch::new(1), waves()),
            &fleet,
            Seed::new(7),
        )
        .map_err(|_| ())?;
        let guards = RunGuards::new(
            Arc::new(KvObserved {
                handle: handle.clone(),
                kv: Arc::clone(&self.kv),
            }),
            SILENCE_DEADLINE_MICROS,
            rules(),
        );
        let mut run = BulkRun::start(
            rec.plan.clone(),
            proof,
            rehearsal,
            Box::new(rollout),
            guards,
            Moment::ZERO,
            self.now_micros(),
        )
        .map_err(|_| ())?;
        let runner = QueueRunner {
            handle: handle.clone(),
            kv: Arc::clone(&self.kv),
            clock: Arc::clone(&self.clock),
            commands: rec.commands.clone(),
            tuning: self.tuning,
        };
        for tick in 0..MAX_TICKS {
            let now = Moment::from_millis(tick.saturating_mul(self.tuning.tick_ms));
            let action = run.tick(&runner, now, self.now_micros());
            self.set_run(
                handle,
                digest,
                &json!({
                    "state": "running", "waves_started": run.waves_started(),
                    "applied": run.applied_servers().len(), "verified": run.verified_servers().len(),
                    "failures": run.failures().len(),
                }),
            );
            if matches!(
                action,
                RunAction::Complete { .. } | RunAction::Halted { .. } | RunAction::Aborted { .. }
            ) {
                return Ok(action);
            }
            std::thread::sleep(std::time::Duration::from_millis(self.tuning.tick_ms));
        }
        Err(())
    }
}

// ---------------------------------------------------------------------------------------------
// Host report
// ---------------------------------------------------------------------------------------------

async fn host_report(
    State(st): State<Arc<DesiredStateState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    body: Bytes,
) -> Response {
    let result = async {
        if body.len() > MAX_REPORT_BYTES {
            return Err(CpError::new(CpCode::BodyTooLarge));
        }
        let mut snapshot: ObservedSnapshot =
            serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]))?;
        let server =
            NodeId::new(host.host_id.clone()).map_err(|_| CpError::new(CpCode::Forbidden))?;
        // The group is found from the certificate, never from the body.
        let mut found = None;
        for (_, text) in st.kv.list("ds_group").await? {
            let group: GroupRec = from_json(&text)?;
            if group.tenants.contains(&host.tenant)
                && st
                    .fleet(&group)
                    .is_ok_and(|f| f.servers().contains(&server))
            {
                found = Some(group);
                break;
            }
        }
        let group = found.ok_or_else(|| CpError::new(CpCode::NotFound))?;
        let now = st.now_micros();
        snapshot.reported_at = now;
        st.kv
            .put("ds_obs", server.as_str(), &to_json(&snapshot)?)
            .await?;
        st.kv
            .put("ds_seen", server.as_str(), &now.to_string())
            .await?;
        st.evaluate(&group, "system").await?;
        Ok((StatusCode::OK, json!({ "data": { "accepted": true } })))
    }
    .await;
    match result {
        Ok((status, value)) => (status, axum::Json(value)).into_response(),
        Err(error) => error.into_response_with(&request.0),
    }
}

/// The routes of the module: operator routes and host routes.
pub fn routes(state: Arc<DesiredStateState>) -> (Router, Router) {
    let operator = Router::new()
        .route("/cp/v1/groups", get(list_groups))
        .route("/cp/v1/groups/{group}", put(put_group).get(get_group))
        .route("/cp/v1/groups/{group}/drift", get(get_drift))
        .route(
            "/cp/v1/groups/{group}/plans",
            post(create_plan).get(list_plans),
        )
        .route("/cp/v1/groups/{group}/plans/{digest}", get(get_plan))
        .route(
            "/cp/v1/groups/{group}/plans/{digest}/approve",
            post(approve),
        )
        .route(
            "/cp/v1/groups/{group}/plans/{digest}/rehearse",
            post(rehearse_plan),
        )
        .route("/cp/v1/groups/{group}/plans/{digest}/run", post(run_plan))
        .with_state(Arc::clone(&state));
    let host = Router::new()
        .route("/cp/v1/host/desired-state/report", post(host_report))
        .with_state(state);
    (operator, host)
}

/// Mounts the module over a prepared state and starts its evaluation timer.
pub fn register_with(ctx: &mut CpCtx, state: Arc<DesiredStateState>) {
    let (operator, host) = routes(Arc::clone(&state));
    ctx.operator_routes(operator);
    ctx.host_routes(host);
    ctx.add_migration(DDL);
    let timer = state;
    ctx.add_task(Box::new(move |mut stop| -> BoxFuture<'static, ()> {
        Box::pin(async move {
            loop {
                let stopped = tokio::select! {
                    changed = stop.changed() => changed.is_err() || *stop.borrow(),
                    () = tokio::time::sleep(std::time::Duration::from_secs(EVALUATE_EVERY_SECS)) => false,
                };
                if stopped {
                    break;
                }
                let _ = timer.evaluate_all().await;
            }
        })
    }));
}

/// Registers the module over the control database with the seed settings catalogue.
///
/// # Errors
/// `cp.internal` when there is no control database or the settings registry cannot be built.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let pool = ctx.pool.clone().ok_or_else(|| {
        CpError::with_reason(CpCode::Internal, "desired_state.no_control_database")
    })?;
    let specs = load_seed_catalogue().map_err(|_| store_failed())?;
    let registry = RegistryBuilder::new()
        .register_all(specs)
        .build()
        .map_err(|_| store_failed())?;
    let kv: Arc<dyn Kv> = Arc::new(PgKv::new(pool));
    register_with(
        ctx,
        DesiredStateState::new(kv, Arc::clone(&ctx.clock), registry, Tuning::default()),
    );
    Ok(())
}
