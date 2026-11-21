//! What a check reports.

use spot_bpm_model::NodeId;

/// How serious a problem is. The publish gate refuses on [`Severity::Error`] only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Shown to the designer; does not stop a publish.
    Warning,
    /// Stops a publish.
    Error,
}

/// The kind of a problem. The stable token is [`Problem::token`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProblemKind {
    /// A backward edge marked undo whose range includes an irreversible effect.
    UndoPastIrreversible,
    /// A document node whose operation has an effect without a declared reversal.
    NoDeclaredReversal,
    /// A commit point whose region holds nothing.
    EmptyCommitPoint,
    /// A rewind that would create a second record because no revision is mapped.
    RewindWouldDuplicate,
    /// A declared effect class the site does not know.
    UnknownEffectClass,
    /// A never-held effect inside a held region: it leaves at once and cannot be discarded.
    HeldRegionContainsNeverHeld,
    /// A document node whose (record type, operation) the registry plans no step for at all: its
    /// effects are unknown, not empty, so an undo across it is unchecked. A warning, because a
    /// record type may legitimately have no steps.
    NoDeclaredPipeline,
    /// The process has more nodes, edges, commit points or marked backward edges than the checks accept. The
    /// process is not checked further; it is refused.
    TooLarge,
}

impl ProblemKind {
    /// The stable token for the designer and for tests.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::UndoPastIrreversible => "undo_past_irreversible",
            Self::NoDeclaredReversal => "no_declared_reversal",
            Self::EmptyCommitPoint => "empty_commit_point",
            Self::RewindWouldDuplicate => "rewind_would_duplicate",
            Self::UnknownEffectClass => "unknown_effect_class",
            Self::HeldRegionContainsNeverHeld => "held_region_contains_never_held",
            Self::NoDeclaredPipeline => "no_declared_pipeline",
            Self::TooLarge => "too_large",
        }
    }

    /// The severity of this kind. Commit-point findings are warnings, as in
    /// `spot-bpm-commit`: they inform the designer and do not block a save.
    #[must_use]
    pub fn severity(self) -> Severity {
        match self {
            Self::EmptyCommitPoint
            | Self::HeldRegionContainsNeverHeld
            | Self::NoDeclaredPipeline => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

/// One design-time problem. It carries ids and class names only (grammar-checked tokens), never a
/// field value or free text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Problem {
    /// What was found.
    pub kind: ProblemKind,
    /// How serious it is.
    pub severity: Severity,
    /// The node the problem is about (the node causing the effect, the commit point, the node
    /// that would create twice), when there is one.
    pub node: Option<NodeId>,
    /// For a problem about a backward edge: the edge, as `(from, to)`.
    pub edge: Option<(NodeId, NodeId)>,
    /// The effect class involved, when there is one.
    pub class: Option<String>,
}

impl Problem {
    pub(crate) fn new(kind: ProblemKind) -> Self {
        Self {
            kind,
            severity: kind.severity(),
            node: None,
            edge: None,
            class: None,
        }
    }

    pub(crate) fn at(mut self, node: &NodeId) -> Self {
        self.node = Some(node.clone());
        self
    }

    pub(crate) fn on_edge(mut self, from: &NodeId, to: &NodeId) -> Self {
        self.edge = Some((from.clone(), to.clone()));
        self
    }

    pub(crate) fn of_class(mut self, class: &str) -> Self {
        self.class = Some(class.to_owned());
        self
    }

    /// The stable token of the problem's kind.
    #[must_use]
    pub fn token(&self) -> &'static str {
        self.kind.token()
    }

    /// Whether the problem stops a publish.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Whether an effect stays inside the operation or leaves the building. Ordered internal first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Where {
    /// Written to append-only stores inside the operation.
    Internal,
    /// Mail, connector calls, webhooks, messages to another organisation.
    External,
}

impl From<spot_app_api::EffectKind> for Where {
    fn from(kind: spot_app_api::EffectKind) -> Self {
        match kind {
            spot_app_api::EffectKind::Internal => Self::Internal,
            // A kind a later contract adds is treated as leaving the building.
            _ => Self::External,
        }
    }
}

/// How an effect is undone. Ordered from most to least reversible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Undo {
    /// By a lifecycle operation.
    Reversible,
    /// By a named compensating action.
    Compensable,
    /// Not at all: a point of no return.
    Irreversible,
}

impl From<spot_app_api::Reversibility> for Undo {
    fn from(r: spot_app_api::Reversibility) -> Self {
        match r {
            spot_app_api::Reversibility::Reversible => Self::Reversible,
            spot_app_api::Reversibility::Compensable => Self::Compensable,
            // A summary a later contract adds is treated as a point of no return (fail closed).
            _ => Self::Irreversible,
        }
    }
}
