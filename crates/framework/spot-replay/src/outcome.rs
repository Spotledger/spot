//! The canonical outcome of one replayed operation: what the old release recorded, and what the
//! new release produced, in a form two of them can be compared field by field.
//!
//! An outcome is **canonicalised** before it is compared: totals are a sorted map keyed by name,
//! and ledger, stock and validation entries are sorted by their own keys. Two runs that produced
//! the same facts in a different order therefore compare equal, and only a real difference shows
//! up as a [`Divergence`](crate::Divergence).
//!
//! Every money and quantity measure is an **integer count of minor units** (ADR-12): a currency's
//! smallest unit for money, and the item's declared scale for a quantity. No floating point enters
//! this crate.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What an operation left the document as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The document was created (a draft, for a posted DocType).
    Created,
    /// The document was updated.
    Updated,
    /// The document was posted.
    Posted,
    /// The document was cancelled.
    Cancelled,
    /// The document was deleted.
    Deleted,
    /// The new release **refused** the operation (its validation failed); the old release allowed
    /// it, or vice versa. A refusal is an outcome, not an error.
    Rejected,
}

/// One ledger posting a posted document produced.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// The account the entry posts to.
    pub account: String,
    /// The debit, in minor units (zero when the entry is a pure credit).
    pub debit: i128,
    /// The credit, in minor units (zero when the entry is a pure debit).
    pub credit: i128,
}

impl LedgerEntry {
    /// An entry on `account`.
    #[must_use]
    pub fn new(account: impl Into<String>, debit: i128, credit: i128) -> Self {
        LedgerEntry {
            account: account.into(),
            debit,
            credit,
        }
    }
}

/// One stock movement a posted document produced.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StockEntry {
    /// The item.
    pub item: String,
    /// The warehouse.
    pub warehouse: String,
    /// The signed movement, in the item's smallest unit (a negative value is an issue).
    pub qty: i128,
}

impl StockEntry {
    /// A movement of `qty` of `item` at `warehouse`.
    #[must_use]
    pub fn new(item: impl Into<String>, warehouse: impl Into<String>, qty: i128) -> Self {
        StockEntry {
            item: item.into(),
            warehouse: warehouse.into(),
            qty,
        }
    }
}

/// One validation error an operation raised, as `(field, code)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ValidationError {
    /// The field the error is about; empty for a document-level error.
    pub field: String,
    /// The stable error code.
    pub code: String,
}

impl ValidationError {
    /// An error on `field` with `code`.
    #[must_use]
    pub fn new(field: impl Into<String>, code: impl Into<String>) -> Self {
        ValidationError {
            field: field.into(),
            code: code.into(),
        }
    }
}

/// The canonical outcome of one operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// The DocType of the document the operation was about.
    pub doctype: String,
    /// The document's record id, as text (a UUIDv7).
    pub record: String,
    /// What the operation left the document as.
    pub status: Status,
    /// Money totals by name, in minor units. Sorted, so two runs compare equal regardless of the
    /// order they were computed in.
    pub totals: BTreeMap<String, i128>,
    /// The ledger postings the operation produced.
    pub ledger: Vec<LedgerEntry>,
    /// The stock movements the operation produced.
    pub stock: Vec<StockEntry>,
    /// The validation errors the operation raised.
    pub validation: Vec<ValidationError>,
}

impl Outcome {
    /// A canonical outcome. The ledger, stock and validation lists are sorted here, so callers may
    /// push them in any order.
    #[must_use]
    pub fn new(
        doctype: impl Into<String>,
        record: impl Into<String>,
        status: Status,
        totals: BTreeMap<String, i128>,
        mut ledger: Vec<LedgerEntry>,
        mut stock: Vec<StockEntry>,
        mut validation: Vec<ValidationError>,
    ) -> Self {
        ledger.sort();
        stock.sort();
        validation.sort();
        Outcome {
            doctype: doctype.into(),
            record: record.into(),
            status,
            totals,
            ledger,
            stock,
            validation,
        }
    }

    /// A canonical outcome that only carries a status: a delete, or a refusal with no detail.
    #[must_use]
    pub fn status_only(
        doctype: impl Into<String>,
        record: impl Into<String>,
        status: Status,
    ) -> Self {
        Outcome::new(
            doctype,
            record,
            status,
            BTreeMap::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    /// Puts the lists back in canonical order. Used after a caller mutated an outcome in place.
    pub fn canonicalise(&mut self) {
        self.ledger.sort();
        self.stock.sort();
        self.validation.sort();
    }

    /// The total under `name`, or zero when the outcome has none.
    #[must_use]
    pub fn total(&self, name: &str) -> i128 {
        self.totals.get(name).copied().unwrap_or(0)
    }

    /// The SHA-256 of the outcome's canonical JSON form, as lower-case hex. Two outcomes that
    /// differ in any fact (or in the order of a list) get different digests.
    #[must_use]
    pub fn digest(&self) -> String {
        let text = serde_json::to_string(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn canonical_order_does_not_change_the_digest() {
        let a = Outcome::new(
            "invoice",
            "r1",
            Status::Posted,
            BTreeMap::from([("total".to_owned(), 1_234)]),
            vec![
                LedgerEntry::new("ar", 1_234, 0),
                LedgerEntry::new("sales", 0, 1_234),
            ],
            vec![StockEntry::new("bolt", "main", -2)],
            vec![ValidationError::new("total", "document.required")],
        );
        let b = Outcome::new(
            "invoice",
            "r1",
            Status::Posted,
            BTreeMap::from([("total".to_owned(), 1_234)]),
            vec![
                LedgerEntry::new("sales", 0, 1_234),
                LedgerEntry::new("ar", 1_234, 0),
            ],
            vec![StockEntry::new("bolt", "main", -2)],
            vec![ValidationError::new("total", "document.required")],
        );
        assert_eq!(a, b);
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn a_changed_total_changes_the_digest() {
        let a = Outcome::status_only("invoice", "r1", Status::Posted);
        let mut b = a.clone();
        b.totals.insert("total".to_owned(), 1);
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn total_reads_a_missing_name_as_zero() {
        let a = Outcome::status_only("invoice", "r1", Status::Posted);
        assert_eq!(a.total("tax"), 0);
    }
}
