//! Declaring personal fields. The metadata catalogue has no `personal` flag today, so the data
//! source is a configuration list (`[services.journal_privacy] fields = [...]`); the shape is
//! the one a catalogue flag would carry, so swapping the source later changes nothing else.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use spot_journal_pii::{FieldSpec, PersonalFields};

use crate::error::PrivacyError;

fn yes() -> bool {
    true
}

/// One personal field of one entity.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDecl {
    /// The entity (DocType) name.
    pub entity: String,
    /// The field path inside the image; `*` walks an array (`phones.*.number`).
    pub path: String,
    /// The path of the field that identifies the data subject (a record-level value). Required:
    /// a personal field with no subject cannot be sealed.
    #[serde(default)]
    pub subject: Option<String>,
    /// Why the data is held.
    pub purpose: String,
    /// The retention class (what may be erased is policy data on the class).
    pub retention_class: String,
    /// `false` for a legal-retention field: sealed under the tenant journal key, untouched by
    /// erasing a subject.
    #[serde(default = "yes")]
    pub erasable: bool,
    /// The field is also a secret (sealed by the secrets layer already).
    #[serde(default)]
    pub secret: bool,
}

/// What a declaration became: whether erasing a subject reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    /// The entity.
    pub entity: String,
    /// The field path.
    pub path: String,
    /// Whether erasure reaches the field (declared erasable and its class does not forbid it).
    pub erasable: bool,
}

/// The built descriptions, per entity, and the summary of every field.
pub struct Built {
    /// One description per entity.
    pub entities: Vec<PersonalFields>,
    /// One entry per declared field.
    pub declared: Vec<Declared>,
}

/// Builds the descriptions from `decls`. A field whose retention class is in `forbidden` is
/// treated as `erasable = false`.
///
/// # Errors
/// [`PrivacyError::SubjectMissing`] for a declaration without a subject (the metadata is refused),
/// [`PrivacyError::DeclarationInvalid`] for one that does not parse.
pub fn build(decls: &[FieldDecl], forbidden: &BTreeSet<String>) -> Result<Built, PrivacyError> {
    let mut by_entity: BTreeMap<&str, PersonalFields> = BTreeMap::new();
    let mut declared = Vec::with_capacity(decls.len());
    for decl in decls {
        let invalid = |reason: &'static str| PrivacyError::DeclarationInvalid {
            entity: decl.entity.clone(),
            path: decl.path.clone(),
            reason,
        };
        let subject = match decl.subject.as_deref() {
            Some(subject) if !subject.trim().is_empty() => subject,
            _ => {
                return Err(PrivacyError::SubjectMissing {
                    entity: decl.entity.clone(),
                    path: decl.path.clone(),
                });
            }
        };
        let mut spec = FieldSpec::new(&decl.path, subject, &decl.purpose, &decl.retention_class)
            .map_err(|_| {
                invalid("the path, the subject path, the purpose or the class is unusable")
            })?;
        let erasable = decl.erasable && !forbidden.contains(&decl.retention_class);
        if !erasable {
            spec = spec.with_erasable(false);
        }
        if decl.secret {
            spec = spec.with_secret(true);
        }
        let entry = match by_entity.remove(decl.entity.as_str()) {
            Some(existing) => existing,
            None => PersonalFields::new(&decl.entity)
                .map_err(|_| invalid("the entity name is empty"))?,
        };
        by_entity.insert(&decl.entity, entry.with_field(spec));
        declared.push(Declared {
            entity: decl.entity.clone(),
            path: decl.path.clone(),
            erasable,
        });
    }
    Ok(Built {
        entities: by_entity.into_values().collect(),
        declared,
    })
}
