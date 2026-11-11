//! Comparing two outcomes: the field-level divergences between what the old release recorded and
//! what the new release produced.
//!
//! [`diff`] compares a recorded [`Outcome`] with a re-executed one and returns one
//! [`Divergence`] per fact that differs. Every numeric difference carries its signed delta in
//! minor units, and a difference of **exactly one minor unit** is flagged
//! [`rounding`](Divergence::rounding): the signature of a changed rounding rule, which is what the
//! acceptance test seeds. A divergence is [`Unexplained`](DivergenceClass::Unexplained) until a
//! declared behaviour entry claims it ([`DeclaredBehaviour`](crate::DeclaredBehaviour)).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::outcome::{LedgerEntry, Outcome, StockEntry, ValidationError};

/// What kind of fact differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceKind {
    /// The document's state after the operation.
    Status,
    /// A money total.
    Amount,
    /// A ledger posting.
    Ledger,
    /// A stock movement.
    Stock,
    /// A validation result (an error the old release raised, or the new one does).
    Validation,
}

/// How a divergence was classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceClass {
    /// A declared behaviour entry (WP-MIG-02) claims it: the release said this would change.
    Expected,
    /// Nothing declared it.
    Unexplained,
}

/// One fact that differs between the recorded and the re-executed outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divergence {
    /// The DocType.
    pub doctype: String,
    /// The document's record id, as text.
    pub record: String,
    /// The journal source the operation came from.
    pub source: String,
    /// The position within the source.
    pub sequence: u64,
    /// What kind of fact differs.
    pub kind: DivergenceKind,
    /// The field path, for example `totals.tax`, `ledger.ar.debit`, `status`.
    pub field: String,
    /// The recorded value, as text.
    pub before: String,
    /// The re-executed value, as text.
    pub after: String,
    /// The signed difference in minor units, for a numeric fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<i128>,
    /// Whether the difference is exactly one minor unit: a changed rounding rule.
    pub rounding: bool,
    /// The classification.
    pub class: DivergenceClass,
    /// The declared behaviour entry that explains it, when the class is expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_by: Option<String>,
}

impl Divergence {
    /// A status divergence.
    fn status(entry: &EntryRef, before: &str, after: &str) -> Self {
        Divergence {
            doctype: entry.doctype.clone(),
            record: entry.record.clone(),
            source: entry.source.clone(),
            sequence: entry.sequence,
            kind: DivergenceKind::Status,
            field: "status".to_owned(),
            before: before.to_owned(),
            after: after.to_owned(),
            delta: None,
            rounding: false,
            class: DivergenceClass::Unexplained,
            declared_by: None,
        }
    }

    /// A numeric divergence: an amount, a ledger leg or a stock movement.
    fn numeric(
        entry: &EntryRef,
        kind: DivergenceKind,
        field: String,
        before: i128,
        after: i128,
    ) -> Self {
        let delta = after - before;
        Divergence {
            doctype: entry.doctype.clone(),
            record: entry.record.clone(),
            source: entry.source.clone(),
            sequence: entry.sequence,
            kind,
            field,
            before: before.to_string(),
            after: after.to_string(),
            delta: Some(delta),
            // Exactly one minor unit: the mark of a rounding change (ADR-12 minor units).
            rounding: delta.unsigned_abs() == 1,
            class: DivergenceClass::Unexplained,
            declared_by: None,
        }
    }

    /// A validation divergence: an error present on one side only.
    fn validation(entry: &EntryRef, field: String, before: &str, after: &str) -> Self {
        Divergence {
            doctype: entry.doctype.clone(),
            record: entry.record.clone(),
            source: entry.source.clone(),
            sequence: entry.sequence,
            kind: DivergenceKind::Validation,
            field,
            before: before.to_owned(),
            after: after.to_owned(),
            delta: None,
            rounding: false,
            class: DivergenceClass::Unexplained,
            declared_by: None,
        }
    }

    /// The same divergence attributed to the journal source and sequence it came from.
    #[must_use]
    pub fn with_origin(mut self, source: impl Into<String>, sequence: u64) -> Self {
        self.source = source.into();
        self.sequence = sequence;
        self
    }

    /// Whether this divergence is a one-minor-unit rounding difference.
    #[must_use]
    pub fn is_rounding(&self) -> bool {
        self.rounding
    }

    /// Whether a declared behaviour entry claimed this divergence.
    #[must_use]
    pub fn is_expected(&self) -> bool {
        self.class == DivergenceClass::Expected
    }
}

/// The identity of the operation a divergence belongs to.
struct EntryRef {
    doctype: String,
    record: String,
    source: String,
    sequence: u64,
}

/// Diffs the recorded `old` against the re-executed `new`, field by field.
///
/// The result is deterministic: status first, then amounts in name order, then ledger legs, then
/// stock movements, then validation results.
#[must_use]
pub fn diff(old: &Outcome, new: &Outcome) -> Vec<Divergence> {
    let entry = EntryRef {
        doctype: old.doctype.clone(),
        record: old.record.clone(),
        source: String::new(),
        sequence: 0,
    };
    let mut out = Vec::new();

    if old.status != new.status {
        out.push(Divergence::status(
            &entry,
            status_name(old.status),
            status_name(new.status),
        ));
    }

    // Totals: the union of names, in name order.
    let mut names: Vec<&String> = old.totals.keys().chain(new.totals.keys()).collect();
    names.sort();
    names.dedup();
    for name in names {
        let before = old.total(name);
        let after = new.total(name);
        if before != after {
            out.push(Divergence::numeric(
                &entry,
                DivergenceKind::Amount,
                format!("totals.{name}"),
                before,
                after,
            ));
        }
    }

    // Ledger: aggregate each account's debit and credit legs, then compare per account.
    for (account, (before_debit, before_credit)) in ledger_by_account(&old.ledger) {
        let (after_debit, after_credit) = ledger_by_account(&new.ledger)
            .get(&account)
            .copied()
            .unwrap_or((0, 0));
        if before_debit != after_debit {
            out.push(Divergence::numeric(
                &entry,
                DivergenceKind::Ledger,
                format!("ledger.{account}.debit"),
                before_debit,
                after_debit,
            ));
        }
        if before_credit != after_credit {
            out.push(Divergence::numeric(
                &entry,
                DivergenceKind::Ledger,
                format!("ledger.{account}.credit"),
                before_credit,
                after_credit,
            ));
        }
    }
    for (account, (after_debit, after_credit)) in ledger_by_account(&new.ledger) {
        if ledger_by_account(&old.ledger).contains_key(&account) {
            continue;
        }
        if after_debit != 0 {
            out.push(Divergence::numeric(
                &entry,
                DivergenceKind::Ledger,
                format!("ledger.{account}.debit"),
                0,
                after_debit,
            ));
        }
        if after_credit != 0 {
            out.push(Divergence::numeric(
                &entry,
                DivergenceKind::Ledger,
                format!("ledger.{account}.credit"),
                0,
                after_credit,
            ));
        }
    }

    // Stock: aggregate each (item, warehouse) movement, then compare.
    for (key, before) in stock_by_location(&old.stock) {
        let after = stock_by_location(&new.stock)
            .get(&key)
            .copied()
            .unwrap_or(0);
        if before != after {
            out.push(Divergence::numeric(
                &entry,
                DivergenceKind::Stock,
                format!("stock.{key}.qty"),
                before,
                after,
            ));
        }
    }
    for (key, after) in stock_by_location(&new.stock) {
        if stock_by_location(&old.stock).contains_key(&key) {
            continue;
        }
        if after != 0 {
            out.push(Divergence::numeric(
                &entry,
                DivergenceKind::Stock,
                format!("stock.{key}.qty"),
                0,
                after,
            ));
        }
    }

    // Validation: an error on one side only, in (field, code) order.
    for error in &new.validation {
        if !old.validation.contains(error) {
            out.push(Divergence::validation(
                &entry,
                format!("validation.{}", display_field(error)),
                "",
                &error.code,
            ));
        }
    }
    for error in &old.validation {
        if !new.validation.contains(error) {
            out.push(Divergence::validation(
                &entry,
                format!("validation.{}", display_field(error)),
                &error.code,
                "",
            ));
        }
    }

    out
}

/// Sums a ledger's debit and credit legs per account.
fn ledger_by_account(entries: &[LedgerEntry]) -> BTreeMap<String, (i128, i128)> {
    let mut out: BTreeMap<String, (i128, i128)> = BTreeMap::new();
    for entry in entries {
        let slot = out.entry(entry.account.clone()).or_insert((0, 0));
        slot.0 += entry.debit;
        slot.1 += entry.credit;
    }
    out
}

/// Sums stock movements per `item@warehouse`.
fn stock_by_location(entries: &[StockEntry]) -> BTreeMap<String, i128> {
    let mut out: BTreeMap<String, i128> = BTreeMap::new();
    for entry in entries {
        *out.entry(format!("{}@{}", entry.item, entry.warehouse))
            .or_insert(0) += entry.qty;
    }
    out
}

/// A validation error's field, or `document` when it is document-level.
fn display_field(error: &ValidationError) -> &str {
    if error.field.is_empty() {
        "document"
    } else {
        &error.field
    }
}

/// The snake-case name of a status.
fn status_name(status: crate::outcome::Status) -> &'static str {
    use crate::outcome::Status;
    match status {
        Status::Created => "created",
        Status::Updated => "updated",
        Status::Posted => "posted",
        Status::Cancelled => "cancelled",
        Status::Deleted => "deleted",
        Status::Rejected => "rejected",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::outcome::Status;

    fn outcome(total: i128) -> Outcome {
        Outcome::new(
            "invoice",
            "r1",
            Status::Posted,
            BTreeMap::from([("tax".to_owned(), total)]),
            vec![LedgerEntry::new("tax_payable", 0, total)],
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn a_one_minor_unit_difference_is_flagged_rounding() {
        let divs = diff(&outcome(100), &outcome(101));
        assert_eq!(divs.len(), 2, "the total and the ledger leg");
        for div in &divs {
            assert!(div.is_rounding(), "{div:?}");
            assert_eq!(div.delta, Some(1));
        }
    }

    #[test]
    fn a_larger_difference_is_not_rounding() {
        let divs = diff(&outcome(100), &outcome(103));
        assert!(divs.iter().all(|d| !d.is_rounding()));
        assert_eq!(divs[0].delta, Some(3));
    }

    #[test]
    fn equal_outcomes_diverge_in_nothing() {
        assert!(diff(&outcome(100), &outcome(100)).is_empty());
    }

    #[test]
    fn a_status_change_is_a_divergence() {
        let a = outcome(100);
        let mut b = outcome(100);
        b.status = Status::Rejected;
        let divs = diff(&a, &b);
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].kind, DivergenceKind::Status);
        assert_eq!(
            (divs[0].before.as_str(), divs[0].after.as_str()),
            ("posted", "rejected")
        );
    }

    #[test]
    fn a_new_validation_error_is_a_divergence() {
        let a = outcome(100);
        let mut b = outcome(100);
        b.validation
            .push(ValidationError::new("tax", "document.required"));
        let divs = diff(&a, &b);
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].kind, DivergenceKind::Validation);
        assert_eq!(divs[0].field, "validation.tax");
    }
}
