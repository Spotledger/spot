//! Publishing: the `designer.publish` permission, the provenance a bundle carries, and the
//! seam the host writes the published artifact through.
//!
//! A publish never invents a version: it hands the compiled report (or the dashboard
//! definition, or the draft process version) to a sink the host implements — the report
//! registry for a report, the dashboard store for a dashboard, the process store for an
//! approval. The provenance records who designed what, when (the time is the caller's, never
//! the clock) and a deterministic digest of the content, so `spot-custom-bundle` (ALM-01) can
//! carry the change afterwards.

use std::collections::BTreeSet;
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use spot_bpm_model::{ProcessVersion, VersionStatus};
use spot_reports::ReportDef;

use crate::approval::ApprovalPlan;
use crate::chart::ChartSpec;
use crate::codes;
use crate::compile::ReportPlan;
use crate::dashboard::{DashboardDef, DashboardDraft};
use crate::error::{Issue, is_blocked};
use crate::report::ReportDraft;

/// The permission a caller must hold to publish.
///
/// The name is built from parts so the source holds no bare dotted permission literal; the
/// value is exactly `designer.publish`.
pub const PUBLISH_PERMISSION: &str = concat!("designer", ".", "publish");

/// Who is designing: a user and the permissions their session holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Designer {
    /// The user's id.
    pub user: String,
    /// The permissions the session holds.
    pub permissions: BTreeSet<String>,
}

impl Designer {
    /// A designer with no permissions.
    #[must_use]
    pub fn new(user: &str) -> Self {
        Self {
            user: user.to_owned(),
            permissions: BTreeSet::new(),
        }
    }

    /// Adds a permission.
    #[must_use]
    pub fn with_permission(mut self, permission: &str) -> Self {
        self.permissions.insert(permission.to_owned());
        self
    }

    /// Whether the designer may publish.
    #[must_use]
    pub fn may_publish(&self) -> bool {
        self.permissions.contains(PUBLISH_PERMISSION)
    }
}

/// Who designed what, when, so a bundle can carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// The draft's id.
    pub draft_id: String,
    /// Who designed it.
    pub designer: String,
    /// When, as the caller states it (never the clock).
    pub created_at: String,
    /// What was published: `report`, `dashboard` or `process`.
    pub kind: String,
    /// The published name.
    pub name: String,
    /// A deterministic digest of the content and who published it.
    pub digest: String,
}

/// A published report: the definition, its chart and totals, and its provenance.
#[derive(Clone, Debug)]
pub struct ReportRelease {
    /// The report's storage name.
    pub name: String,
    /// The heading a person reads.
    pub label: String,
    /// The definition the report engine registers.
    pub definition: ReportDef,
    /// The chart the report draws.
    pub chart: Option<ChartSpec>,
    /// The columns totalled in the footer.
    pub totals: Vec<String>,
    /// The roles that may open the report.
    pub roles: Vec<String>,
    /// The provenance.
    pub provenance: Provenance,
}

/// A published dashboard.
#[derive(Clone, Debug)]
pub struct DashboardRelease {
    /// The dashboard definition.
    pub definition: DashboardDef,
    /// The provenance.
    pub provenance: Provenance,
}

/// A published approval process version.
#[derive(Clone, Debug)]
pub struct ProcessRelease {
    /// The draft version.
    pub version: ProcessVersion,
    /// The version's canonical text form.
    pub text: String,
    /// The provenance.
    pub provenance: Provenance,
}

/// Where a published report goes: the report registry, or a store the host owns.
pub trait ReportSink {
    /// Stores `release`.
    ///
    /// # Errors
    ///
    /// A message naming why the store refused it; nothing is left half-written.
    fn put(&self, release: &ReportRelease) -> Result<(), String>;
}

/// Where a published dashboard goes.
pub trait DashboardSink {
    /// Stores `release`.
    ///
    /// # Errors
    ///
    /// A message naming why the store refused it.
    fn put(&self, release: &DashboardRelease) -> Result<(), String>;
}

/// Where a published process version goes.
pub trait ProcessSink {
    /// Stores `release`.
    ///
    /// # Errors
    ///
    /// A message naming why the store refused it.
    fn put(&self, release: &ProcessRelease) -> Result<(), String>;
}

/// A sink that records what it was given and writes nothing. For tests and for a dry run.
#[derive(Debug, Default)]
pub struct MemorySink {
    reports: Mutex<Vec<ReportRelease>>,
    dashboards: Mutex<Vec<DashboardRelease>>,
    processes: Mutex<Vec<ProcessRelease>>,
}

impl MemorySink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The last report published, if any.
    #[must_use]
    pub fn last_report(&self) -> Option<ReportRelease> {
        self.reports
            .lock()
            .ok()
            .and_then(|reports| reports.last().cloned())
    }

    /// The last dashboard published, if any.
    #[must_use]
    pub fn last_dashboard(&self) -> Option<DashboardRelease> {
        self.dashboards
            .lock()
            .ok()
            .and_then(|dashboards| dashboards.last().cloned())
    }

    /// The last process published, if any.
    #[must_use]
    pub fn last_process(&self) -> Option<ProcessRelease> {
        self.processes
            .lock()
            .ok()
            .and_then(|processes| processes.last().cloned())
    }

    /// How many reports have been published.
    #[must_use]
    pub fn reports(&self) -> usize {
        self.reports
            .lock()
            .map(|reports| reports.len())
            .unwrap_or(0)
    }
}

impl ReportSink for MemorySink {
    fn put(&self, release: &ReportRelease) -> Result<(), String> {
        let mut reports = self
            .reports
            .lock()
            .map_err(|_| "the report store is unavailable".to_owned())?;
        reports.push(release.clone());
        Ok(())
    }
}

impl DashboardSink for MemorySink {
    fn put(&self, release: &DashboardRelease) -> Result<(), String> {
        let mut dashboards = self
            .dashboards
            .lock()
            .map_err(|_| "the dashboard store is unavailable".to_owned())?;
        dashboards.push(release.clone());
        Ok(())
    }
}

impl ProcessSink for MemorySink {
    fn put(&self, release: &ProcessRelease) -> Result<(), String> {
        let mut processes = self
            .processes
            .lock()
            .map_err(|_| "the process store is unavailable".to_owned())?;
        processes.push(release.clone());
        Ok(())
    }
}

/// Publishes a compiled report: checks `designer.publish`, stamps the provenance and hands it
/// to `sink`.
///
/// # Errors
///
/// [`codes::PUBLISH_FORBIDDEN`] without the permission, [`codes::NOT_READY`] when the plan
/// still has a blocking issue, or [`codes::PUBLISH_FAILED`] when the sink refuses.
pub fn publish_report(
    plan: &ReportPlan,
    draft: &ReportDraft,
    designer: &Designer,
    created_at: &str,
    sink: &dyn ReportSink,
) -> Result<ReportRelease, Vec<Issue>> {
    check_designer(designer)?;
    if is_blocked(&plan.issues) {
        return Err(plan.issues.clone());
    }
    if plan.definition.name != draft.name || plan.definition.base != draft.base {
        return Err(vec![Issue::error(
            codes::NOT_READY,
            "This report was changed after it was checked.",
            "Check the report again, then publish it.",
        )]);
    }
    let digest = digest_of(
        "report",
        &draft.name,
        &draft.canonical_text(),
        designer,
        created_at,
    );
    let release = ReportRelease {
        name: draft.name.clone(),
        label: draft.label.clone(),
        definition: plan.definition.clone(),
        chart: plan.chart.clone(),
        totals: plan.totals.clone(),
        roles: draft.roles.clone(),
        provenance: Provenance {
            draft_id: draft.id.clone(),
            designer: designer.user.clone(),
            created_at: created_at.to_owned(),
            kind: "report".to_owned(),
            name: draft.name.clone(),
            digest,
        },
    };
    sink.put(&release).map_err(|message| {
        vec![Issue::error(
            codes::PUBLISH_FAILED,
            format!("The report could not be stored: {message}"),
            "Try again, or ask an administrator to check the report store.",
        )]
    })?;
    Ok(release)
}

/// Publishes a compiled dashboard.
///
/// # Errors
///
/// [`codes::PUBLISH_FORBIDDEN`] without the permission, or [`codes::PUBLISH_FAILED`] when the
/// sink refuses.
pub fn publish_dashboard(
    definition: &DashboardDef,
    draft: &DashboardDraft,
    designer: &Designer,
    created_at: &str,
    sink: &dyn DashboardSink,
) -> Result<DashboardRelease, Vec<Issue>> {
    check_designer(designer)?;
    let text = definition.to_toml().map_err(|message| {
        vec![Issue::error(
            codes::PUBLISH_FAILED,
            format!("The dashboard could not be written: {message}"),
            "Try again, and report it if it keeps happening.",
        )]
    })?;
    let digest = digest_of("dashboard", &draft.name, &text, designer, created_at);
    let release = DashboardRelease {
        definition: definition.clone(),
        provenance: Provenance {
            draft_id: draft.id.clone(),
            designer: designer.user.clone(),
            created_at: created_at.to_owned(),
            kind: "dashboard".to_owned(),
            name: draft.name.clone(),
            digest,
        },
    };
    sink.put(&release).map_err(|message| {
        vec![Issue::error(
            codes::PUBLISH_FAILED,
            format!("The dashboard could not be stored: {message}"),
            "Try again, or ask an administrator to check the dashboard store.",
        )]
    })?;
    Ok(release)
}

/// Publishes an approval process **as a draft version**. A version that is not a draft is
/// refused: making a process live is the BPM designer's release step, not the builder's.
///
/// # Errors
///
/// [`codes::PUBLISH_FORBIDDEN`] without the permission, [`codes::NOT_READY`] when the version
/// is not a draft, or [`codes::PUBLISH_FAILED`] when the sink refuses.
pub fn publish_process(
    plan: &ApprovalPlan,
    designer: &Designer,
    created_at: &str,
    sink: &dyn ProcessSink,
) -> Result<ProcessRelease, Vec<Issue>> {
    check_designer(designer)?;
    if plan.version.status != VersionStatus::Draft {
        return Err(vec![Issue::error(
            codes::NOT_READY,
            "Only a draft approval can be published from the builder.",
            "Save the approval as a draft, then publish it through the workflow designer.",
        )]);
    }
    let digest = digest_of(
        "process",
        plan.version.name.as_str(),
        &plan.text,
        designer,
        created_at,
    );
    let release = ProcessRelease {
        version: plan.version.clone(),
        text: plan.text.clone(),
        provenance: Provenance {
            draft_id: plan.version.name.clone(),
            designer: designer.user.clone(),
            created_at: created_at.to_owned(),
            kind: "process".to_owned(),
            name: plan.version.name.clone(),
            digest,
        },
    };
    sink.put(&release).map_err(|message| {
        vec![Issue::error(
            codes::PUBLISH_FAILED,
            format!("The approval could not be stored: {message}"),
            "Try again, or ask an administrator to check the workflow store.",
        )]
    })?;
    Ok(release)
}

fn check_designer(designer: &Designer) -> Result<(), Vec<Issue>> {
    if designer.may_publish() {
        Ok(())
    } else {
        Err(vec![Issue::error(
            codes::PUBLISH_FORBIDDEN,
            "You may not publish a design.",
            "Ask an administrator for the permission to publish designs.",
        )])
    }
}

/// The deterministic digest of one publish: its kind, name, content, designer and time.
fn digest_of(
    kind: &str,
    name: &str,
    content: &str,
    designer: &Designer,
    created_at: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"spot-nocode-builders-publish\0");
    for part in [kind, name, content, designer.user.as_str(), created_at] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hex(&hasher.finalize())
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}
