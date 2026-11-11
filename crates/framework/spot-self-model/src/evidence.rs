//! Evidence references, history entries, time ranges and provenance.
//!
//! An explanation is only allowed to name a cause that has an **evidence record**: a metadata
//! version, a customisation revision, a migration run id, a permission snapshot entry, a
//! script id or a telemetry record. This module is that vocabulary.

use serde::{Deserialize, Serialize};
use spot_graph_model::NodeTypeId;

use crate::model::NodeRef;

/// The authoritative record a piece of the graph comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    /// The effective metadata version.
    Metadata,
    /// A site customisation revision.
    Customisation,
    /// The step and seam registry.
    SeamRegistry,
    /// The installer's installed-app state.
    Installer,
    /// An app manifest.
    Manifest,
    /// A migration audit run.
    MigrationAudit,
    /// A release metadata patch declaration.
    ReleaseMetadata,
    /// The permission snapshot.
    PermissionSnapshot,
    /// The TypeScript script index.
    ScriptIndex,
    /// The generated `.d.ts` check.
    DtsCheck,
    /// A telemetry record.
    Telemetry,
    /// The fleet health engine's finding.
    FleetHealth,
    /// A settings change record.
    Settings,
    /// The audit chain.
    Audit,
    /// An incremental system event.
    Event,
}

impl EvidenceSource {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Customisation => "customisation",
            Self::SeamRegistry => "seam_registry",
            Self::Installer => "installer",
            Self::Manifest => "manifest",
            Self::MigrationAudit => "migration_audit",
            Self::ReleaseMetadata => "release_metadata",
            Self::PermissionSnapshot => "permission_snapshot",
            Self::ScriptIndex => "script_index",
            Self::DtsCheck => "dts_check",
            Self::Telemetry => "telemetry",
            Self::FleetHealth => "fleet_health",
            Self::Settings => "settings",
            Self::Audit => "audit",
            Self::Event => "event",
        }
    }
}

/// One pointer into an authoritative record.
///
/// `record` is an id, a version or a file/key pair, never a document value and never a
/// secret. `at` is unix microseconds supplied by the caller; the crate reads no clock.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceRef {
    /// Which source keeps the record.
    pub source: EvidenceSource,
    /// The record's id inside that source.
    pub record: String,
    /// When the record was written, in unix microseconds, when the source says.
    pub at: Option<i64>,
}

impl EvidenceRef {
    /// An evidence pointer with a time.
    #[must_use]
    pub fn at(source: EvidenceSource, record: impl Into<String>, at: i64) -> Self {
        Self {
            source,
            record: record.into(),
            at: Some(at),
        }
    }

    /// An evidence pointer without a time.
    #[must_use]
    pub fn untimed(source: EvidenceSource, record: impl Into<String>) -> Self {
        Self {
            source,
            record: record.into(),
            at: None,
        }
    }

    /// Whether the pointer names a record.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.record.trim().is_empty()
    }
}

/// What kind of change a history entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// The node appeared.
    Created,
    /// An attribute changed.
    Updated,
    /// The node disappeared.
    Removed,
    /// A bundle or app version was promoted.
    Promoted,
    /// An assignment or grant expired.
    Expired,
    /// A job or health signal failed.
    Failed,
    /// A job or health signal recovered.
    Recovered,
    /// A grant was added.
    Granted,
    /// A grant was removed.
    Revoked,
    /// A health signal was observed.
    Observed,
}

impl ChangeKind {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Removed => "removed",
            Self::Promoted => "promoted",
            Self::Expired => "expired",
            Self::Failed => "failed",
            Self::Recovered => "recovered",
            Self::Granted => "granted",
            Self::Revoked => "revoked",
            Self::Observed => "observed",
        }
    }

    /// The kind a text form names.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::EventInvalid`](crate::SelfModelError::EventInvalid) when the text is
    /// not one of [`ChangeKind::as_str`]'s forms.
    pub fn parse(text: &str) -> Result<Self, crate::SelfModelError> {
        match text {
            "created" => Ok(Self::Created),
            "updated" => Ok(Self::Updated),
            "removed" => Ok(Self::Removed),
            "promoted" => Ok(Self::Promoted),
            "expired" => Ok(Self::Expired),
            "failed" => Ok(Self::Failed),
            "recovered" => Ok(Self::Recovered),
            "granted" => Ok(Self::Granted),
            "revoked" => Ok(Self::Revoked),
            "observed" => Ok(Self::Observed),
            other => Err(crate::SelfModelError::EventInvalid {
                reason: format!("`{other}` is not a change kind"),
            }),
        }
    }
}

/// One entry in a node's history.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// The node the change is about.
    pub node: NodeTypeId,
    /// When it happened, in unix microseconds.
    pub at: i64,
    /// What kind of change it was.
    pub change: ChangeKind,
    /// A short, non-sensitive description of the change.
    pub detail: String,
    /// The record that proves it.
    pub evidence: EvidenceRef,
}

impl HistoryEntry {
    /// One history entry.
    #[must_use]
    pub fn new(
        node: NodeTypeId,
        at: i64,
        change: ChangeKind,
        detail: impl Into<String>,
        evidence: EvidenceRef,
    ) -> Self {
        Self {
            node,
            at,
            change,
            detail: detail.into(),
            evidence,
        }
    }
}

/// An inclusive time window; either end may be open.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct TimeRange {
    /// The earliest unix microsecond included, or `None` for no lower bound.
    pub from: Option<i64>,
    /// The latest unix microsecond included, or `None` for no upper bound.
    pub to: Option<i64>,
}

impl TimeRange {
    /// Every time.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// A closed window.
    #[must_use]
    pub fn new(from: i64, to: i64) -> Self {
        Self {
            from: Some(from),
            to: Some(to),
        }
    }

    /// Whether the window is well formed (`from <= to` when both are present).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match (self.from, self.to) {
            (Some(from), Some(to)) => from <= to,
            _ => true,
        }
    }

    /// Whether `at` falls inside the window.
    #[must_use]
    pub fn contains(&self, at: i64) -> bool {
        self.from.is_none_or(|from| at >= from) && self.to.is_none_or(|to| at <= to)
    }
}

/// Who made a source change, when, under which record, and what caused it.
///
/// `cause` is the node a source names as the reason for the change: a migration run that
/// promoted the bundle, an app version that shipped the script, or another customisation.
/// Supplying it lets `why` Continue past the customisation node; leaving it out never invents
/// a cause.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Provenance {
    /// The identity in the audit chain's form (`system`, `user:<id>` or `agent:<id>`).
    pub actor: String,
    /// Unix microseconds.
    pub at: i64,
    /// The record id that proves the change.
    pub record: String,
    /// The node that caused the change, when the source names one.
    pub cause: Option<NodeRef>,
}

impl Provenance {
    /// A provenance record with no named cause.
    #[must_use]
    pub fn new(actor: impl Into<String>, at: i64, record: impl Into<String>) -> Self {
        Self {
            actor: actor.into(),
            at,
            record: record.into(),
            cause: None,
        }
    }

    /// Names the node that caused the change.
    #[must_use]
    pub fn caused_by(mut self, cause: NodeRef) -> Self {
        self.cause = Some(cause);
        self
    }

    /// The evidence pointer for a source.
    #[must_use]
    pub fn evidence(&self, source: EvidenceSource) -> EvidenceRef {
        EvidenceRef::at(source, self.record.clone(), self.at)
    }
}
