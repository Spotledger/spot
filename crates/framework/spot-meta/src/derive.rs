//! Computed inheritance of protection groups from inputs to derived fields (ADR-070 section 12,
//! rule 5). A derived field is secured by the groups of all its inputs, transitively.
//!
//! This module defines the input and the algorithm. Actual derivations come from expressions
//! (WP-MET-02: `fetch_from`, `depends_on`, computed fields, rollups, report columns) and from
//! explicit declaration (a test source for now).
//!
//! # The algorithm
//!
//! `compute_inheritance` finds, for every derived field, the union of the groups of all its
//! inputs, transitively (if a field is derived from another derived field, we include both sets).
//! The result excludes the field's own group (if it declares one), de-duplicates, sorts, and
//! reports cycles (a field depending on itself, or on a chain that loops back).

use std::collections::BTreeSet;

use crate::definition::Definition;
use crate::error::{MetaError, MetaErrorKind};

/// A field reference in a DocType: its DocType and field name.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct FieldRef {
    /// The DocType name.
    pub doctype: String,
    /// The field name.
    pub field: String,
}

impl FieldRef {
    /// A field reference from a DocType and field name.
    pub fn new(doctype: impl Into<String>, field: impl Into<String>) -> Self {
        Self {
            doctype: doctype.into(),
            field: field.into(),
        }
    }
}

/// A derived field and its inputs (ADR-070 section 12, rule 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derivation {
    /// The field that is derived.
    pub target: FieldRef,
    /// The fields whose values it combines or reads.
    pub inputs: Vec<FieldRef>,
}

impl Derivation {
    /// A derivation of a target from its inputs.
    pub fn new(target: FieldRef, inputs: Vec<FieldRef>) -> Self {
        Self { target, inputs }
    }
}

/// A source of derivation information. WP-MET-02 implements it for expressions
/// (`fetch_from`, `depends_on`, defaults); until then a test source or an explicit list does.
pub trait DerivationSource {
    /// The derivations this source knows about.
    fn derivations(&self) -> Vec<Derivation>;
}

impl DerivationSource for Vec<Derivation> {
    fn derivations(&self) -> Vec<Derivation> {
        self.clone()
    }
}

/// The computed inheritance of protection groups for every derived field.
pub type InheritanceMap = std::collections::BTreeMap<FieldRef, Vec<String>>;

fn own_group<'a>(definitions: &'a [Definition], r: &FieldRef) -> Option<&'a str> {
    definitions
        .iter()
        .filter(|d| d.name == r.doctype)
        .flat_map(|d| d.fields.iter())
        .find(|f| f.name == r.field)
        .and_then(|f| f.protection_group.as_deref())
}

/// Computes inherited protection groups for every derived field: the groups of all its inputs,
/// transitively (a derived field of a derived field includes both), without its own group,
/// de-duplicated and sorted. The result does not depend on the order of `derivations`.
///
/// # Errors
///
/// `meta.derivation_cycle`, naming the path, if a field depends on itself, directly or not.
pub fn compute_inheritance(
    definitions: &[Definition],
    derivations: &[Derivation],
) -> Result<InheritanceMap, MetaError> {
    let by_target: std::collections::BTreeMap<&FieldRef, BTreeSet<&FieldRef>> = {
        let mut m: std::collections::BTreeMap<&FieldRef, BTreeSet<&FieldRef>> =
            std::collections::BTreeMap::new();
        for d in derivations {
            m.entry(&d.target).or_default().extend(d.inputs.iter());
        }
        m
    };

    let mut map = InheritanceMap::new();
    for target in by_target.keys() {
        let mut groups = BTreeSet::new();
        let mut path = vec![(*target).clone()];
        collect(definitions, &by_target, target, &mut path, &mut groups)?;
        if let Some(own) = own_group(definitions, target) {
            groups.remove(own);
        }
        if !groups.is_empty() {
            map.insert(
                (*target).clone(),
                groups.into_iter().map(str::to_owned).collect(),
            );
        }
    }
    Ok(map)
}

fn collect<'a>(
    definitions: &'a [Definition],
    by_target: &std::collections::BTreeMap<&FieldRef, BTreeSet<&FieldRef>>,
    current: &FieldRef,
    path: &mut Vec<FieldRef>,
    groups: &mut BTreeSet<&'a str>,
) -> Result<(), MetaError> {
    let Some(inputs) = by_target.get(current) else {
        return Ok(());
    };
    for input in inputs {
        if path.contains(input) {
            let mut shown: Vec<String> = path
                .iter()
                .map(|r| format!("{}.{}", r.doctype, r.field))
                .collect();
            shown.push(format!("{}.{}", input.doctype, input.field));
            return Err(MetaError::new(
                MetaErrorKind::BadProperty,
                format!(
                    "derivation cycle: {} (meta.derivation_cycle)",
                    shown.join(" -> ")
                ),
            )
            .in_doctype(&path[0].doctype)
            .in_field(&path[0].field));
        }
        if let Some(g) = own_group(definitions, input) {
            groups.insert(g);
        }
        path.push((*input).clone());
        collect(definitions, by_target, input, path, groups)?;
        path.pop();
    }
    Ok(())
}

/// The `meta.derived_unprotected` lint: a field derived from a protected input must itself be in
/// one of the input groups or be declassified with a reason. One error per offending field,
/// naming the input, its group and the two fixes.
pub fn derived_unprotected(
    definitions: &[Definition],
    derivations: &[Derivation],
) -> Vec<MetaError> {
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    for d in derivations {
        let Some(field) = definitions
            .iter()
            .filter(|def| def.name == d.target.doctype)
            .flat_map(|def| def.fields.iter())
            .find(|f| f.name == d.target.field)
        else {
            continue;
        };
        if field.declassified.is_some() {
            continue;
        }
        // The groups of the direct inputs (their own group, or the groups they inherit are
        // covered by the same rule on the input itself).
        let mut input_groups = BTreeSet::new();
        let mut first: Option<(&FieldRef, &str)> = None;
        for input in &d.inputs {
            if let Some(g) = own_group(definitions, input) {
                input_groups.insert(g);
                first.get_or_insert((input, g));
            }
        }
        let Some((input, group)) = first else {
            continue;
        };
        let ok = field
            .protection_group
            .as_deref()
            .is_some_and(|g| input_groups.contains(g));
        if !ok && seen.insert(d.target.clone()) {
            errors.push(
                MetaError::new(
                    MetaErrorKind::BadProperty,
                    format!(
                        "derived from `{}.{}`, which is in protection group `{group}`: set \
                         protection_group to `{group}` (or another group of its inputs), or set \
                         declassified with a reason (meta.derived_unprotected)",
                        input.doctype, input.field
                    ),
                )
                .in_doctype(&d.target.doctype)
                .in_field(&d.target.field),
            );
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::Definition;

    #[test]
    fn test_empty_derivations() {
        assert!(
            compute_inheritance(&[], &[])
                .map(|r| r.is_empty())
                .unwrap_or(false)
        );
    }

    #[test]
    fn test_no_groups() {
        let derivation = Derivation::new(
            FieldRef::new("order", "total"),
            vec![
                FieldRef::new("order", "qty"),
                FieldRef::new("order", "price"),
            ],
        );
        let def = Definition::new("order");
        assert!(
            compute_inheritance(&[def], &[derivation])
                .map(|r| r.is_empty())
                .unwrap_or(false)
        );
    }

    #[test]
    fn test_cycle_detection() {
        let derivations = vec![
            Derivation::new(FieldRef::new("t", "a"), vec![FieldRef::new("t", "b")]),
            Derivation::new(FieldRef::new("t", "b"), vec![FieldRef::new("t", "a")]),
        ];
        let def = Definition::new("t");
        let result = compute_inheritance(&[def], &derivations);
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(e.kind, MetaErrorKind::BadProperty);
        }
    }
}
