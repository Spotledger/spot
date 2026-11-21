//! Where the checker learns what a field *is* (ADR-080 section 5.6, ADR-070).
//!
//! A contract names the fields that may cross. That list must never carry a
//! field the metadata classifies as a **secret**: a secret never leaves its
//! tenant, not even to a participant that asked for it (ADR-058 rule 6,
//! ADR-070 section 12). The checker asks this trait rather than reading the
//! metadata crate, so `spot-bpm-contract` stays free of it and a host passes
//! whatever view it has.
//!
//! A host with DocType metadata adapts it here: [`FieldClass::Secret`] for a
//! field of metadata kind `secret` or carrying the `secret` tag, and the other
//! classes for the rest. [`PublicFields`] answers "public" to everything and is
//! what a contract checked without metadata uses; [`ClassifiedFields`] is the
//! fixture hosts and tests drive.

use std::collections::BTreeMap;

/// What a field's metadata says about it, narrowed to what a contract needs to
/// know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FieldClass {
    /// May be shown and exchanged.
    #[default]
    Public,
    /// Internal but not personal.
    Internal,
    /// Personal data. It may cross when the contract lists it by name — which
    /// is the only way a contract lists anything.
    Personal,
    /// A secret or special-category value. It never crosses.
    Secret,
}

impl FieldClass {
    /// Whether a field with this class may never leave the tenant.
    #[must_use]
    pub fn is_secret(self) -> bool {
        self == Self::Secret
    }
}

/// What the checker asks the metadata about a field.
pub trait FieldMetaView {
    /// The class of `field`.
    ///
    /// The default is [`FieldClass::Public`], so a host that does not track
    /// field classes still implements the trait by declaring nothing.
    fn class(&self, field: &str) -> FieldClass {
        let _ = field;
        FieldClass::Public
    }
}

/// A metadata view that classifies every field as public.
#[derive(Debug, Clone, Copy, Default)]
pub struct PublicFields;

impl FieldMetaView for PublicFields {}

/// A metadata view built from a plain table, for a host with metadata in hand
/// and for tests.
#[derive(Debug, Clone, Default)]
pub struct ClassifiedFields {
    classes: BTreeMap<String, FieldClass>,
}

impl ClassifiedFields {
    /// An empty view: every field is public.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same view with one more classified field.
    #[must_use]
    pub fn with(mut self, field: impl Into<String>, class: FieldClass) -> Self {
        self.classes.insert(field.into(), class);
        self
    }
}

impl FieldMetaView for ClassifiedFields {
    fn class(&self, field: &str) -> FieldClass {
        self.classes.get(field).copied().unwrap_or_default()
    }
}
