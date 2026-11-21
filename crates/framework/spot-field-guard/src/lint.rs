//! The derived-value lint (ADR-070 section 12, rule 5): a derived value carries the
//! **strictest** protection of its inputs. A field computed from a protected field must be in
//! every protection group of every input, or the derivation is a side door.
//!
//! `spot-meta` computes inheritance when it builds the facts; this lint is the independent
//! check over the facts a [`ProtectionSource`] actually serves, so a bug or a hand-edited
//! override that drops a group is caught before it ships.

use std::collections::BTreeSet;

use crate::source::ProtectionSource;

/// One derived field and the fields it is computed from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derivation {
    /// The DocType of the derived field.
    pub doctype: String,
    /// The derived field.
    pub field: String,
    /// The `(doctype, field)` inputs.
    pub inputs: Vec<(String, String)>,
}

/// One breach: a derived field missing a group one of its inputs is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breach {
    /// `doctype.field` of the derived field.
    pub derived: String,
    /// `doctype.field` of the input whose group is missing.
    pub input: String,
    /// The group the derived field lacks.
    pub missing_group: String,
}

/// Checks every derivation; returns every breach, in input order. An input or derived field
/// of an unknown DocType counts as a breach with an empty group name, so a lint over facts
/// that are missing cannot pass silently.
#[must_use]
pub fn check_derived(source: &dyn ProtectionSource, derivations: &[Derivation]) -> Vec<Breach> {
    let mut out = Vec::new();
    for d in derivations {
        let derived = format!("{}.{}", d.doctype, d.field);
        let Some(own) = source.protection(&d.doctype) else {
            out.push(Breach {
                derived,
                input: String::new(),
                missing_group: String::new(),
            });
            continue;
        };
        let have: BTreeSet<_> = own.field(&d.field).groups.into_iter().collect();
        for (input_doctype, input_field) in &d.inputs {
            let input = format!("{input_doctype}.{input_field}");
            let Some(prot) = source.protection(input_doctype) else {
                out.push(Breach {
                    derived: derived.clone(),
                    input,
                    missing_group: String::new(),
                });
                continue;
            };
            for group in prot.field(input_field).groups {
                if !have.contains(&group) {
                    out.push(Breach {
                        derived: derived.clone(),
                        input: input.clone(),
                        missing_group: group.0,
                    });
                }
            }
        }
    }
    out
}
