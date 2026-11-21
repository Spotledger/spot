//! The database and runtime operations on the agent (WP-ADM-06.T2): real executors for the
//! `spot-agent-runtime` ports, registered under the namespace `runtime`.
//!
//! Enabling the `runtime` namespace in `allowed_modules` enables the whole set the
//! `spot-agent-runtime` catalogue declares (`db.*`, `node.*`, `runtime.*`).
//!
//! # No SQL and no paths in the agent
//!
//! Every port is an executor over [`RuntimeExec`], which runs a **named, typed template of
//! the privileged helper** (`sudo -n spot-agent-priv run <template>`, parameters validated
//! in the agent and again in the helper). The agent never builds SQL or a path: the helper
//! owns the `psql` / `pg_ctl` / `spot` command lines, with the parameter names from the
//! `spot-agent-runtime` allow-list. [`REQUIRED_TEMPLATES`] lists the templates this file
//! calls; until they exist in `privilege/templates.rs` every call fails closed
//! (`agent.privilege_unknown_operation`).
//!
//! Every change is read back by the library (`set_parameter` reads the value, reports
//! reload versus restart, and the runtime health is read after start), so before/after and
//! verification are reported by the module itself.

use std::sync::Arc;

use spot_agent_core::HandlerRegistry;
use spot_agent_runtime::RuntimeResult;
use spot_agent_runtime::replica::ReplicaRole;
use spot_agent_runtime::{
    ApplyPort, Approval, ApprovalDecision, ApprovalPort, ReleasePlanner, RuntimeCatalogue,
    RuntimeControl, RuntimeError, RuntimeHandler, RuntimeModule, RuntimePorts, SettingsPorts,
    StartReport, StopReport,
};
use spot_agent_runtime::{
    BackupEntry, BackupKind, BackupPort, BackupRequest, BackupStarted, DrainReport, PgAdminPort,
    ReplicaLag, ReplicaPort, ReplicaStatus, StandbyOutcome, StandbyRequest,
};
use spot_mgmt_ops::OperationModule as _;
use spot_migration_apply::{ApplyOutcome, RecoveryOutcome};
use spot_migration_plan::ReleasePlan;
use spot_snapshot::VerifyReport;

use super::HandlerCtx;
use crate::privilege::HELPER_PATH;
use crate::privilege::exec::{ChildRunner, ChildSpec, StdChildRunner};
use crate::privilege::helper::HelperReply;

/// The operation namespace.
pub const NAMESPACE: &str = "runtime";

/// The privileged helper templates this file calls: name and parameter slots. The helper
/// builds the command lines itself from these fixed names (no SQL, no path parameter).
pub const REQUIRED_TEMPLATES: &[(&str, &str)] = &[
    (
        "pg.param.read",
        "name: enum of the runtime allow-list -> stdout `<value>\\t<pending_restart t|f>`",
    ),
    (
        "pg.param.set",
        "name: allow-listed enum, value: validated by type and bounds (ALTER SYSTEM SET)",
    ),
    ("pg.reload", "no parameters (pg_reload_conf())"),
    (
        "pg.restart",
        "no parameters (service restart; risk destructive)",
    ),
    (
        "pg.replica.status",
        "stdout role=/streaming=/primary_host=/receive_lsn=/replay_lsn= lines",
    ),
    ("pg.replica.lag", "stdout bytes=/seconds=/streaming= lines"),
    (
        "pg.standby.create",
        "host, port:int, slot|- (the data dir is the node's fixed one)",
    ),
    (
        "pg.promote",
        "no parameters (risk destructive; the module requires confirmation upstream)",
    ),
    ("pg.primary.reachable", "stdout true|false"),
    (
        "backup.run",
        "kind: base|logical, label: [A-Za-z0-9_.-]{1,64} (calls `spot backup run` as the service user)",
    ),
    (
        "backup.list",
        "stdout one `id\\tkind\\tlabel\\tbytes\\tcreated_at\\tverified` per line",
    ),
    (
        "backup.verify",
        "id: [A-Za-z0-9_.-]{1,64} -> stdout files_checked=/bytes_checked=",
    ),
    (
        "runtime.stop",
        "drain_secs:int -> stdout drained=/in_flight=/waited_secs=/stopped=",
    ),
    (
        "runtime.start",
        "no parameters -> stdout started= plus note lines",
    ),
    (
        "node.reload",
        "no parameters (`spot reload` after the [services.*] settings changed)",
    ),
];

/// How a template is run. The production implementation calls the privileged helper; tests
/// use a fake.
pub trait RuntimeExec: Send + Sync {
    /// Runs template `name` with `params` and returns its stdout.
    ///
    /// # Errors
    /// [`RuntimeError::Unavailable`] (never carrying output or secrets) when the helper
    /// refuses, fails or times out.
    fn run(&self, name: &str, params: &[String]) -> RuntimeResult<String>;
}

/// The production executor: `sudo -n <helper> run <name>` with the parameters on stdin.
pub struct HelperExec {
    child: Arc<dyn ChildRunner>,
}

impl HelperExec {
    /// An executor over the real process runner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            child: Arc::new(StdChildRunner),
        }
    }

    /// An executor over `child` (tests).
    #[must_use]
    pub fn with_runner(child: Arc<dyn ChildRunner>) -> Self {
        Self { child }
    }
}

impl Default for HelperExec {
    fn default() -> Self {
        Self::new()
    }
}

fn unavailable(what: &str) -> RuntimeError {
    RuntimeError::Unavailable {
        what: what.to_owned(),
    }
}

impl RuntimeExec for HelperExec {
    fn run(&self, name: &str, params: &[String]) -> RuntimeResult<String> {
        let stdin = serde_json::json!({ "params": params }).to_string() + "\n";
        let raw = self
            .child
            .run(&ChildSpec {
                program: "/usr/bin/sudo".to_owned(),
                args: vec![
                    "-n".to_owned(),
                    HELPER_PATH.to_owned(),
                    "run".to_owned(),
                    name.to_owned(),
                ],
                timeout_secs: 3600,
                output_cap: 1 << 20,
                stdin: Some(stdin.into_bytes()),
            })
            .map_err(|_| unavailable("the privilege helper"))?;
        let reply: HelperReply = serde_json::from_str(raw.stdout.trim())
            .map_err(|_| unavailable("the privilege helper reply"))?;
        if !reply.ok {
            return Err(unavailable(
                reply
                    .code
                    .as_deref()
                    .unwrap_or("the privilege helper refused"),
            ));
        }
        if reply.timed_out {
            return Err(unavailable("the operation timed out"));
        }
        if reply.status != 0 {
            return Err(unavailable("the operation failed"));
        }
        Ok(reply.stdout)
    }
}

/// `key=value` lines as a lookup.
fn kv(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        l.strip_prefix(key)
            .and_then(|r| r.strip_prefix('='))
            .map(|v| v.trim().to_owned())
    })
}

fn bad(what: &str) -> RuntimeError {
    RuntimeError::Unavailable {
        what: format!("an unparsable {what} answer"),
    }
}

fn parse_bool(v: Option<String>, what: &str) -> RuntimeResult<bool> {
    match v.as_deref() {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(bad(what)),
    }
}

fn parse_u64(v: Option<String>, what: &str) -> RuntimeResult<u64> {
    v.and_then(|s| s.parse().ok()).ok_or_else(|| bad(what))
}

/// PostgreSQL parameters through the helper.
pub struct PgExec(pub Arc<dyn RuntimeExec>);

impl PgAdminPort for PgExec {
    fn read(&self, name: &str) -> RuntimeResult<(String, bool)> {
        let out = self.0.run("pg.param.read", &[name.to_owned()])?;
        let (value, pending) = out
            .trim_end()
            .split_once('\t')
            .ok_or_else(|| bad("parameter"))?;
        Ok((value.to_owned(), pending == "t"))
    }
    fn alter_system(&self, name: &str, value: &str) -> RuntimeResult<()> {
        self.0
            .run("pg.param.set", &[name.to_owned(), value.to_owned()])
            .map(|_| ())
    }
    fn reload(&self) -> RuntimeResult<()> {
        self.0.run("pg.reload", &[]).map(|_| ())
    }
    fn restart(&self) -> RuntimeResult<()> {
        self.0.run("pg.restart", &[]).map(|_| ())
    }
}

/// Replicas through the helper.
pub struct ReplicaExec(pub Arc<dyn RuntimeExec>);

impl ReplicaPort for ReplicaExec {
    fn status(&self) -> RuntimeResult<ReplicaStatus> {
        let out = self.0.run("pg.replica.status", &[])?;
        let role = match kv(&out, "role").as_deref() {
            Some("primary") => ReplicaRole::Primary,
            Some("standby") => ReplicaRole::Standby,
            _ => return Err(bad("replica")),
        };
        let opt = |k: &str| kv(&out, k).filter(|v| v != "-");
        Ok(ReplicaStatus {
            role,
            streaming: parse_bool(kv(&out, "streaming"), "replica")?,
            primary_host: opt("primary_host"),
            receive_lsn: opt("receive_lsn"),
            replay_lsn: opt("replay_lsn"),
        })
    }
    fn lag(&self) -> RuntimeResult<ReplicaLag> {
        let out = self.0.run("pg.replica.lag", &[])?;
        Ok(ReplicaLag {
            bytes: parse_u64(kv(&out, "bytes"), "lag")?,
            seconds: kv(&out, "seconds").and_then(|s| s.parse().ok()),
            streaming: parse_bool(kv(&out, "streaming"), "lag")?,
        })
    }
    fn create_standby(&self, request: &StandbyRequest) -> RuntimeResult<StandbyOutcome> {
        let slot = request.slot.clone().unwrap_or_else(|| "-".to_owned());
        let out = self.0.run(
            "pg.standby.create",
            &[request.host.clone(), request.port.to_string(), slot],
        )?;
        Ok(StandbyOutcome {
            data_dir: request.data_dir.clone(),
            host: request.host.clone(),
            port: request.port,
            slot: request.slot.clone(),
            notes: out
                .lines()
                .map(str::to_owned)
                .filter(|l| !l.is_empty())
                .take(20)
                .collect(),
        })
    }
    fn promote(&self) -> RuntimeResult<()> {
        self.0.run("pg.promote", &[]).map(|_| ())
    }
    fn primary_reachable(&self) -> RuntimeResult<bool> {
        parse_bool(
            Some(self.0.run("pg.primary.reachable", &[])?.trim().to_owned()),
            "reachability",
        )
    }
}

/// Backups through the helper (`spot backup run` as the service user).
pub struct BackupExec(pub Arc<dyn RuntimeExec>);

impl BackupPort for BackupExec {
    fn start(&self, request: &BackupRequest, _now: i64) -> RuntimeResult<BackupStarted> {
        let out = self.0.run(
            "backup.run",
            &[request.kind.name().to_owned(), request.label.clone()],
        )?;
        Ok(BackupStarted {
            id: kv(&out, "id").ok_or_else(|| bad("backup"))?,
            location: kv(&out, "location").ok_or_else(|| bad("backup"))?,
            kind: request.kind,
            created_at: kv(&out, "created_at")
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad("backup"))?,
        })
    }
    fn list(&self) -> RuntimeResult<Vec<BackupEntry>> {
        let out = self.0.run("backup.list", &[])?;
        let mut entries = Vec::new();
        for line in out.lines().filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split('\t').collect();
            let [id, kind, label, bytes, created, verified] = f.as_slice() else {
                return Err(bad("backup list"));
            };
            entries.push(BackupEntry {
                id: (*id).to_owned(),
                kind: BackupKind::from_name(kind).ok_or_else(|| bad("backup list"))?,
                label: (*label).to_owned(),
                bytes: bytes.parse().map_err(|_| bad("backup list"))?,
                created_at: created.parse().map_err(|_| bad("backup list"))?,
                verified: *verified == "true",
            });
        }
        Ok(entries)
    }
    fn verify(&self, id: &str) -> RuntimeResult<VerifyReport> {
        let out = self.0.run("backup.verify", &[id.to_owned()])?;
        Ok(VerifyReport {
            files_checked: parse_u64(kv(&out, "files_checked"), "verify")?,
            bytes_checked: parse_u64(kv(&out, "bytes_checked"), "verify")?,
        })
    }
}

/// Runtime service control through the helper.
pub struct RuntimeServiceExec(pub Arc<dyn RuntimeExec>);

impl RuntimeControl for RuntimeServiceExec {
    fn stop(&self, drain_secs: u64, _now: i64) -> RuntimeResult<StopReport> {
        let out = self.0.run("runtime.stop", &[drain_secs.to_string()])?;
        Ok(StopReport {
            drain: DrainReport {
                drained: parse_bool(kv(&out, "drained"), "stop")?,
                in_flight: parse_u64(kv(&out, "in_flight"), "stop")?,
                waited_secs: parse_u64(kv(&out, "waited_secs"), "stop")?,
            },
            stopped: parse_bool(kv(&out, "stopped"), "stop")?,
        })
    }
    fn start(&self) -> RuntimeResult<StartReport> {
        let out = self.0.run("runtime.start", &[])?;
        Ok(StartReport {
            started: parse_bool(kv(&out, "started"), "start")?,
            notes: out
                .lines()
                .filter(|l| !l.is_empty() && !l.starts_with("started="))
                .map(str::to_owned)
                .take(20)
                .collect(),
        })
    }
}

/// The upgrade engine is not part of this task: planning, approval and apply refuse as
/// unavailable.
pub struct UpgradeUnavailable;

impl ReleasePlanner for UpgradeUnavailable {
    fn plan(&self, _target_version: &str, _now: i64) -> RuntimeResult<ReleasePlan> {
        Err(unavailable("the upgrade planner"))
    }
}

impl ApprovalPort for UpgradeUnavailable {
    fn submit(&self, _plan: &ReleasePlan, _now: i64) -> RuntimeResult<ApprovalDecision> {
        Err(unavailable("the upgrade approval flow"))
    }
}

impl ApplyPort for UpgradeUnavailable {
    fn apply(
        &self,
        _plan: &ReleasePlan,
        _approval: &Approval,
        _now: i64,
    ) -> RuntimeResult<ApplyOutcome> {
        Err(unavailable("the upgrade apply engine"))
    }
    fn recover(&self, _plan: &ReleasePlan, _now: i64) -> RuntimeResult<RecoveryOutcome> {
        Err(unavailable("the upgrade apply engine"))
    }
}

/// The ports over one executor.
#[must_use]
pub fn ports(exec: Arc<dyn RuntimeExec>) -> RuntimePorts {
    RuntimePorts {
        pg: Arc::new(PgExec(exec.clone())),
        replica: Arc::new(ReplicaExec(exec.clone())),
        backup: Arc::new(BackupExec(exec.clone())),
        runtime: Arc::new(RuntimeServiceExec(exec)),
        planner: Arc::new(UpgradeUnavailable),
        approval: Arc::new(UpgradeUnavailable),
        apply: Arc::new(UpgradeUnavailable),
    }
}

/// Builds the module for `node_id` over `exec`.
///
/// # Errors
/// [`RuntimeError::Settings`] when the compiled-in settings schema is invalid.
pub fn module(exec: Arc<dyn RuntimeExec>, node_id: &str) -> RuntimeResult<RuntimeModule> {
    Ok(RuntimeModule::new(
        ports(exec),
        SettingsPorts::runtime()?,
        node_id,
    ))
}

/// The operations of the runtime module.
pub fn register_catalogue(builder: &mut spot_mgmt_ops::CatalogueBuilder) {
    RuntimeCatalogue.register(builder);
}

/// Registers the runtime handlers (Linux nodes only).
pub fn register(reg: &mut HandlerRegistry, _ctx: &HandlerCtx) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let node = std::env::var("SPOT_NODE_ID").unwrap_or_else(|_| "node".to_owned());
    if let Ok(m) = module(Arc::new(HelperExec::new()), &node) {
        RuntimeHandler::register(&Arc::new(RuntimeHandler::new(Arc::new(m))), reg);
    }
}
