//! The BI policy: which entities are exposed and how. Nothing is exposed by default.
//!
//! The policy is TOML. Structure and names are checked on load; the checks that need the catalogue
//! (does the entity exist, is the field protected, does the name fit) run in
//! [`generate`](crate::generate), so that every problem is reported in one pass.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use spot_storage_contract::Name;

use crate::error::{BiErrors, POLICY_INVALID, POLICY_PARSE};
use crate::sql::RESERVED_PREFIX;

/// The schema the base tables live in unless the policy says otherwise.
pub const DEFAULT_BASE_SCHEMA: &str = "public";

/// A loaded, structurally valid BI policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiPolicy {
    base_schema: Name,
    expose: BTreeSet<Name>,
    entities: BTreeMap<Name, EntityPolicy>,
}

/// The policy of one entity. Every setting is optional; [`EntityPolicy::default`] applies when
/// the policy has no table for the entity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityPolicy {
    /// The view version (`v1`, `v2`, ...). Defaults to 1.
    pub version: Option<u32>,
    /// A display name for `bi.catalogue`. Defaults to the entity name.
    pub display: Option<String>,
    /// The only fields to show. Unset means every field that is not protected.
    pub include: Option<BTreeSet<Name>>,
    /// Fields to hide. Protected fields are hidden whether or not they are listed here.
    pub exclude: BTreeSet<Name>,
    /// Show only posted rows (`docstatus = 1`). Defaults to true for documents.
    pub posted_only: Option<bool>,
    /// Expose the child tables as sibling views. Defaults to false.
    pub children: Option<bool>,
    /// The field whose value is shown next to a link to this entity (as `<field>_name`).
    pub title_field: Option<Name>,
    /// Column descriptions, by field name.
    pub labels: BTreeMap<Name, String>,
}

impl EntityPolicy {
    /// The effective version.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version.unwrap_or(1)
    }

    /// The effective posted-only setting.
    #[must_use]
    pub fn posted_only(&self) -> bool {
        self.posted_only.unwrap_or(true)
    }

    /// The effective children setting.
    #[must_use]
    pub fn children(&self) -> bool {
        self.children.unwrap_or(false)
    }

    /// The keys a child table may not set, because they belong to its parent.
    pub(crate) fn parent_only_keys_set(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if self.version.is_some() {
            keys.push("version");
        }
        if self.display.is_some() {
            keys.push("display");
        }
        if self.posted_only.is_some() {
            keys.push("posted_only");
        }
        if self.children.is_some() {
            keys.push("children");
        }
        if self.title_field.is_some() {
            keys.push("title_field");
        }
        keys
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    base_schema: Option<String>,
    #[serde(default)]
    expose: Vec<String>,
    #[serde(default)]
    entity: BTreeMap<String, EntityFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityFile {
    version: Option<u32>,
    display: Option<String>,
    include: Option<Vec<String>>,
    #[serde(default)]
    exclude: Vec<String>,
    posted_only: Option<bool>,
    children: Option<bool>,
    title_field: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

impl BiPolicy {
    /// Loads and checks a policy. Every problem found is reported, not only the first.
    ///
    /// # Errors
    ///
    /// `bi.policy_parse` for text that is not a policy, `bi.policy_invalid` for values that are
    /// out of range or badly named.
    pub fn from_toml(text: &str) -> Result<BiPolicy, BiErrors> {
        let file: PolicyFile = toml::from_str(text).map_err(|e| {
            let mut errors = BiErrors::default();
            errors.push(POLICY_PARSE, e.to_string());
            errors
        })?;
        let mut errors = BiErrors::default();
        let base_schema = match file.base_schema.as_deref() {
            None => name_or_default(DEFAULT_BASE_SCHEMA),
            Some(text) => match Name::new(text) {
                Ok(name) if name.as_str() != crate::sql::BI_SCHEMA => Some(name),
                _ => {
                    errors.push(
                        POLICY_INVALID,
                        format!("base_schema `{text}` is not a usable schema name"),
                    );
                    None
                }
            },
        };
        let mut expose = BTreeSet::new();
        for text in &file.expose {
            match Name::new(text) {
                Ok(name) => {
                    if !expose.insert(name) {
                        errors.push(
                            POLICY_INVALID,
                            format!("`{text}` is listed in expose twice"),
                        );
                    }
                }
                Err(_) => errors.push(
                    POLICY_INVALID,
                    format!("`{text}` in expose is not a valid storage name"),
                ),
            }
        }
        let mut entities = BTreeMap::new();
        for (key, raw) in file.entity {
            let Ok(name) = Name::new(&key) else {
                errors.push(
                    POLICY_INVALID,
                    format!("[entity.{key}] is not a valid storage name"),
                );
                continue;
            };
            if let Some(entity) = check_entity(&key, raw, &mut errors) {
                entities.insert(name, entity);
            }
        }
        match base_schema {
            Some(base_schema) if errors.is_empty() => Ok(BiPolicy {
                base_schema,
                expose,
                entities,
            }),
            _ => Err(errors),
        }
    }

    /// The schema the base tables live in.
    #[must_use]
    pub fn base_schema(&self) -> &str {
        self.base_schema.as_str()
    }

    /// The exposed entities, in name order.
    pub fn exposed(&self) -> impl Iterator<Item = &Name> {
        self.expose.iter()
    }

    /// The policy table of `entity`, if the policy has one.
    #[must_use]
    pub fn entity(&self, entity: &Name) -> Option<&EntityPolicy> {
        self.entities.get(entity)
    }

    /// Every entity that has a policy table, in name order.
    pub fn configured(&self) -> impl Iterator<Item = (&Name, &EntityPolicy)> {
        self.entities.iter()
    }
}

fn name_or_default(text: &str) -> Option<Name> {
    Name::new(text).ok()
}

fn check_entity(key: &str, raw: EntityFile, errors: &mut BiErrors) -> Option<EntityPolicy> {
    let before = errors.errors().len();
    if let Some(0) = raw.version {
        errors.push(
            POLICY_INVALID,
            format!("[entity.{key}] version must be 1 or more"),
        );
    }
    let names = |list: &[String], what: &str, errors: &mut BiErrors| -> BTreeSet<Name> {
        let mut out = BTreeSet::new();
        for text in list {
            match Name::new(text) {
                Ok(name) => {
                    out.insert(name);
                }
                Err(_) => errors.push(
                    POLICY_INVALID,
                    format!("[entity.{key}] {what} `{text}` is not a valid storage name"),
                ),
            }
        }
        out
    };
    let include = raw
        .include
        .as_ref()
        .map(|list| names(list, "include", errors));
    let exclude = names(&raw.exclude, "exclude", errors);
    let title_field = match raw.title_field.as_deref() {
        None => None,
        Some(text) => match Name::new(text) {
            Ok(name) => Some(name),
            Err(_) => {
                errors.push(
                    POLICY_INVALID,
                    format!("[entity.{key}] title_field `{text}` is not a valid storage name"),
                );
                None
            }
        },
    };
    let mut labels = BTreeMap::new();
    for (field, text) in &raw.labels {
        let Ok(name) = Name::new(field) else {
            errors.push(
                POLICY_INVALID,
                format!("[entity.{key}] label for `{field}` is not a valid storage name"),
            );
            continue;
        };
        if text.contains('\0') {
            errors.push(
                POLICY_INVALID,
                format!("[entity.{key}] label for `{field}` contains a NUL character"),
            );
        }
        labels.insert(name, text.clone());
    }
    if let Some(display) = &raw.display
        && display.contains('\0')
    {
        errors.push(
            POLICY_INVALID,
            format!("[entity.{key}] display contains a NUL character"),
        );
    }
    if key.starts_with(RESERVED_PREFIX) {
        errors.push(
            POLICY_INVALID,
            format!("[entity.{key}] uses the reserved `{RESERVED_PREFIX}` namespace"),
        );
    }
    if errors.errors().len() != before {
        return None;
    }
    Some(EntityPolicy {
        version: raw.version,
        display: raw.display,
        include,
        exclude,
        posted_only: raw.posted_only,
        children: raw.children,
        title_field,
        labels,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_exposed_by_default() {
        let policy = BiPolicy::from_toml("").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(policy.exposed().count(), 0);
        assert_eq!(policy.base_schema(), DEFAULT_BASE_SCHEMA);
    }

    #[test]
    fn unknown_keys_are_a_parse_error() {
        let errors = BiPolicy::from_toml("expose = [\"a\"]\nsecret_mode = true\n").unwrap_err();
        assert_eq!(errors.codes(), vec![POLICY_PARSE]);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let text = r#"
            expose = ["ok", "ok", "Bad Name"]
            base_schema = "bi"
            [entity.party]
            version = 0
            exclude = ["Not_Valid"]
            [entity.spot_meta]
            [entity.sales]
            labels = { total = "a\u0000b" }
        "#;
        let errors = BiPolicy::from_toml(text).unwrap_err();
        let codes = errors.codes();
        assert!(codes.iter().all(|c| *c == POLICY_INVALID), "{errors}");
        // base_schema, the duplicate, the bad expose name, version, the bad exclude name,
        // the reserved entity, and the NUL label.
        assert!(codes.len() >= 7, "{errors}");
    }
}
