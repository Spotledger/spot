//! Declaring policies and resolving them over effective metadata.
//!
//! [`PolicySet`] is what a site (or an app's metadata) declared: a DocType name to a
//! [`SyncPolicy`], built from the text form of [`crate::text`]. [`resolve`] merges it over the
//! effective metadata [`EffectiveMeta`], so every DocType the metadata holds — including a
//! DocType created at runtime and any customisation already merged into it — gets a policy,
//! defaulting to `local`. A DocType syncs by configuration alone; no code names it.
//!
//! Child tables follow their parent (ADR-062 section 3, ADR-063 section 1): a DocType reached by
//! a `table` or `table_multiselect` field carries its parent's resolved policy, because a
//! document travels with its child rows.

use std::collections::BTreeMap;

use spot_meta::{EffectiveMeta, FieldType};

use crate::error::PolicyError;
use crate::policy::SyncPolicy;
use crate::text::PolicyProperty;

/// The policies a site declared, keyed by DocType name.
///
/// Built from the metadata text form: [`PolicySet::set`] applies one [`PolicyProperty`],
/// [`PolicySet::parse`] parses a key/value pair first. A declaration for a DocType the metadata
/// does not hold is kept and reported by [`crate::verify`] as `sync_policy.unknown_doctype`.
#[derive(Clone, Debug, Default)]
pub struct PolicySet {
    declared: BTreeMap<String, SyncPolicy>,
}

impl PolicySet {
    /// An empty set: every DocType resolves to the `local` default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces a DocType's policy.
    pub fn declare(&mut self, doctype: &str, policy: SyncPolicy) {
        self.declared.insert(doctype.to_owned(), policy);
    }

    /// Applies one property to a DocType's declared policy, starting from the `local` default.
    pub fn set(&mut self, doctype: &str, property: &PolicyProperty) {
        let entry = self
            .declared
            .entry(doctype.to_owned())
            .or_insert_with(SyncPolicy::local);
        property.apply_to(entry);
    }

    /// Parses one key/value property and applies it to a DocType.
    ///
    /// # Errors
    ///
    /// Whatever [`PolicyProperty::parse`] returns.
    pub fn parse(&mut self, doctype: &str, key: &str, value: &str) -> Result<(), PolicyError> {
        let property = PolicyProperty::parse(key, value)?;
        self.set(doctype, &property);
        Ok(())
    }

    /// The declared policy for a DocType, when one was declared.
    #[must_use]
    pub fn get(&self, doctype: &str) -> Option<&SyncPolicy> {
        self.declared.get(doctype)
    }

    /// The declared DocType names, in order.
    pub fn doctypes(&self) -> impl Iterator<Item = &str> {
        self.declared.keys().map(String::as_str)
    }

    /// How many policies were declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.declared.len()
    }

    /// Whether nothing was declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }
}

/// Every DocType's resolved policy: what the sync sender, the receiver and the partition checks
/// consult through [`crate::PolicyLookup`].
#[derive(Clone, Debug, Default)]
pub struct PolicyTable {
    entries: BTreeMap<String, SyncPolicy>,
    unknown: Vec<String>,
    local: SyncPolicy,
}

impl PolicyTable {
    /// The policy of a DocType the table holds.
    #[must_use]
    pub fn get(&self, doctype: &str) -> Option<&SyncPolicy> {
        self.entries.get(doctype)
    }

    /// The policy of a DocType; a DocType the table does not hold is `local`, the default.
    #[must_use]
    pub fn policy(&self, doctype: &str) -> &SyncPolicy {
        self.entries.get(doctype).unwrap_or(&self.local)
    }

    /// Every resolved DocType, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SyncPolicy)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// How many DocTypes were resolved.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no DocType was resolved (empty metadata).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Declared DocType names the metadata does not hold, in order. [`crate::verify`] turns each
    /// into a `sync_policy.unknown_doctype` problem.
    #[must_use]
    pub fn unknown(&self) -> &[String] {
        &self.unknown
    }
}

/// Resolves every DocType in `meta` to a policy: the declared one, or `local`.
///
/// Runtime-created DocTypes and customisations are included because they are part of the
/// effective metadata's definitions. Child tables follow their parent. Declarations naming a
/// DocType the metadata does not hold are kept in [`PolicyTable::unknown`].
#[must_use]
pub fn resolve(meta: &EffectiveMeta, declared: &PolicySet) -> PolicyTable {
    let mut entries: BTreeMap<String, SyncPolicy> = BTreeMap::new();
    for definition in meta.definitions() {
        let policy = declared
            .get(&definition.name)
            .cloned()
            .unwrap_or_else(SyncPolicy::local);
        entries.insert(definition.name.clone(), policy);
    }

    propagate_children(meta, &mut entries);

    let unknown = declared
        .doctypes()
        .filter(|name| meta.definition(name).is_none())
        .map(str::to_owned)
        .collect();

    PolicyTable {
        entries,
        unknown,
        local: SyncPolicy::local(),
    }
}

/// Gives every child DocType its parent's resolved policy (a document travels with its child
/// rows). A child with no parent in the metadata keeps its own resolved policy.
fn propagate_children(meta: &EffectiveMeta, entries: &mut BTreeMap<String, SyncPolicy>) {
    let mut parent_of: BTreeMap<&str, &str> = BTreeMap::new();
    for definition in meta.definitions() {
        for field in &definition.fields {
            if matches!(field.ty, FieldType::Table | FieldType::TableMultiselect)
                && let Some(child) = field.options.as_deref()
            {
                parent_of.entry(child).or_insert(definition.name.as_str());
            }
        }
    }

    // A child may itself have children, so propagate to a fixpoint (bounded by the DocType
    // count: every round pushes one level down).
    for _ in 0..=meta.definitions().len() {
        let mut changed = false;
        for (child, parent) in &parent_of {
            let Some(parent_policy) = entries.get(*parent).cloned() else {
                continue;
            };
            if let Some(entry) = entries.get_mut(*child)
                && *entry != parent_policy
            {
                *entry = parent_policy;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}
