//! The hardening baseline applied by the agent (WP-ADM-10.T2): `hardening.check` (read: the
//! findings), `hardening.plan` (read: the exact changes) and `hardening.apply` (change;
//! each rule individually selectable; dry-run by default).
//!
//! `spot-hardening` defines the baseline and the pure evaluation; this file supplies the
//! host facts ([`HostFacts`], read through the `spot-agent-linux` filesystem seam, so a
//! fixture host is a mounted directory) and applies remedies through the Linux operation
//! modules (`spot_agent_linux::modules`), never through a free-form command.
//!
//! # Lock-out guard
//!
//! A rule whose control has `lockout_risk` (SSH password login off, firewall default deny)
//! is applied only when the command states a key-based administrator session
//! (`admin_key_session=true`) **and** a network revert ticket (WP-ADM-04.T5) is pending and
//! named by `revert_ticket`; otherwise the apply is refused with no change made.
//!
//! # Output
//!
//! Findings reference the rule id, severity and a digest of the evidence (never file
//! contents). Because a handler's success carries no payload, the report is written to
//! `<state>/hardening-report.json` (read by the console through the file-collection
//! operation) and the drift count by severity to `<state>/hardening-drift.json`, which
//! [`drift_series`] turns into heartbeat series.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use spot_agent_core::{ExecutionPlan, HandlerOutcome, HandlerRegistry, OperationHandler};
use spot_agent_linux::{FileSystem, LinuxContext, LinuxError, LinuxOperation};
use spot_agent_linux_sys::network::NetworkModule;
use spot_hardening::{
    Baseline, ControlId, FactKey, FactSource, FactValue, HardeningError, Outcome, Report, Severity,
    evaluate,
};
use spot_mgmt_ops::{
    CatalogueBuilder, Invocation, OpId, OperationSpec, ParamSpec, ParamType, ParamValue, Risk,
    TargetKind,
};

use super::HandlerCtx;

/// The operation namespace.
pub const NAMESPACE: &str = "hardening";
/// `hardening.check`.
pub const CHECK: &str = "hardening.check";
/// `hardening.plan`.
pub const PLAN: &str = "hardening.plan";
/// `hardening.apply`.
pub const APPLY: &str = "hardening.apply";

const RULES_PATTERN: &str = "[a-z][a-z0-9_.]{0,62}(,[a-z][a-z0-9_.]{0,62}){0,31}";

fn spec(
    id: &str,
    title: &str,
    summary: &str,
    risk: Risk,
    params: Vec<ParamSpec>,
) -> Option<OperationSpec> {
    Some(OperationSpec {
        id: OpId::new(id)?,
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Server,
    })
}

fn rules_param(required: bool) -> ParamSpec {
    ParamSpec {
        name: "rules".to_owned(),
        ty: ParamType::Text { max_len: 2048, pattern: Some(RULES_PATTERN.to_owned()) },
        required,
        default: None,
        doc: "Comma-separated control ids (for example ssh.root_login,mac.enforcing); every failing control when omitted (plan only).".to_owned(),
        secret: false,
    }
}

/// The operations this handler set declares.
pub fn register_catalogue(builder: &mut CatalogueBuilder) {
    for spec in catalogue_specs() {
        builder.add("agent-hardening", spec);
    }
}

/// The three operation specs.
#[must_use]
pub fn catalogue_specs() -> Vec<OperationSpec> {
    let apply_params = vec![
        rules_param(true),
        ParamSpec {
            name: "dry_run".to_owned(),
            ty: ParamType::Bool,
            required: false,
            default: Some(ParamValue::Bool(true)),
            doc: "Only report what would change (the default).".to_owned(),
            secret: false,
        },
        ParamSpec {
            name: "admin_key_session".to_owned(),
            ty: ParamType::Bool,
            required: false,
            default: Some(ParamValue::Bool(false)),
            doc: "The operator confirms a key-based administrator session exists.".to_owned(),
            secret: false,
        },
        ParamSpec {
            name: "revert_ticket".to_owned(),
            ty: ParamType::Int {
                min: 0,
                max: i64::MAX,
            },
            required: false,
            default: None,
            doc: "The id of the pending network revert ticket (WP-ADM-04.T5).".to_owned(),
            secret: false,
        },
    ];
    [
        spec(CHECK, "Check the hardening baseline", "Evaluates the baseline and reports findings by rule, severity and evidence digest.", Risk::Read, vec![]),
        spec(PLAN, "Plan hardening changes", "Shows the exact operations that would fix the selected failing rules.", Risk::Read, vec![rules_param(false)]),
        spec(
            APPLY,
            "Apply hardening rules",
            "Applies the selected rules; a dry run by default; access-affecting rules need a key-based admin session and a pending network revert ticket.",
            Risk::High,
            apply_params,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Registers the handlers.
pub fn register(reg: &mut HandlerRegistry, ctx: &HandlerCtx) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let context = Arc::new(LinuxContext::new(
        ctx.linux_runner.clone(),
        ctx.linux_fs.clone(),
    ));
    let handler = Arc::new(HardeningHandler::new(
        ctx,
        context.clone(),
        ctx.linux_fs.clone(),
    ));
    for id in [CHECK, PLAN, APPLY] {
        if let Some(op) = OpId::new(id) {
            reg.register(op, handler.clone());
        }
    }
}

/// Host facts read through the filesystem seam. A fact the host does not report is `None`
/// (the control is then not applicable): this set covers the SSH, MAC and kernel facts; the
/// firewall, service, update and file-permission facts need command probes that are added
/// with their templates.
pub struct HostFacts {
    fs: Arc<dyn FileSystem>,
}

impl HostFacts {
    /// Facts over `fs` (the real root, or a mounted fixture directory).
    #[must_use]
    pub fn new(fs: Arc<dyn FileSystem>) -> Self {
        Self { fs }
    }

    fn read(&self, path: &str) -> Option<String> {
        self.fs.read_to_string(path).ok()
    }

    /// The first effective value of an sshd directive (sshd uses the first one it sees;
    /// drop-ins are read first, like the `Include` at the top of the stock config).
    fn sshd(&self, directive: &str) -> Option<String> {
        let mut files: Vec<String> = Vec::new();
        for name in ["00-spot.conf", "10-hardening.conf", "50-local.conf"] {
            files.push(format!("/etc/ssh/sshd_config.d/{name}"));
        }
        files.push("/etc/ssh/sshd_config".to_owned());
        for path in files {
            let Some(text) = self.read(&path) else {
                continue;
            };
            for line in text.lines() {
                let mut parts = line.split_whitespace();
                if parts
                    .next()
                    .is_some_and(|k| k.eq_ignore_ascii_case(directive))
                    && let Some(v) = parts.next() {
                        return Some(v.to_ascii_lowercase());
                    }
            }
        }
        None
    }

    fn ssh_enabled(&self) -> Result<bool, LinuxError> {
        for unit in ["sshd.service", "ssh.service"] {
            if self.fs.exists(&format!(
                "/etc/systemd/system/multi-user.target.wants/{unit}"
            ))? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl FactSource for HostFacts {
    fn fact(&self, key: &FactKey) -> Result<Option<FactValue>, HardeningError> {
        let unavailable = |_: LinuxError| HardeningError::FactUnavailable {
            fact: key.as_str().to_owned(),
            detail: "the host could not be read".to_owned(),
        };
        Ok(match key.as_str() {
            "ssh.permit_root_login" => Some(FactValue::Text(
                self.sshd("PermitRootLogin")
                    .unwrap_or_else(|| "prohibit-password".to_owned()),
            )),
            "ssh.password_authentication" => Some(FactValue::Text(
                self.sshd("PasswordAuthentication")
                    .unwrap_or_else(|| "yes".to_owned()),
            )),
            "ssh.enabled" => Some(FactValue::Bool(self.ssh_enabled().map_err(unavailable)?)),
            "mac.mode" => {
                if let Some(v) = self.read("/sys/fs/selinux/enforce") {
                    Some(FactValue::Text(
                        if v.trim() == "1" {
                            "enforcing"
                        } else {
                            "permissive"
                        }
                        .to_owned(),
                    ))
                } else if self
                    .read("/sys/module/apparmor/parameters/enabled")
                    .is_some_and(|v| v.trim() == "Y")
                {
                    Some(FactValue::Text("enforcing".to_owned()))
                } else {
                    Some(FactValue::Text("disabled".to_owned()))
                }
            }
            "kernel.hardening_sysctls" => {
                let wants = [
                    ("/proc/sys/net/ipv4/conf/all/rp_filter", "1"),
                    ("/proc/sys/kernel/kptr_restrict", "1"),
                    ("/proc/sys/net/ipv4/tcp_syncookies", "1"),
                ];
                let mut ok = true;
                for (path, want) in wants {
                    ok &= self.read(path).is_some_and(|v| v.trim() == want);
                }
                Some(FactValue::Bool(ok))
            }
            _ => None,
        })
    }
}

/// One finding: the rule, its severity and a digest of the evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The control id.
    pub rule: String,
    /// `low`..`critical`.
    pub severity: String,
    /// `fail` or `error`.
    pub status: String,
    /// First 16 hex digits of SHA-256 over the evidence text (what was found), never the
    /// text itself.
    pub evidence_sha256: String,
}

/// Counts of failing controls by severity: the drift summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drift {
    /// Critical findings.
    pub critical: usize,
    /// High findings.
    pub high: usize,
    /// Medium findings.
    pub medium: usize,
    /// Low findings.
    pub low: usize,
}

fn digest16(text: &str) -> String {
    let mut out = String::new();
    for b in Sha256::digest(text.as_bytes()).iter().take(8) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The findings of a report, in baseline order.
#[must_use]
pub fn findings(report: &Report) -> Vec<Finding> {
    report
        .results
        .iter()
        .filter_map(|r| {
            let (status, evidence) = match &r.outcome {
                Outcome::Fail { found, .. } => ("fail", found.as_str()),
                Outcome::Error { detail } => ("error", detail.as_str()),
                _ => return None,
            };
            Some(Finding {
                rule: r.id.as_str().to_owned(),
                severity: format!("{:?}", r.severity).to_ascii_lowercase(),
                status: status.to_owned(),
                evidence_sha256: digest16(evidence),
            })
        })
        .collect()
}

/// The drift summary of a report.
#[must_use]
pub fn drift(report: &Report) -> Drift {
    let mut d = Drift::default();
    for r in report.results.iter().filter(|r| r.outcome.is_fail()) {
        match r.severity {
            Severity::Critical => d.critical += 1,
            Severity::High => d.high += 1,
            Severity::Medium => d.medium += 1,
            Severity::Low => d.low += 1,
        }
    }
    d
}

/// The heartbeat series for the last check: `hardening_findings{severity=...}` counts.
#[must_use]
pub fn drift_series(state_dir: &Path) -> Vec<(String, String)> {
    let Some(d) = std::fs::read_to_string(state_dir.join("hardening-drift.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Drift>(&t).ok())
    else {
        return Vec::new();
    };
    vec![
        (
            "hardening_findings_critical".to_owned(),
            d.critical.to_string(),
        ),
        ("hardening_findings_high".to_owned(), d.high.to_string()),
        ("hardening_findings_medium".to_owned(), d.medium.to_string()),
        ("hardening_findings_low".to_owned(), d.low.to_string()),
    ]
}

/// Why an operation did not complete (the text is the handler failure detail).
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// A selected rule is unknown.
    UnknownRule,
    /// A selected rule is not failing or has no remedy.
    NotRemediable,
    /// An access-affecting rule without the key session and the pending revert ticket.
    LockoutGuard,
    /// The check could not run.
    Failed(String),
}

impl Refused {
    fn detail(&self) -> String {
        match self {
            Self::UnknownRule => "hardening.unknown_rule: a selected rule is not in the baseline".to_owned(),
            Self::NotRemediable => "hardening.not_remediable: a selected rule is passing or has no remedy".to_owned(),
            Self::LockoutGuard => "hardening.lockout_guard: an access-affecting rule needs admin_key_session=true and a pending network revert ticket named by revert_ticket; nothing was changed".to_owned(),
            Self::Failed(d) => format!("hardening.failed: {d}"),
        }
    }
}

/// The handler for the three operations.
pub struct HardeningHandler {
    facts: HostFacts,
    modules: Vec<Arc<dyn LinuxOperation>>,
    catalogue: spot_mgmt_ops::Catalogue,
    sys: HandlerCtx,
    state_dir: PathBuf,
}

impl HardeningHandler {
    /// Builds the handler over the context.
    #[must_use]
    pub fn new(ctx: &HandlerCtx, context: Arc<LinuxContext>, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            facts: HostFacts::new(fs),
            modules: spot_agent_linux::modules(&context),
            catalogue: spot_agent_linux::build_catalogue().unwrap_or_else(|_| {
                spot_mgmt_ops::CatalogueBuilder::new()
                    .build()
                    .unwrap_or_else(|_| std::process::exit(70))
            }),
            sys: ctx.clone(),
            state_dir: ctx.state_dir.clone(),
        }
    }

    fn baseline() -> Result<Baseline, Refused> {
        Baseline::linux().map_err(|_| Refused::Failed("the baseline is invalid".to_owned()))
    }

    fn report(&self) -> Result<(Baseline, Report), Refused> {
        let baseline = Self::baseline()?;
        let report = evaluate(&baseline, &self.facts);
        Ok((baseline, report))
    }

    fn save(&self, name: &str, text: &str) {
        let _ = std::fs::create_dir_all(&self.state_dir);
        let _ = crate::state::atomic_write(&self.state_dir.join(name), text.as_bytes());
    }

    /// `hardening.check`: evaluate, write the report and the drift summary.
    ///
    /// # Errors
    /// [`Refused::Failed`].
    pub fn check(&self) -> Result<Vec<Finding>, Refused> {
        let (_, report) = self.report()?;
        let found = findings(&report);
        if let Ok(text) = serde_json::to_string(&found) {
            self.save("hardening-report.json", &text);
        }
        if let Ok(text) = serde_json::to_string(&drift(&report)) {
            self.save("hardening-drift.json", &text);
        }
        Ok(found)
    }

    fn select(
        &self,
        baseline: &Baseline,
        report: &Report,
        rules: Option<&str>,
    ) -> Result<Vec<ControlId>, Refused> {
        let ids: Vec<ControlId> = match rules {
            Some(list) => {
                let mut out = Vec::new();
                for part in list.split(',') {
                    let id = ControlId::try_new(part).map_err(|_| Refused::UnknownRule)?;
                    baseline.control(&id).ok_or(Refused::UnknownRule)?;
                    out.push(id);
                }
                out
            }
            None => report
                .results
                .iter()
                .filter(|r| r.outcome.is_fail() && r.remedy.is_some())
                .map(|r| r.id.clone())
                .collect(),
        };
        for id in &ids {
            let result = report.result(id).ok_or(Refused::UnknownRule)?;
            if !(result.outcome.is_fail() && result.remedy.is_some()) {
                return Err(Refused::NotRemediable);
            }
        }
        Ok(ids)
    }

    /// The exact operations a selection would run, in order, as `op param=value` lines.
    ///
    /// # Errors
    /// [`Refused`].
    pub fn plan(&self, rules: Option<&str>) -> Result<Vec<String>, Refused> {
        let (baseline, report) = self.report()?;
        let ids = self.select(&baseline, &report, rules)?;
        let mut lines = Vec::new();
        for id in ids {
            if let Some(remedy) = report.result(&id).and_then(|r| r.remedy.clone()) {
                let params: Vec<String> = remedy
                    .params
                    .iter()
                    .map(|(k, v)| format!("{k}={}", describe(v)))
                    .collect();
                lines.push(
                    format!("{id}: {} {}", remedy.op.as_str(), params.join(" "))
                        .trim_end()
                        .to_owned(),
                );
            }
        }
        self.save(
            "hardening-plan.json",
            &serde_json::to_string(&lines).unwrap_or_default(),
        );
        Ok(lines)
    }

    fn guard_satisfied(&self, admin_key_session: bool, revert_ticket: Option<i64>) -> bool {
        let (Some(ticket), true) = (revert_ticket, admin_key_session) else {
            return false;
        };
        let network = NetworkModule::new(
            self.sys.sys_runner.as_ref(),
            self.sys.sys_fs.as_ref(),
            self.sys.clock.clone(),
        )
        .with_state_dir(self.state_dir.join("network"));
        matches!(network.pending(), Ok(Some(p)) if i64::try_from(p.id).is_ok_and(|id| id == ticket))
    }

    /// `hardening.apply`: refuses a lock-out rule without the guard (before changing
    /// anything), then applies each selected remedy through its operation module and
    /// verifies it.
    ///
    /// # Errors
    /// [`Refused`].
    pub fn apply(
        &self,
        rules: &str,
        dry_run: bool,
        admin_key_session: bool,
        revert_ticket: Option<i64>,
    ) -> Result<Vec<String>, Refused> {
        let (baseline, report) = self.report()?;
        let ids = self.select(&baseline, &report, Some(rules))?;
        let needs_guard = ids
            .iter()
            .any(|id| baseline.control(id).is_some_and(|c| c.lockout_risk));
        if needs_guard && !dry_run && !self.guard_satisfied(admin_key_session, revert_ticket) {
            return Err(Refused::LockoutGuard);
        }
        let mut done = Vec::new();
        for id in ids {
            let Some(remedy) = report.result(&id).and_then(|r| r.remedy.clone()) else {
                continue;
            };
            let args = self
                .catalogue
                .validate(&Invocation {
                    op: remedy.op.clone(),
                    params: remedy.params.clone(),
                })
                .map_err(|_| Refused::Failed(format!("the remedy of {id} is not valid")))?;
            let module = self
                .modules
                .iter()
                .find(|m| m.op() == &remedy.op)
                .ok_or_else(|| {
                    Refused::Failed(format!("no module implements the remedy of {id}"))
                })?;
            if dry_run {
                let preview = module
                    .plan(&args)
                    .map_err(|e| Refused::Failed(e.code().to_owned()))?;
                done.push(format!("{id}: would {}", preview.summary));
                continue;
            }
            module
                .apply(&args)
                .map_err(|e| Refused::Failed(e.code().to_owned()))?;
            let verified = module
                .verify(&args)
                .map_err(|e| Refused::Failed(e.code().to_owned()))?;
            if !verified.ok {
                return Err(Refused::Failed(format!(
                    "{id} was applied but verification failed"
                )));
            }
            done.push(format!("{id}: applied and verified"));
        }
        Ok(done)
    }
}

fn describe(value: &ParamValue) -> String {
    match value {
        ParamValue::Text(t) => t.clone(),
        ParamValue::Bool(b) => b.to_string(),
        ParamValue::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

fn param<'a>(plan: &'a ExecutionPlan, name: &str) -> Option<&'a ParamValue> {
    plan.args()
        .params
        .iter()
        .find(|p| p.name == name)
        .map(|p| &p.value)
}

impl OperationHandler for HardeningHandler {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        let rules = match param(plan, "rules") {
            Some(ParamValue::Text(t)) => Some(t.as_str()),
            _ => None,
        };
        let result: Result<(), Refused> = match plan.op().as_str() {
            CHECK => self.check().map(|_| ()),
            PLAN => self.plan(rules).map(|_| ()),
            APPLY => {
                let flag = |name: &str, default: bool| match param(plan, name) {
                    Some(ParamValue::Bool(b)) => *b,
                    _ => default,
                };
                let ticket = match param(plan, "revert_ticket") {
                    Some(ParamValue::Int(i)) => Some(*i),
                    _ => None,
                };
                match rules {
                    Some(r) => self
                        .apply(
                            r,
                            flag("dry_run", true),
                            flag("admin_key_session", false),
                            ticket,
                        )
                        .map(|_| ()),
                    None => Err(Refused::UnknownRule),
                }
            }
            _ => Err(Refused::Failed("unknown hardening operation".to_owned())),
        };
        match result {
            Ok(()) => HandlerOutcome::Succeeded,
            Err(refused) => HandlerOutcome::Failed {
                detail: refused.detail(),
            },
        }
    }
}

/// A map of rule id to remedy text, for tests and the console.
#[must_use]
pub fn remedies(report: &Report) -> BTreeMap<String, String> {
    report
        .results
        .iter()
        .filter_map(|r| {
            r.remedy
                .as_ref()
                .map(|m| (r.id.as_str().to_owned(), m.op.as_str().to_owned()))
        })
        .collect()
}
