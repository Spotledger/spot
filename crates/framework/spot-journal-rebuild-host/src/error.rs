//! Typed errors with stable `journal_rebuild_host.*` codes. A message carries counts and
//! positions, never a record, a key or a connection string.

/// Why a rebuild was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// A live node holds the site's lease: a rebuild is offline only.
    #[error("the site is running; stop it before rebuilding")]
    SiteRunning,
    /// The backup's recorded head is not in the journal.
    #[error("the backup's head is not in the journal")]
    BaseNotInJournal,
    /// The request, a manifest or a snapshot names another tenant than the site's.
    #[error("the journal belongs to another tenant than the site")]
    TenantMismatch,
    /// A derived entity has no registered projector.
    #[error("the derived entity {entity} has no registered projector")]
    ProjectorMissing {
        /// The entity.
        entity: String,
    },
    /// The planner refused (carries the planner's own code).
    #[error("the rebuild plan was refused ({0})")]
    PlanRefused(&'static str),
    /// The target (`before <timestamp>`) cannot be resolved to a position.
    #[error("the target cannot be resolved to a journal position")]
    TargetUnresolved,
    /// A checkpoint exists for a different plan; resuming would mix two rebuilds.
    #[error("the saved checkpoint is for a different plan; discard it or finish that rebuild")]
    CheckpointForOtherPlan,
    /// The checkpoint could not be read or written.
    #[error("the rebuild checkpoint could not be read or written")]
    CheckpointFailed,
    /// A previous rebuild did not finish: the database is half built.
    #[error("a rebuild of this database did not finish")]
    Incomplete,
    /// The executor refused or failed (carries its code); the checkpoint stays for a resume.
    #[error("the rebuild stopped ({0})")]
    RebuildFailed(String),
    /// The feed could not adopt the journal's position.
    #[error("the outbox position could not be adopted")]
    FeedFailed,
    /// A projector failed.
    #[error("the projector {0} failed")]
    ProjectorFailed(String),
    /// An invariant did not hold after the rebuild.
    #[error("an invariant does not hold: {0}")]
    InvariantFailed(String),
    /// The journal could not be read.
    #[error("the journal could not be read")]
    JournalFailed,
}

impl HostError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SiteRunning => "journal_rebuild_host.site_running",
            Self::BaseNotInJournal => "journal_rebuild_host.base_not_in_journal",
            Self::TenantMismatch => "journal_rebuild_host.tenant_mismatch",
            Self::ProjectorMissing { .. } => "journal_rebuild_host.projector_missing",
            Self::PlanRefused(_) => "journal_rebuild_host.plan_refused",
            Self::TargetUnresolved => "journal_rebuild_host.target_unresolved",
            Self::CheckpointForOtherPlan => "journal_rebuild_host.checkpoint_for_other_plan",
            Self::CheckpointFailed => "journal_rebuild_host.checkpoint_failed",
            Self::Incomplete => "journal_rebuild_host.incomplete",
            Self::RebuildFailed(_) => "journal_rebuild_host.rebuild_failed",
            Self::FeedFailed => "journal_rebuild_host.feed_failed",
            Self::ProjectorFailed(_) => "journal_rebuild_host.projector_failed",
            Self::InvariantFailed(_) => "journal_rebuild_host.invariant_failed",
            Self::JournalFailed => "journal_rebuild_host.journal_failed",
        }
    }
}
