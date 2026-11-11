//! The compatibility checker: enforces the evolution rules of ADR-056.
//!
//! [`check`] compares a registered schema with a proposed one for the same topic.
//!
//! **Within one major version, the only allowed changes are:**
//! - new optional fields;
//! - deprecation marks;
//! - new values of an **open** enum;
//! - a higher revision.
//!
//! **Across a major version bump,** fields may change freely, but every field name that
//! disappears must be listed as retired, retired names stay retired, and none is reused.

use std::collections::BTreeMap;

use crate::schema::{EventSchema, FieldSchema, PayloadType};

/// One breaking change, with the field path it concerns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Breaking {
    /// The schemas are for different topics.
    TopicChanged,
    /// The new major version is lower than the old one.
    MajorDecreased,
    /// Same major version but the revision did not increase although the schema changed.
    RevisionNotIncreased,
    /// A field was removed within a major version.
    FieldRemoved(String),
    /// A field's type changed.
    TypeChanged(String),
    /// A field changed between required and optional.
    RequiredChanged(String),
    /// A new field is required. New fields must be optional within a major version, because
    /// events already stored do not have them.
    NewRequiredField(String),
    /// An enum lost a value.
    EnumValueRemoved(String, String),
    /// A closed enum gained a value, or an enum changed between open and closed.
    EnumClosedChanged(String),
    /// A field name disappeared in a new major version without being listed as retired.
    RemovedNotRetired(String),
    /// A previously retired name is no longer listed as retired.
    RetiredDropped(String),
    /// A field uses a retired name.
    RetiredReused(String),
}

/// Checks that `new` may replace `old` in the registry. Returns every breaking change found.
pub fn check(old: &EventSchema, new: &EventSchema) -> Result<(), Vec<Breaking>> {
    let mut out = Vec::new();
    if old.topic != new.topic {
        out.push(Breaking::TopicChanged);
        return Err(out);
    }
    for name in &old.retired {
        if !new.retired.contains(name) {
            out.push(Breaking::RetiredDropped(name.clone()));
        }
    }
    for name in new.fields.keys() {
        if new.retired.contains(name) || old.retired.contains(name) {
            out.push(Breaking::RetiredReused(name.clone()));
        }
    }
    if new.major < old.major {
        out.push(Breaking::MajorDecreased);
    } else if new.major > old.major {
        for name in old.fields.keys() {
            if !new.fields.contains_key(name) && !new.retired.contains(name) {
                out.push(Breaking::RemovedNotRetired(name.clone()));
            }
        }
    } else {
        if old != new && new.revision <= old.revision {
            out.push(Breaking::RevisionNotIncreased);
        }
        compare_fields(&old.fields, &new.fields, "", &mut out);
    }
    if out.is_empty() { Ok(()) } else { Err(out) }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn compare_fields(
    old: &BTreeMap<String, FieldSchema>,
    new: &BTreeMap<String, FieldSchema>,
    path: &str,
    out: &mut Vec<Breaking>,
) {
    for (name, o) in old {
        let p = join(path, name);
        match new.get(name) {
            None => out.push(Breaking::FieldRemoved(p)),
            Some(n) => {
                if o.required != n.required {
                    out.push(Breaking::RequiredChanged(p.clone()));
                }
                compare_types(&o.ty, &n.ty, &p, out);
            }
        }
    }
    for (name, n) in new {
        if !old.contains_key(name) && n.required {
            out.push(Breaking::NewRequiredField(join(path, name)));
        }
    }
}

fn compare_types(old: &PayloadType, new: &PayloadType, path: &str, out: &mut Vec<Breaking>) {
    match (old, new) {
        (
            PayloadType::Enum {
                values: ov,
                open: oo,
            },
            PayloadType::Enum {
                values: nv,
                open: no,
            },
        ) => {
            for v in ov.difference(nv) {
                out.push(Breaking::EnumValueRemoved(path.to_owned(), v.clone()));
            }
            let added = nv.difference(ov).next().is_some();
            if oo != no || (added && !oo) {
                out.push(Breaking::EnumClosedChanged(path.to_owned()));
            }
        }
        (PayloadType::List(o), PayloadType::List(n)) => {
            compare_types(o, n, &format!("{path}[]"), out)
        }
        (PayloadType::Object(o), PayloadType::Object(n)) => compare_fields(o, n, path, out),
        (o, n) if std::mem::discriminant(o) == std::mem::discriminant(n) => {}
        _ => out.push(Breaking::TypeChanged(path.to_owned())),
    }
}
