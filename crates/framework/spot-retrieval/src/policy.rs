//! The index policy: which fields of which DocTypes are indexed, and how their long values are
//! chunked.
//!
//! The policy is TOML, loaded against the live permission [`Snapshot`] so a field the rules
//! protect can never be indexed by accident:
//!
//! - a DocType the policy does not name is **not indexed** (default deny);
//! - a field the policy names must exist in the metadata;
//! - a field that is **restricted** — it belongs to a protection group, carries a
//!   classification tag, has a mask, has a field grant, or its name is one the platform treats
//!   as secret-bearing (`spot_redact::DEFAULT_FIELD_NAMES`) — is refused with
//!   `retrieval.policy_field_restricted` unless the policy lists it under `restricted_ok`. An
//!   allowed restricted field is indexed, but every chunk it produces is tagged with the field
//!   name, and the retriever drops that chunk for any principal whose field view does not allow
//!   the field.
//!
//! Unknown keys are an error, never ignored.
//!
//! ```toml
//! [defaults]
//! max_chunk_chars = 800
//! overlap_chars = 80
//!
//! [doctypes.sales_invoice]
//! fields = ["title", "notes"]
//! restricted_ok = ["internal_note"]
//!
//! [doctypes.sales_invoice.chunking]
//! max_chunk_chars = 400
//! overlap_chars = 40
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use spot_permissions::{FieldTarget, Snapshot};

use crate::chunk::MIN_CHUNK_CHARS;
use crate::error::RetrievalError;

/// The default chunk window when a policy sets none.
pub const DEFAULT_MAX_CHUNK_CHARS: usize = 800;
/// The default overlap when a policy sets none.
pub const DEFAULT_OVERLAP_CHARS: usize = 80;

fn default_max_chunk_chars() -> usize {
    DEFAULT_MAX_CHUNK_CHARS
}

fn default_overlap_chars() -> usize {
    DEFAULT_OVERLAP_CHARS
}

/// How one field's value is split into chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunking {
    /// The longest chunk, in characters.
    #[serde(default = "default_max_chunk_chars")]
    pub max_chunk_chars: usize,
    /// How many characters consecutive chunks share, fewer than `max_chunk_chars`.
    #[serde(default = "default_overlap_chars")]
    pub overlap_chars: usize,
}

impl Default for Chunking {
    fn default() -> Self {
        Self {
            max_chunk_chars: DEFAULT_MAX_CHUNK_CHARS,
            overlap_chars: DEFAULT_OVERLAP_CHARS,
        }
    }
}

impl Chunking {
    /// Whether the window can make progress: a positive limit and an overlap below it.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.max_chunk_chars >= MIN_CHUNK_CHARS && self.overlap_chars < self.max_chunk_chars
    }
}

/// One field the policy indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldPlan {
    /// The field name.
    pub name: String,
    /// How its value is chunked.
    pub chunking: Chunking,
    /// Whether the permission rules mark it restricted (it was allowed under `restricted_ok`).
    pub restricted: bool,
}

/// One DocType's plan: the fields to index and the chunking they default to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocTypePolicy {
    /// The DocType.
    pub doctype: String,
    /// The fields to index, in policy order.
    pub fields: Vec<FieldPlan>,
    /// The DocType's default chunk window, applied to a field that names none.
    pub chunking: Chunking,
    /// The restricted fields the policy deliberately allows, in policy order.
    pub restricted_ok: Vec<String>,
}

impl DocTypePolicy {
    /// The plan for `field`, when the policy indexes it.
    #[must_use]
    pub fn field(&self, field: &str) -> Option<&FieldPlan> {
        self.fields.iter().find(|f| f.name == field)
    }
}

/// The whole policy: the DocTypes that are indexed.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct IndexPolicy {
    doctypes: BTreeMap<String, DocTypePolicy>,
}

impl IndexPolicy {
    /// Parses and validates the policy against `snapshot`.
    ///
    /// # Errors
    ///
    /// [`RetrievalError`] with `retrieval.policy_parse` (bad TOML or unknown key),
    /// `retrieval.policy_unknown_doctype`, `retrieval.policy_unknown_field`,
    /// `retrieval.policy_field_restricted` or `retrieval.policy_invalid`.
    pub fn load(text: &str, snapshot: &Snapshot) -> Result<IndexPolicy, RetrievalError> {
        let file: PolicyFile = toml::from_str(text).map_err(|e| RetrievalError::PolicyParse {
            detail: e.to_string(),
        })?;
        check_chunking(&file.defaults, "defaults")?;
        let mut doctypes = BTreeMap::new();
        for (name, raw) in file.doctypes {
            let doctype = name.trim();
            if doctype.is_empty() {
                return Err(RetrievalError::PolicyInvalid {
                    detail: "a DocType table has an empty name".to_owned(),
                });
            }
            let policy = load_doctype(doctype, &raw, file.defaults, snapshot)?;
            doctypes.insert(doctype.to_owned(), policy);
        }
        Ok(IndexPolicy { doctypes })
    }

    /// The plan for `doctype`, or `None` when the policy does not index it (default deny).
    #[must_use]
    pub fn doctype(&self, doctype: &str) -> Option<&DocTypePolicy> {
        self.doctypes.get(doctype)
    }

    /// Every DocType the policy indexes, in name order.
    pub fn doctypes(&self) -> impl Iterator<Item = &DocTypePolicy> {
        self.doctypes.values()
    }
}

fn load_doctype(
    doctype: &str,
    raw: &DocTypeFile,
    defaults: Chunking,
    snapshot: &Snapshot,
) -> Result<DocTypePolicy, RetrievalError> {
    let facts = snapshot
        .facts(doctype)
        .ok_or_else(|| RetrievalError::PolicyUnknownDoctype {
            doctype: doctype.to_owned(),
        })?;
    let chunking = raw.chunking.unwrap_or(defaults);
    check_chunking(&chunking, doctype)?;

    let allowed: BTreeSet<&str> = raw.restricted_ok.iter().map(String::as_str).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut fields = Vec::new();
    for name in &raw.fields {
        let field = name.trim();
        if !seen.insert(field) {
            return Err(RetrievalError::PolicyInvalid {
                detail: format!("`{doctype}` indexes `{field}` twice"),
            });
        }
        let facts = facts
            .field(field)
            .ok_or_else(|| RetrievalError::PolicyUnknownField {
                doctype: doctype.to_owned(),
                field: field.to_owned(),
            })?;
        let restricted = is_restricted(snapshot, doctype, field, facts.groups.len());
        if restricted && !allowed.contains(field) {
            return Err(RetrievalError::PolicyFieldRestricted {
                doctype: doctype.to_owned(),
                field: field.to_owned(),
            });
        }
        fields.push(FieldPlan {
            name: field.to_owned(),
            chunking,
            restricted,
        });
    }
    for name in &raw.restricted_ok {
        if !seen.contains(name.trim()) {
            return Err(RetrievalError::PolicyInvalid {
                detail: format!(
                    "`{doctype}` lists `{}` under restricted_ok but does not index it",
                    name.trim()
                ),
            });
        }
    }
    Ok(DocTypePolicy {
        doctype: doctype.to_owned(),
        fields,
        chunking,
        restricted_ok: raw
            .restricted_ok
            .iter()
            .map(|s| s.trim().to_owned())
            .collect(),
    })
}

fn check_chunking(chunking: &Chunking, at: &str) -> Result<(), RetrievalError> {
    if chunking.is_valid() {
        Ok(())
    } else {
        Err(RetrievalError::PolicyInvalid {
            detail: format!(
                "`{at}` has max_chunk_chars {} and overlap_chars {}: the overlap must be smaller",
                chunking.max_chunk_chars, chunking.overlap_chars
            ),
        })
    }
}

/// Whether the permission rules protect `field`: it is in a protection group, carries a
/// classification tag or a mask, has a field grant, or its name is one the platform treats as
/// secret-bearing. `groups` is the field's group count (a field in a group is closed by
/// default).
fn is_restricted(snapshot: &Snapshot, doctype: &str, field: &str, groups: usize) -> bool {
    if groups > 0 {
        return true;
    }
    let Some(facts) = snapshot.facts(doctype) else {
        return true;
    };
    let Some(field_facts) = facts.field(field) else {
        return true;
    };
    if !field_facts.tags.is_empty() || field_facts.mask.is_some() {
        return true;
    }
    if spot_redact::DEFAULT_FIELD_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(field))
    {
        return true;
    }
    snapshot.field_grants().iter().any(|grant| {
        grant.doctype == doctype
            && match &grant.target {
                FieldTarget::Field(path) => path == field,
                FieldTarget::Group(name) => field_facts.groups.contains(name),
            }
    })
}

/// The raw policy file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    defaults: Chunking,
    #[serde(default)]
    doctypes: BTreeMap<String, DocTypeFile>,
}

/// One DocType's raw policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocTypeFile {
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    restricted_ok: Vec<String>,
    #[serde(default)]
    chunking: Option<Chunking>,
}
