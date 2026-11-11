//! The self-model vocabulary: node kinds, edge kinds and source kinds.

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;

/// What a node in the system graph is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum NodeKind {
    /// A DocType.
    DocType,
    /// A field of a DocType.
    Field,
    /// A TypeScript script.
    Script,
    /// A service seam.
    Seam,
    /// One step at one hook point.
    PipelineStep,
    /// An installed app.
    App,
    /// A site customisation: a custom field, a property override or a step override.
    Customisation,
    /// A migration run.
    Migration,
    /// A schema or data patch.
    Patch,
    /// A role.
    Role,
    /// A duty or assignment that grants something.
    Duty,
    /// A permission: a right a role or duty holds.
    Permission,
    /// A job.
    Job,
    /// A schedule.
    Schedule,
    /// A setting.
    Setting,
    /// A runtime health signal.
    HealthSignal,
}

impl NodeKind {
    /// Every node kind, in declaration order.
    pub const ALL: [NodeKind; 16] = [
        Self::DocType,
        Self::Field,
        Self::Script,
        Self::Seam,
        Self::PipelineStep,
        Self::App,
        Self::Customisation,
        Self::Migration,
        Self::Patch,
        Self::Role,
        Self::Duty,
        Self::Permission,
        Self::Job,
        Self::Schedule,
        Self::Setting,
        Self::HealthSignal,
    ];

    /// The stable text form, as the graph keys and scenario files spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DocType => "DocType",
            Self::Field => "Field",
            Self::Script => "Script",
            Self::Seam => "Seam",
            Self::PipelineStep => "PipelineStep",
            Self::App => "App",
            Self::Customisation => "Customisation",
            Self::Migration => "Migration",
            Self::Patch => "Patch",
            Self::Role => "Role",
            Self::Duty => "Duty",
            Self::Permission => "Permission",
            Self::Job => "Job",
            Self::Schedule => "Schedule",
            Self::Setting => "Setting",
            Self::HealthSignal => "HealthSignal",
        }
    }

    /// The kind a text form names.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::UnknownNodeKind`](crate::SelfModelError::UnknownNodeKind) when the
    /// text is not one of [`NodeKind::as_str`]'s forms.
    pub fn parse(text: &str) -> Result<Self, crate::SelfModelError> {
        match text {
            "DocType" => Ok(Self::DocType),
            "Field" => Ok(Self::Field),
            "Script" => Ok(Self::Script),
            "Seam" => Ok(Self::Seam),
            "PipelineStep" => Ok(Self::PipelineStep),
            "App" => Ok(Self::App),
            "Customisation" => Ok(Self::Customisation),
            "Migration" => Ok(Self::Migration),
            "Patch" => Ok(Self::Patch),
            "Role" => Ok(Self::Role),
            "Duty" => Ok(Self::Duty),
            "Permission" => Ok(Self::Permission),
            "Job" => Ok(Self::Job),
            "Schedule" => Ok(Self::Schedule),
            "Setting" => Ok(Self::Setting),
            "HealthSignal" => Ok(Self::HealthSignal),
            other => Err(crate::SelfModelError::UnknownNodeKind {
                kind: other.to_owned(),
            }),
        }
    }

    /// The minimum tier an introspection caller needs to see this kind.
    ///
    /// T0 (Explain) sees metadata, structure and runtime health; roles, duties, permissions
    /// and settings are configuration and need T1 (Configure). No kind carries document data
    /// values, so a T0 caller never sees one.
    #[must_use]
    pub fn min_tier(self) -> Tier {
        match self {
            Self::Role | Self::Duty | Self::Permission | Self::Setting => Tier::T1,
            Self::DocType
            | Self::Field
            | Self::Script
            | Self::Seam
            | Self::PipelineStep
            | Self::App
            | Self::Customisation
            | Self::Migration
            | Self::Patch
            | Self::Job
            | Self::Schedule
            | Self::HealthSignal => Tier::T0,
        }
    }
}

/// What one node is to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A DocType or app defines a field, script, seam, step, schedule or patch.
    Defines,
    /// A field or script references a DocType or field (a link or expression reference).
    References,
    /// A script hooks into a pipeline step (Script -> PipelineStep).
    HooksInto,
    /// A customisation overrides a field, step or seam (Customisation -> target).
    Overrides,
    /// An app was installed by a migration (App -> Migration).
    InstalledBy,
    /// A node changed by a migration, customisation, setting or signal (effect -> cause).
    ChangedBy,
    /// A migration, patch, setting or duty affects a node (cause -> effect).
    Affects,
    /// A permission is granted by a role or duty (Permission -> Role/Duty).
    GrantedBy,
    /// A permission governs a DocType or field (Permission -> target).
    Governs,
    /// A health signal observed a node (HealthSignal -> target).
    ObservedOn,
}

impl EdgeKind {
    /// Every edge kind, in declaration order.
    pub const ALL: [EdgeKind; 10] = [
        Self::Defines,
        Self::References,
        Self::HooksInto,
        Self::Overrides,
        Self::InstalledBy,
        Self::ChangedBy,
        Self::Affects,
        Self::GrantedBy,
        Self::Governs,
        Self::ObservedOn,
    ];

    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Defines => "defines",
            Self::References => "references",
            Self::HooksInto => "hooks_into",
            Self::Overrides => "overrides",
            Self::InstalledBy => "installed_by",
            Self::ChangedBy => "changed_by",
            Self::Affects => "affects",
            Self::GrantedBy => "granted_by",
            Self::Governs => "governs",
            Self::ObservedOn => "observed_on",
        }
    }

    /// The kind a text form names.
    ///
    /// # Errors
    ///
    /// [`SelfModelError::UnknownEdgeKind`](crate::SelfModelError::UnknownEdgeKind) when the
    /// text is not one of [`EdgeKind::as_str`]'s forms.
    pub fn parse(text: &str) -> Result<Self, crate::SelfModelError> {
        match text {
            "defines" => Ok(Self::Defines),
            "references" => Ok(Self::References),
            "hooks_into" => Ok(Self::HooksInto),
            "overrides" => Ok(Self::Overrides),
            "installed_by" => Ok(Self::InstalledBy),
            "changed_by" => Ok(Self::ChangedBy),
            "affects" => Ok(Self::Affects),
            "granted_by" => Ok(Self::GrantedBy),
            "governs" => Ok(Self::Governs),
            "observed_on" => Ok(Self::ObservedOn),
            other => Err(crate::SelfModelError::UnknownEdgeKind {
                kind: other.to_owned(),
            }),
        }
    }

    /// How this edge is read while answering "why".
    ///
    /// The stored direction is the edge's own subject -> object direction. `why` orients an
    /// edge so that it always walks *towards the cause*:
    ///
    /// - [`EdgeKind::ChangedBy`], [`EdgeKind::InstalledBy`] and [`EdgeKind::GrantedBy`] are
    ///   written effect -> cause and are walked forward.
    /// - [`EdgeKind::Overrides`], [`EdgeKind::Affects`], [`EdgeKind::HooksInto`],
    ///   [`EdgeKind::Governs`] and [`EdgeKind::ObservedOn`] are written cause -> effect and
    ///   are walked backward.
    /// - [`EdgeKind::Defines`] and [`EdgeKind::References`] describe structure, not change,
    ///   so they never become a cause step.
    #[must_use]
    pub fn cause_direction(self) -> CauseDirection {
        match self {
            Self::ChangedBy | Self::InstalledBy | Self::GrantedBy => CauseDirection::EffectToCause,
            Self::Overrides
            | Self::Affects
            | Self::HooksInto
            | Self::Governs
            | Self::ObservedOn => CauseDirection::CauseToEffect,
            Self::Defines | Self::References => CauseDirection::NotCausal,
        }
    }
}

/// How [`EdgeKind::cause_direction`] reads an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CauseDirection {
    /// The edge is written effect -> cause and is walked forward.
    EffectToCause,
    /// The edge is written cause -> effect and is walked backward.
    CauseToEffect,
    /// The edge is structural, not causal.
    NotCausal,
}

/// Which authoritative source a part of the graph came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// Effective metadata and site customisations.
    Metadata,
    /// The step and seam registry's override report.
    Seams,
    /// Installed apps and their manifests.
    Apps,
    /// Migration audit runs and release metadata.
    Migrations,
    /// The permission snapshot summary.
    Permissions,
    /// The TypeScript script index and `.d.ts` field references.
    Scripts,
    /// Runtime health signals.
    Health,
    /// Settings changes.
    Settings,
}

impl SourceKind {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Seams => "seams",
            Self::Apps => "apps",
            Self::Migrations => "migrations",
            Self::Permissions => "permissions",
            Self::Scripts => "scripts",
            Self::Health => "health",
            Self::Settings => "settings",
        }
    }
}
