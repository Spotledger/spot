//! The declared-behaviour hook: which divergences a release said would happen.
//!
//! A release ships a **behaviour changelog** (WP-MIG-02): the entries that say "this patch
//! changes the rounding of tax", "this patch stops validating a field". A replay consults it
//! through [`DeclaredBehaviour`]: a divergence a declared entry claims is
//! [`Expected`](crate::DivergenceClass::Expected), every other divergence is
//! [`Unexplained`](crate::DivergenceClass::Unexplained). The trait is the seam, so a replay works
//! without WP-MIG-02 and sharpens when it arrives; [`DeclaredBehaviourList`] is the in-process
//! form, and [`NoDeclarations`] declares nothing.

use crate::divergence::{Divergence, DivergenceKind};

/// One declared behaviour entry: a change the release said would move an outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BehaviourEntry {
    /// The entry's id (the plan change id).
    pub id: String,
    /// The DocType it applies to; any when absent.
    pub doctype: Option<String>,
    /// A field-path prefix it applies to, for example `totals.tax`; any when absent.
    pub field_prefix: Option<String>,
    /// The divergence kind it applies to; any when absent.
    pub kind: Option<DivergenceKind>,
    /// The largest difference, in minor units, it declares. When absent, any difference matches.
    pub max_minor_units: Option<i128>,
}

impl BehaviourEntry {
    /// An entry with `id` that claims every divergence.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        BehaviourEntry {
            id: id.into(),
            doctype: None,
            field_prefix: None,
            kind: None,
            max_minor_units: None,
        }
    }

    /// The same entry limited to `doctype`.
    #[must_use]
    pub fn for_doctype(mut self, doctype: impl Into<String>) -> Self {
        self.doctype = Some(doctype.into());
        self
    }

    /// The same entry limited to a field-path prefix.
    #[must_use]
    pub fn for_field(mut self, prefix: impl Into<String>) -> Self {
        self.field_prefix = Some(prefix.into());
        self
    }

    /// The same entry limited to a divergence kind.
    #[must_use]
    pub fn of_kind(mut self, kind: DivergenceKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// The same entry limited to a difference of at most `minor_units` minor units.
    #[must_use]
    pub fn within(mut self, minor_units: i128) -> Self {
        self.max_minor_units = Some(minor_units);
        self
    }

    /// Whether this entry claims `divergence`.
    #[must_use]
    pub fn claims(&self, divergence: &Divergence) -> bool {
        if self
            .doctype
            .as_deref()
            .is_some_and(|doctype| doctype != divergence.doctype)
        {
            return false;
        }
        if self
            .field_prefix
            .as_deref()
            .is_some_and(|prefix| !divergence.field.starts_with(prefix))
        {
            return false;
        }
        if self.kind.is_some_and(|kind| kind != divergence.kind) {
            return false;
        }
        if let Some(limit) = self.max_minor_units {
            match divergence.delta {
                Some(delta) if delta.abs() <= limit => {}
                // A bounded entry only claims a numeric divergence within its bound.
                _ => return false,
            }
        }
        true
    }
}

/// What the release metadata declares about behaviour.
///
/// WP-MIG-02's release metadata implements this; a replay works without it through
/// [`NoDeclarations`] and treats every divergence as unexplained.
pub trait DeclaredBehaviour: Send + Sync {
    /// The id of the declared behaviour entry that claims `divergence`, if any.
    fn declared_for(&self, divergence: &Divergence) -> Option<String>;
}

/// A release that declares nothing: every divergence is unexplained.
pub struct NoDeclarations;

impl DeclaredBehaviour for NoDeclarations {
    fn declared_for(&self, _divergence: &Divergence) -> Option<String> {
        None
    }
}

/// The in-process form of a behaviour changelog: a list of declared entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeclaredBehaviourList {
    entries: Vec<BehaviourEntry>,
}

impl DeclaredBehaviourList {
    /// A list of the declared entries.
    #[must_use]
    pub fn new(entries: Vec<BehaviourEntry>) -> Self {
        DeclaredBehaviourList { entries }
    }

    /// The declared entries.
    #[must_use]
    pub fn entries(&self) -> &[BehaviourEntry] {
        &self.entries
    }

    /// Adds one entry.
    pub fn push(&mut self, entry: BehaviourEntry) {
        self.entries.push(entry);
    }
}

impl DeclaredBehaviour for DeclaredBehaviourList {
    fn declared_for(&self, divergence: &Divergence) -> Option<String> {
        self.entries
            .iter()
            .find(|entry| entry.claims(divergence))
            .map(|entry| entry.id.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::divergence::{Divergence, DivergenceClass};

    fn divergence(kind: DivergenceKind, field: &str, delta: Option<i128>) -> Divergence {
        Divergence {
            doctype: "invoice".to_owned(),
            record: "r1".to_owned(),
            source: "s".to_owned(),
            sequence: 1,
            kind,
            field: field.to_owned(),
            before: "0".to_owned(),
            after: "1".to_owned(),
            delta,
            rounding: delta == Some(1),
            class: DivergenceClass::Unexplained,
            declared_by: None,
        }
    }

    #[test]
    fn an_entry_claims_only_what_it_declares() {
        let entry = BehaviourEntry::new("behaviour/rounding")
            .for_doctype("invoice")
            .for_field("totals.tax")
            .within(1);
        assert!(entry.claims(&divergence(DivergenceKind::Amount, "totals.tax", Some(1))));
        assert!(!entry.claims(&divergence(DivergenceKind::Amount, "totals.tax", Some(2))));
        assert!(!entry.claims(&divergence(DivergenceKind::Amount, "totals.total", Some(1))));
        assert!(!entry.claims(&divergence(
            DivergenceKind::Ledger,
            "ledger.ar.debit",
            Some(1)
        )));
    }

    #[test]
    fn a_bounded_entry_does_not_claim_a_status_change() {
        let entry = BehaviourEntry::new("x").within(1);
        assert!(!entry.claims(&divergence(DivergenceKind::Status, "status", None)));
    }

    #[test]
    fn the_list_returns_the_first_matching_id() {
        let list = DeclaredBehaviourList::new(vec![
            BehaviourEntry::new("a").for_field("totals.total"),
            BehaviourEntry::new("b").for_field("totals.tax"),
        ]);
        assert_eq!(
            list.declared_for(&divergence(DivergenceKind::Amount, "totals.tax", Some(1))),
            Some("b".to_owned())
        );
        assert_eq!(
            list.declared_for(&divergence(DivergenceKind::Stock, "stock.x.qty", Some(1))),
            None
        );
        assert_eq!(
            NoDeclarations.declared_for(&divergence(DivergenceKind::Amount, "totals.tax", Some(1))),
            None
        );
    }
}
