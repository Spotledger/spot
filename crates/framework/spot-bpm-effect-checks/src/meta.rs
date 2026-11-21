//! What the checks need to know that neither the process nor the registry says.
//!
//! The process model has no field for "this backward edge undoes" or "this node revises on
//! rewind", and the registry does not know which classes the site has registered. The host
//! answers those through [`EffectMeta`]; [`PolicyMeta`] is a plain table implementation.

use std::collections::BTreeSet;

use spot_app_api::EffectClassName;
use spot_bpm_model::{Edge, NodeId};
use spot_effect_hold::{EffectClass, PolicyTable};

/// What a backward edge does to the effects already produced (ADR-080 6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeMode {
    /// An ordinary edge.
    Forward,
    /// Goes back and keeps the effects; the next pass revises the records.
    Rewind,
    /// Goes back and reverses the effects.
    Undo,
}

/// The label that marks an undo edge when the host does not say otherwise.
pub const LABEL_UNDO: &str = "undo";
/// The label that marks a rewind edge when the host does not say otherwise.
pub const LABEL_REWIND: &str = "rewind";

/// Host-supplied facts for the checks. Pure: no method may do I/O.
pub trait EffectMeta {
    /// What `edge` does when it goes back. The default reads the edge label (`undo` or `rewind`,
    /// compared whole, case-sensitively); an unlabelled edge is forward, so a missing mark never
    /// creates a problem that is not there, and the designer shows the mode on every back edge.
    fn edge_mode(&self, edge: &Edge) -> EdgeMode {
        match edge.label.as_deref() {
            Some(LABEL_UNDO) => EdgeMode::Undo,
            Some(LABEL_REWIND) => EdgeMode::Rewind,
            _ => EdgeMode::Forward,
        }
    }

    /// Whether the process declares how `node` revises the record its earlier pass created.
    fn has_revision(&self, node: &NodeId) -> bool;

    /// Whether the site knows the effect class.
    fn is_known_class(&self, class: &EffectClassName) -> bool;

    /// Whether the class is never held (leaves at once, cannot be discarded).
    fn is_never_held(&self, class: &EffectClassName) -> bool;
}

/// A table-driven [`EffectMeta`].
#[derive(Clone, Debug)]
pub struct PolicyMeta {
    policies: PolicyTable,
    known: Option<BTreeSet<String>>,
    revisions: BTreeSet<NodeId>,
}

impl Default for PolicyMeta {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyMeta {
    /// Platform defaults: every class is known, only fiscal transmissions are never held, no node
    /// declares a revision.
    #[must_use]
    pub fn new() -> Self {
        Self {
            policies: PolicyTable::new(),
            known: None,
            revisions: BTreeSet::new(),
        }
    }

    /// Uses this hold policy table.
    #[must_use]
    pub fn with_policies(mut self, policies: PolicyTable) -> Self {
        self.policies = policies;
        self
    }

    /// Restricts the known classes to these (an unlisted class is unknown). Without this call
    /// every class is known.
    #[must_use]
    pub fn with_known_classes<'a>(mut self, classes: impl IntoIterator<Item = &'a str>) -> Self {
        self.known = Some(classes.into_iter().map(str::to_owned).collect());
        self
    }

    /// Declares that `node` revises its earlier record on a rewind.
    #[must_use]
    pub fn with_revision(mut self, node: NodeId) -> Self {
        self.revisions.insert(node);
        self
    }
}

impl EffectMeta for PolicyMeta {
    fn has_revision(&self, node: &NodeId) -> bool {
        self.revisions.contains(node)
    }

    fn is_known_class(&self, class: &EffectClassName) -> bool {
        self.known
            .as_ref()
            .is_none_or(|known| known.contains(class.as_str()))
    }

    fn is_never_held(&self, class: &EffectClassName) -> bool {
        self.policies.is_never_hold(&class_of(class))
    }
}

/// The hold crate's class for a registry class name: the platform's own names map to their
/// variant, anything else is `Other` (held by default).
fn class_of(class: &EffectClassName) -> EffectClass {
    match class.as_str() {
        "mail" => EffectClass::Mail,
        "bank_api" => EffectClass::BankApi,
        "webhook" => EffectClass::Webhook,
        "connector" => EffectClass::Connector,
        "sms" => EffectClass::Sms,
        "push" => EffectClass::Push,
        "edge_sync" => EffectClass::EdgeSync,
        "fiscal_transmission" => EffectClass::FiscalTransmission,
        other => EffectClass::Other(other.to_owned()),
    }
}
