//! Scope keys and the declared digest plan.
//!
//! A **scope key** is an opaque `(key type, value)` pair a node holds, for example
//! `(company, C1)` or `(warehouse, W7)` (ADR-062). Apps register key types; the platform never
//! interprets them, and neither does this crate: a [`Scope`] is compared and printed, nothing
//! more.
//!
//! Which entities and fields a scope's [`Digest`](crate::Digest) covers is a **policy**
//! decision (ADR-063, still proposed). This crate keeps it behind the plain [`DigestPlan`]
//! value, so the reconciler is written once and the policy is supplied by whoever owns it.

use spot_storage_contract::Name;

/// An opaque scope key: a `(key type, value)` pair a node holds (ADR-062).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope {
    /// The key type an app registered, for example `company`.
    pub key_type: String,
    /// The key value, for example `C1`.
    pub value: String,
}

impl Scope {
    /// A scope key.
    pub fn new(key_type: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key_type: key_type.into(),
            value: value.into(),
        }
    }

    /// The scope key as `key_type=value`, for a metric label or a log line.
    pub fn label(&self) -> String {
        format!("{}={}", self.key_type, self.value)
    }
}

/// A grouped sum: a value field summed per distinct key field of one entity.
///
/// The GL balance by account is `GroupSpec { entity: gl_entry, key_field: account,
/// value_field: amount }`; the stock quantity by item is `GroupSpec { entity: stock_ledger,
/// key_field: item, value_field: qty }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupSpec {
    /// The entity whose records carry the values.
    pub entity: Name,
    /// The field grouped by (the account or the item).
    pub key_field: Name,
    /// The integer field summed.
    pub value_field: Name,
}

impl GroupSpec {
    /// A grouped sum.
    pub fn new(entity: Name, key_field: Name, value_field: Name) -> Self {
        Self {
            entity,
            key_field,
            value_field,
        }
    }
}

/// Which entities and fields a scope's digest covers.
///
/// The plan is **declared**, never guessed: a DocType is counted only when the plan names it, a
/// total is summed only for a declared field, and a group is summed only for a declared
/// entity. Money and quantities are summed as exact integers (ADR-12); the plan's fields are
/// integer fields holding minor units or whole quantities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigestPlan {
    /// The field holding a record's human-readable name, the key the range digest is sorted by.
    pub name_field: Name,
    /// Entities whose record count is part of the digest.
    pub counted: Vec<Name>,
    /// `(entity, field)` pairs whose integer values are summed, in the order declared.
    pub totals: Vec<(Name, Name)>,
    /// The GL balance by account, when the scope has one.
    pub gl: Option<GroupSpec>,
    /// The stock quantity by item, when the scope has one.
    pub stock: Option<GroupSpec>,
}

impl DigestPlan {
    /// An empty plan keyed by `name_field`.
    pub fn new(name_field: Name) -> Self {
        Self {
            name_field,
            counted: Vec::new(),
            totals: Vec::new(),
            gl: None,
            stock: None,
        }
    }

    /// Adds an entity whose count is part of the digest.
    pub fn counting(mut self, entity: Name) -> Self {
        self.counted.push(entity);
        self
    }

    /// Adds an integer total field of an entity.
    pub fn totalling(mut self, entity: Name, field: Name) -> Self {
        self.totals.push((entity, field));
        self
    }

    /// Sets the GL balance grouped sum.
    pub fn with_gl(mut self, spec: GroupSpec) -> Self {
        self.gl = Some(spec);
        self
    }

    /// Sets the stock quantity grouped sum.
    pub fn with_stock(mut self, spec: GroupSpec) -> Self {
        self.stock = Some(spec);
        self
    }

    /// Every entity the plan reads: the counted entities plus the grouped sums' entities.
    pub fn entities(&self) -> Vec<Name> {
        let mut out = self.counted.clone();
        if let Some(gl) = &self.gl {
            out.push(gl.entity.clone());
        }
        if let Some(stock) = &self.stock {
            out.push(stock.entity.clone());
        }
        out
    }
}
