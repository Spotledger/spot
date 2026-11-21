//! Where the guard learns what is protected: the [`ProtectionSource`] trait, a plain-TOML
//! implementation for tests and fixtures, and (feature `meta`) the adapter over the effective
//! metadata of WP-MET-01.T6.
//!
//! The guard holds no copy: it asks the source on every call, so a metadata swap is seen by
//! the very next call.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_permissions::{FieldFacts, GroupName};

/// The protection facts of one DocType.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct DocProtection {
    /// The DocType's name.
    pub name: String,
    /// The fields' protection facts; a field not listed is unprotected.
    #[serde(default)]
    pub fields: Vec<FieldFacts>,
    /// Table field name to child DocType name, so a child-table column is judged on its own
    /// DocType's facts.
    #[serde(default)]
    pub tables: BTreeMap<String, String>,
    /// Groups that ask for a field access trace, as declared on the DocType. The permission
    /// snapshot's own `trace` flag counts too: a group traces if either says so.
    #[serde(default)]
    pub trace_groups: Vec<GroupName>,
}

impl DocProtection {
    /// The facts of a field by name; an unlisted field is unprotected.
    #[must_use]
    pub fn field(&self, name: &str) -> FieldFacts {
        self.fields
            .iter()
            .find(|f| f.path == name)
            .cloned()
            .unwrap_or_else(|| FieldFacts::plain(name))
    }

    /// The child DocType a table field holds rows of.
    #[must_use]
    pub fn child_doctype(&self, table: &str) -> Option<&str> {
        self.tables.get(table).map(String::as_str)
    }

    /// The names of the fields that have protection facts.
    #[must_use]
    pub fn protected_names(&self) -> BTreeSet<&str> {
        self.fields.iter().map(|f| f.path.as_str()).collect()
    }
}

/// Returns the protection facts of a DocType. Implementations hold no decision, only facts.
pub trait ProtectionSource: Send + Sync {
    /// The facts of `doctype`, or `None` when the source does not know it. The guard refuses
    /// to release anything of an unknown DocType.
    fn protection(&self, doctype: &str) -> Option<Arc<DocProtection>>;
}

/// A fixed set of DocType facts, loaded from TOML or built in code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StaticProtection {
    doctypes: BTreeMap<String, Arc<DocProtection>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticFile {
    #[serde(default)]
    doctype: Vec<DocProtection>,
}

impl StaticProtection {
    /// From facts built in code.
    #[must_use]
    pub fn new(doctypes: Vec<DocProtection>) -> Self {
        StaticProtection {
            doctypes: doctypes
                .into_iter()
                .map(|d| (d.name.clone(), Arc::new(d)))
                .collect(),
        }
    }

    /// Loads `[[doctype]]` tables:
    ///
    /// ```toml
    /// [[doctype]]
    /// name = "sales_invoice"
    /// trace_groups = ["hr"]
    /// tables = { items = "sales_invoice_item" }
    /// [[doctype.fields]]
    /// path = "card_number"
    /// groups = ["pci"]
    /// mask = { last_chars = 4 }
    /// ```
    ///
    /// # Errors
    ///
    /// The TOML parse error text.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let file: StaticFile = toml::from_str(text).map_err(|e| e.to_string())?;
        Ok(Self::new(file.doctype))
    }
}

impl ProtectionSource for StaticProtection {
    fn protection(&self, doctype: &str) -> Option<Arc<DocProtection>> {
        self.doctypes.get(doctype).cloned()
    }
}

#[cfg(feature = "meta")]
pub use meta_adapter::MetaProtection;

#[cfg(feature = "meta")]
mod meta_adapter {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use spot_app_api::meta::FieldKind;
    use spot_meta::EffectiveMeta;
    use spot_permissions::{FieldFacts, GroupName, MaskFormat, Tag};

    use super::{DocProtection, ProtectionSource};

    /// The facts as the effective metadata computes them (`EffectiveMeta::field_protection`:
    /// own group plus inherited groups, masks, classification, trace groups), read once for
    /// one metadata generation. A new generation is a new adapter; the facts are metadata,
    /// not decisions, so holding them is not a decision cache.
    #[derive(Clone, Debug, Default)]
    pub struct MetaProtection {
        doctypes: BTreeMap<String, Arc<DocProtection>>,
    }

    /// `placeholder` or `last_chars(n)`, the text `field_protection` writes.
    fn parse_mask(text: &str) -> MaskFormat {
        text.strip_prefix("last_chars(")
            .and_then(|rest| rest.strip_suffix(')'))
            .and_then(|n| n.parse::<u8>().ok())
            .map_or(MaskFormat::Placeholder, MaskFormat::LastChars)
    }

    impl MetaProtection {
        /// The facts of every DocType in `meta`.
        #[must_use]
        pub fn new(meta: &EffectiveMeta) -> Self {
            let all = meta.field_protection();
            let mut doctypes = BTreeMap::new();
            for view in meta.doctypes() {
                let name = view.name.as_str();
                let prefix = format!("{name}.");
                let fields = all
                    .iter()
                    .filter_map(|p| {
                        let field = p.path.strip_prefix(&prefix)?;
                        Some(FieldFacts {
                            path: field.to_owned(),
                            groups: p.groups.iter().cloned().map(GroupName).collect(),
                            tags: p.tags.iter().cloned().map(Tag).collect(),
                            mask: p.mask.as_deref().map(parse_mask),
                        })
                    })
                    .collect();
                let tables = view
                    .fields
                    .iter()
                    .filter_map(|f| match &f.kind {
                        FieldKind::Table { doctype }
                        | FieldKind::TableMultiselect { doctype, .. } => {
                            Some((f.name.as_str().to_owned(), doctype.as_str().to_owned()))
                        }
                        _ => None,
                    })
                    .collect();
                let trace_groups = meta
                    .protection_groups(name)
                    .into_iter()
                    .filter(|(_, _, trace)| *trace)
                    .map(|(group, _, _)| GroupName(group))
                    .collect();
                doctypes.insert(
                    name.to_owned(),
                    Arc::new(DocProtection {
                        name: name.to_owned(),
                        fields,
                        tables,
                        trace_groups,
                    }),
                );
            }
            MetaProtection { doctypes }
        }
    }

    impl ProtectionSource for MetaProtection {
        fn protection(&self, doctype: &str) -> Option<Arc<DocProtection>> {
            self.doctypes.get(doctype).cloned()
        }
    }
}
