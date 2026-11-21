//! What metadata says about a field: its type, classification tags, protection group, mask
//! spec, link target and denormalised source.
//!
//! The sandbox does not keep a list of sensitive fields. It asks the effective metadata, and
//! the same facts the field guard (WP-SEC-04) protects are the ones this crate masks. Two
//! constructors cover the real inputs:
//!
//! - [`MetadataClasses::from_definitions`] reads compiled [`Definition`]s (the effective
//!   metadata of a tenant), including `fetch_from` copies.
//! - [`MetadataClasses::with_protection`] overlays the facts of a
//!   [`ProtectionSource`](spot_field_guard::ProtectionSource), which is the field guard's view:
//!   own group plus inherited groups, classification and the mask format.
//!
//! [`MetadataClasses::from_toml`] exists for fixtures and tests; it is the same fact set in a
//! compact form.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use spot_field_guard::ProtectionSource;
use spot_meta::{Definition, FieldType, MaskSpec};
use spot_permissions::MaskFormat;

use crate::error::{Error, Result};
use crate::plan::MaskingPolicy;

/// Everything the masker needs to know about one field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldClass {
    /// The DocType that holds the field.
    pub doctype: String,
    /// The field's storage name.
    pub field: String,
    /// The field's type; decimals and integers tagged `financial` are ledger amounts.
    pub ty: FieldType,
    /// Classification tags, lower-cased.
    pub tags: BTreeSet<String>,
    /// Protection groups the field belongs to (inherited groups included for a derived copy).
    pub groups: BTreeSet<String>,
    /// The mask the metadata asks for, honoured as written.
    pub mask: Option<MaskSpec>,
    /// For a link field, the DocType it points at.
    pub link_target: Option<String>,
    /// For a denormalised copy (`fetch_from`), the field it copies: `(doctype, field)`.
    pub derived_from: Option<(String, String)>,
}

impl FieldClass {
    /// A field with no protection facts.
    #[must_use]
    pub fn plain(doctype: &str, field: &str, ty: FieldType) -> Self {
        Self {
            doctype: doctype.to_owned(),
            field: field.to_owned(),
            ty,
            tags: BTreeSet::new(),
            groups: BTreeSet::new(),
            mask: None,
            link_target: None,
            derived_from: None,
        }
    }

    /// Whether the field has no classification at all: no tag, no protection group, no mask.
    #[must_use]
    pub fn is_unclassified(&self) -> bool {
        self.tags.is_empty() && self.groups.is_empty() && self.mask.is_none()
    }

    /// Whether the policy closes this field. Fail closed: the policy's explicit Keep decision
    /// is the only thing that opens an unclassified field. A classified field is closed when
    /// it carries a closing tag, sits in a protection group the policy covers (every group when
    /// the policy lists none), or has a mask spec.
    #[must_use]
    pub fn is_closed(&self, policy: &MaskingPolicy) -> bool {
        if policy.keeps(&self.doctype, &self.field) {
            return false;
        }
        if self.is_unclassified() || self.mask.is_some() {
            return true;
        }
        if self.tags.iter().any(|tag| policy.closes_tag(tag)) {
            return true;
        }
        if policy.groups.is_empty() {
            !self.groups.is_empty()
        } else {
            self.groups
                .iter()
                .any(|group| policy.groups.iter().any(|listed| listed == group))
        }
    }

    /// Whether the field is a link rather than a value: link fields hold record ids and are
    /// never masked, because the id is the reference and the identity lives on the target.
    #[must_use]
    pub fn is_link(&self) -> bool {
        matches!(
            self.ty,
            FieldType::Link
                | FieldType::DynamicLink
                | FieldType::Table
                | FieldType::TableMultiselect
        )
    }
}

/// A source of field facts. Implementations hold facts, not decisions.
pub trait ClassificationSource {
    /// The facts of `doctype.field`, or `None` when the metadata does not know the field.
    fn class(&self, doctype: &str, field: &str) -> Option<FieldClass>;

    /// Every DocType the source knows, in stable order.
    fn doctypes(&self) -> Vec<String>;

    /// Every field name the source knows for `doctype`, in stable order.
    fn fields(&self, doctype: &str) -> Vec<String>;
}

/// The effective metadata of one tenant, as classes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataClasses {
    classes: BTreeMap<(String, String), FieldClass>,
}

impl MetadataClasses {
    /// Reads every field of every definition, then resolves `fetch_from` copies so a copy
    /// inherits its source's tags, groups and mask.
    #[must_use]
    pub fn from_definitions(definitions: &[Definition]) -> Self {
        let mut classes: BTreeMap<(String, String), FieldClass> = BTreeMap::new();
        for definition in definitions {
            let mut links: BTreeMap<&str, Option<String>> = BTreeMap::new();
            for field in &definition.fields {
                if matches!(field.ty, FieldType::Link) {
                    links.insert(
                        field.name.as_str(),
                        field.options.as_deref().map(first_option),
                    );
                }
            }
            for field in &definition.fields {
                let derived_from = field.fetch_from.as_deref().and_then(|spec| {
                    let (link, target) = spec.split_once('.')?;
                    let doctype = links.get(link)?.clone()?;
                    Some((doctype, target.to_owned()))
                });
                let class = FieldClass {
                    doctype: definition.name.clone(),
                    field: field.name.clone(),
                    ty: field.ty,
                    tags: field
                        .classification
                        .iter()
                        .map(|tag| tag.to_ascii_lowercase())
                        .collect(),
                    groups: field.protection_group.iter().cloned().collect(),
                    mask: field.masking.clone(),
                    link_target: field.options.as_deref().map(first_option),
                    derived_from,
                };
                classes.insert((class.doctype.clone(), class.field.clone()), class);
            }
        }
        let mut this = Self { classes };
        this.resolve_derived();
        this
    }

    /// Overlays the field guard's facts: a group, tag or mask the guard knows is unioned with
    /// what the definition said. The guard is the authority on protection, so its facts are
    /// added, never removed.
    #[must_use]
    pub fn with_protection(mut self, source: &dyn ProtectionSource) -> Self {
        let keys: Vec<(String, String)> = self.classes.keys().cloned().collect();
        for key in keys {
            let Some(protection) = source.protection(&key.0) else {
                continue;
            };
            let facts = protection.field(&key.1);
            if let Some(class) = self.classes.get_mut(&key) {
                class
                    .tags
                    .extend(facts.tags.iter().map(|tag| tag.0.to_ascii_lowercase()));
                class
                    .groups
                    .extend(facts.groups.iter().map(|group| group.0.clone()));
                if class.mask.is_none() {
                    class.mask = facts.mask.map(|mask| match mask {
                        MaskFormat::Placeholder => MaskSpec::placeholder(),
                        MaskFormat::LastChars(n) => MaskSpec::last_chars(n),
                    });
                }
            }
        }
        self.resolve_derived();
        self
    }

    /// Builds classes directly, for a fixture or a test.
    #[must_use]
    pub fn from_classes(classes: Vec<FieldClass>) -> Self {
        let mut this = Self {
            classes: classes
                .into_iter()
                .map(|class| ((class.doctype.clone(), class.field.clone()), class))
                .collect(),
        };
        this.resolve_derived();
        this
    }

    /// Loads classes from a compact TOML fixture:
    ///
    /// ```toml
    /// [[doctype]]
    /// name = "customer"
    /// [[doctype.field]]
    /// name = "email"
    /// type = "text"
    /// tags = ["pii"]
    /// group = "personal"
    /// ```
    ///
    /// # Errors
    /// [`Error::Policy`] with the parse text when the fixture is malformed.
    pub fn from_toml(text: &str) -> Result<Self> {
        let file: FixtureFile = toml::from_str(text).map_err(|error| Error::Policy {
            reason: error.to_string(),
        })?;
        let mut classes = Vec::new();
        for doctype in file.doctype {
            for field in doctype.field {
                let ty = parse_type(&field.ty).ok_or_else(|| Error::Policy {
                    reason: format!("unknown field type in the fixture for {}", doctype.name),
                })?;
                classes.push(FieldClass {
                    doctype: doctype.name.clone(),
                    field: field.name,
                    ty,
                    tags: field
                        .tags
                        .into_iter()
                        .map(|t| t.to_ascii_lowercase())
                        .collect(),
                    groups: field.group.into_iter().collect(),
                    mask: match field.mask.as_deref() {
                        Some("placeholder") => Some(MaskSpec::placeholder()),
                        Some("last_chars") => field.last_chars.map(MaskSpec::last_chars),
                        _ => None,
                    },
                    link_target: field.link,
                    derived_from: field.derived_from,
                });
            }
        }
        Ok(Self::from_classes(classes))
    }

    /// The class of `doctype.field`.
    #[must_use]
    pub fn get(&self, doctype: &str, field: &str) -> Option<&FieldClass> {
        self.classes.get(&(doctype.to_owned(), field.to_owned()))
    }

    /// The number of known fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.classes.len()
    }

    /// Whether no field is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Union the source's tags, groups and mask into every `fetch_from` copy. A table repeats
    /// as often as its chain is deep, so the loop stops as soon as a pass changes nothing.
    fn resolve_derived(&mut self) {
        // Five passes cover a realistic copy chain; a cycle cannot loop forever because the
        // pass must change something to continue.
        for _ in 0..5 {
            let mut changed = false;
            let snapshot = self.classes.clone();
            for class in self.classes.values_mut() {
                let Some((doctype, field)) = class.derived_from.clone() else {
                    continue;
                };
                let Some(source) = snapshot.get(&(doctype, field)) else {
                    continue;
                };
                let tags_before = class.tags.len();
                let groups_before = class.groups.len();
                class.tags.extend(source.tags.iter().cloned());
                class.groups.extend(source.groups.iter().cloned());
                if class.mask.is_none() {
                    class.mask = source.mask.clone();
                }
                if class.tags.len() != tags_before || class.groups.len() != groups_before {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
}

impl ClassificationSource for MetadataClasses {
    fn class(&self, doctype: &str, field: &str) -> Option<FieldClass> {
        self.get(doctype, field).cloned()
    }

    fn doctypes(&self) -> Vec<String> {
        let doctypes: BTreeSet<String> = self
            .classes
            .keys()
            .map(|(doctype, _)| doctype.clone())
            .collect();
        doctypes.into_iter().collect()
    }

    fn fields(&self, doctype: &str) -> Vec<String> {
        self.classes
            .keys()
            .filter(|(owner, _)| owner == doctype)
            .map(|(_, field)| field.clone())
            .collect()
    }
}

fn first_option(options: &str) -> String {
    options
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn parse_type(text: &str) -> Option<FieldType> {
    Some(match text {
        "bool" => FieldType::Bool,
        "int" => FieldType::Int,
        "decimal" => FieldType::Decimal,
        "text" => FieldType::Text,
        "long_text" => FieldType::LongText,
        "select" => FieldType::Select,
        "date" => FieldType::Date,
        "time" => FieldType::Time,
        "datetime" => FieldType::DateTime,
        "link" => FieldType::Link,
        "dynamic_link" => FieldType::DynamicLink,
        "json" => FieldType::Json,
        "bytes" => FieldType::Bytes,
        "table" => FieldType::Table,
        "attach" => FieldType::Attach,
        "secret" => FieldType::Secret,
        "table_multiselect" => FieldType::TableMultiselect,
        _ => return None,
    })
}

#[derive(Deserialize)]
struct FixtureFile {
    #[serde(default)]
    doctype: Vec<FixtureDoctype>,
}

#[derive(Deserialize)]
struct FixtureDoctype {
    name: String,
    #[serde(default)]
    field: Vec<FixtureField>,
}

#[derive(Deserialize)]
struct FixtureField {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    mask: Option<String>,
    #[serde(default)]
    last_chars: Option<u8>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    derived_from: Option<(String, String)>,
}
