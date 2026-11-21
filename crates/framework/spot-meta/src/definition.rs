//! The definition format (v0, internal until the SDK freezes it): one TOML file per DocType.
//!
//! The lookup section (ADR-068 §3, WP-MET-01.T5) is parsed here and kept in the
//! [`FieldSpec`]; validation lives in `validate.rs` and the compiled form in `compile.rs`.
//! Nothing in this crate evaluates a lookup or calls a seam.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{MetaError, MetaErrorKind};

/// The platform's default lookup seam (ADR-068 §3.1): it searches the target DocType. The
/// contract's `lookup.rs` owns the value; the same text is defined here because the platform
/// default is part of the definition format's meaning and this crate does not depend on the
/// app API's lookup module for anything it parses.
pub const PLATFORM_LINK_SEARCH: &str = "platform:link_search";

/// The control hints a definition may name (ADR-068 amendment A2), in the app API 0.4.0
/// `ControlHint`'s order and spelling. A hint never changes what is stored; a client that does
/// not offer the control falls back to its own default.
pub const CONTROL_HINTS: [&str; 8] = [
    "switch",
    "checkbox",
    "dropdown",
    "radio",
    "segmented",
    "autocomplete",
    "chips",
    "textarea",
];

/// Checks a control-hint name against [`CONTROL_HINTS`]. The error names every valid value, so
/// a definition or an override that spells one wrong can be fixed without reading a contract.
pub(crate) fn check_control_name(name: &str) -> Result<(), String> {
    if CONTROL_HINTS.contains(&name) {
        Ok(())
    } else {
        Err(format!(
            "unknown control `{name}`; use one of: {}",
            CONTROL_HINTS.join(", ")
        ))
    }
}

/// Reads a field's `control`, refusing a name [`CONTROL_HINTS`] does not list. The refusal is a
/// load error (as serde reports it), so an unknown name never reaches the effective metadata.
fn deserialize_control<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(name) = &value {
        check_control_name(name).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

/// The type of a field in a definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldType {
    /// True or false.
    #[serde(rename = "bool")]
    Bool,
    /// 64-bit integer.
    #[serde(rename = "int")]
    Int,
    /// Exact decimal; needs `scale`.
    #[serde(rename = "decimal")]
    Decimal,
    /// Short text, optionally limited by `max_chars`.
    #[serde(rename = "text")]
    Text,
    /// Text without a limit.
    #[serde(rename = "long_text")]
    LongText,
    /// Text whose allowed values are listed in `options`, one per line.
    #[serde(rename = "select")]
    Select,
    /// Calendar date.
    #[serde(rename = "date")]
    Date,
    /// Time of day.
    #[serde(rename = "time")]
    Time,
    /// Instant.
    #[serde(rename = "datetime")]
    DateTime,
    /// Link to a document of the DocType in `options`.
    #[serde(rename = "link")]
    Link,
    /// Link whose DocType is named per record by the text field in `options`.
    #[serde(rename = "dynamic_link")]
    DynamicLink,
    /// JSON document.
    #[serde(rename = "json")]
    Json,
    /// Raw bytes.
    #[serde(rename = "bytes")]
    Bytes,
    /// A WGS-84 position (ADR-087 section 4).
    #[serde(rename = "geo")]
    Geo,
    /// A half-open range of values (ADR-087 section 5).
    #[serde(rename = "range")]
    Range,
    /// Child rows of the DocType in `options`.
    #[serde(rename = "table")]
    Table,
    /// Reference to a file record (the DocType `file`), never the bytes (ADR-068 §2).
    #[serde(rename = "attach")]
    Attach,
    /// A password or key held by the secrets service; it has no column (ADR-068 §2).
    #[serde(rename = "secret")]
    Secret,
    /// A child table whose rows hold one link field, a multi-pick of a master.
    #[serde(rename = "table_multiselect")]
    TableMultiselect,
}

impl FieldType {
    /// Whether values are text in storage.
    pub fn is_text_like(self) -> bool {
        matches!(self, Self::Text | Self::LongText | Self::Select)
    }

    /// Whether the field holds child rows (`table` or `table_multiselect`), so it has no column.
    pub fn is_table(self) -> bool {
        matches!(self, Self::Table | Self::TableMultiselect)
    }
}

/// The `lookup` section of a field (ADR-068 §3.2): which operation supplies the field's
/// candidate values, as configuration.
///
/// The definition-format twin of the contract's `LookupSpec`
/// (`contracts/app-api/src/lookup.rs`, WP-CON-02.T2). `Compile` turns the resolved one into
/// the contract type for the app view. This crate never evaluates a lookup; the rules are
/// only parsed here and checked in `validate.rs`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupSpec {
    /// The seam to call, `app:name`; absent means the platform default
    /// (`platform:link_search`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seam: Option<String>,
    /// Fixed parameters passed to every call. Values are strings, parsed by the seam.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// Conditional choices, evaluated in order, first match wins — once the shared expression
    /// language exists (WP-UI-11). Until then they are parsed and kept, never evaluated, and
    /// a definition that has one is refused (`validate.rs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when: Vec<LookupRule>,
}

/// One conditional choice of a lookup (ADR-068 §3.2): the seam to use while `if` holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupRule {
    /// The condition, written in the shared expression language (WP-UI-11); kept, not parsed.
    #[serde(rename = "if")]
    pub if_expr: String,
    /// The seam to call when the condition holds.
    pub seam: String,
    /// Parameters added to the spec's own for this choice.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

impl LookupSpec {
    /// A lookup with `seam` set and nothing else.
    pub fn seam(seam: impl Into<String>) -> Self {
        Self {
            seam: Some(seam.into()),
            params: BTreeMap::new(),
            when: Vec::new(),
        }
    }
}

/// The `lookup` property of a field, which has two forms.
///
/// On the types that resolve their candidates from somewhere — `link`, `dynamic_link`,
/// `select`, `table_multiselect` and `text` — it is the seam table ([`LookupSpec`]). On a
/// `secret` field it is the flag `lookup = true`: the value must be **found by value**, so the
/// field compiles to one more column holding a blind index (ADR-068 §2, WP-SEC-03.T2). A
/// `secret` always holds a sealed value; the flag only adds the index, and it is refused on
/// every other type, as is the seam table on a secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldLookup {
    /// `lookup = true` — a `secret` field that is looked up by its blind index.
    Index(bool),
    /// `[fields.lookup]` — the seam that supplies the field's candidate values.
    Spec(LookupSpec),
}

impl FieldLookup {
    /// The seam spec, when the property is the table form.
    pub fn spec(&self) -> Option<&LookupSpec> {
        match self {
            Self::Spec(spec) => Some(spec),
            Self::Index(_) => None,
        }
    }

    /// Whether this is the flag form (`lookup = true` or `lookup = false`).
    pub fn is_flag(&self) -> bool {
        matches!(self, Self::Index(_))
    }

    /// Whether the flag is set. `lookup = false` means nothing at all.
    pub fn enabled(&self) -> bool {
        matches!(self, Self::Index(true))
    }
}

/// How a masked value is shown to a role holding `read_masked` but not `read` (ADR-070
/// section 12, rule 2). A mask never touches what is stored: the stored value is the real
/// one, and the format decides what a reader without `read` sees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaskSpec {
    /// A fixed placeholder, revealing nothing (the default if neither field is set).
    #[serde(default, skip_serializing_if = "is_false")]
    pub placeholder: bool,
    /// The value with only its last `n` characters shown, the rest masked (1–8 characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_chars: Option<u8>,
}

impl MaskSpec {
    /// A placeholder mask (the default).
    pub fn placeholder() -> Self {
        Self {
            placeholder: true,
            last_chars: None,
        }
    }

    /// A mask showing only the last `n` characters.
    pub fn last_chars(n: u8) -> Self {
        Self {
            placeholder: false,
            last_chars: Some(n),
        }
    }

    /// Whether the mask is well-formed: at most one field set, and `last_chars` is 1–8.
    pub fn well_formed(&self) -> bool {
        match (&self.placeholder, &self.last_chars) {
            (true, None) => true,
            (false, Some(n)) => (1..=8).contains(n),
            _ => false,
        }
    }
}

impl Default for MaskSpec {
    fn default() -> Self {
        Self::placeholder()
    }
}

/// One field, as written in a definition or a custom field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    /// Storage name.
    pub name: String,
    /// Human label; defaults to the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The field's type.
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// Link target, child DocType, select values (one per line) or the type field of a dynamic
    /// link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
    /// Must be set when saved (enforced by the engine).
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    /// Not editable by users.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    /// May change on a posted document.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_on_posted: bool,
    /// Values must be unique (a unique index).
    #[serde(default, skip_serializing_if = "is_false")]
    pub unique: bool,
    /// Add an index.
    #[serde(default, skip_serializing_if = "is_false")]
    pub index: bool,
    /// A `select` that holds several of its values (ADR-068 amendment A2): the stored value is
    /// an array of them. Valid on `select` only and refused with `unique`. It changes the
    /// shape of the stored value, so it is definition-only: a site may not override it
    /// (`descriptor.rs`).
    ///
    /// A `default` on such a field is a list of the field's options, one value per line (the
    /// same convention `options` uses). A list of one value still ends with a line break, so
    /// it is not mistaken for a scalar default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub multiple: bool,
    /// Default value as a string, parsed by type. Text starting with `=` is an expression in
    /// the shared language (`=doc.qty * 2`): it is kept as text and never parsed here, and
    /// `spot-plan` compiles it. A literal default that itself starts with `=` is written `==`
    /// (`==x` is the literal `=x`). A default without the prefix is parsed by type as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Expression (shared language) that decides whether the field is shown. Kept as text,
    /// shape-checked only; `spot-plan` parses it (ADR-068 §1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<String>,
    /// Expression that makes the field required while it is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mandatory_depends_on: Option<String>,
    /// Expression that makes the field read-only while it is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_depends_on: Option<String>,
    /// `<link_field>.<target_field>`: the value is copied from the document the `link_field`
    /// of this DocType points at. Only the shape and the link field are checked here; the
    /// target field is checked by `spot-plan` once both DocTypes are known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_from: Option<String>,
    /// Digits after the point (`decimal` only; required there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u8>,
    /// Total digits (`decimal` only); 18 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<u8>,
    /// Longest text (`text` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<u32>,
    /// How the value is interpreted (ADR-068 §2): one of the formats of the base type. Absent
    /// means the type's default. A format never changes the storage type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The language marker of a `code` format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax: Option<String>,
    /// The symbology of a `barcode` format (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbology: Option<String>,
    /// The highest value of a `rating` format (1 to 10); 5 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u8>,
    /// The `link` field of the same DocType that holds the currency of a `currency` format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency_field: Option<String>,
    /// Longer help text (a presentation hint, ADR-068 §1.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The UI hides it; hiding is not security.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// Shown in the list view.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_list_view: bool,
    /// Offered in the standard filter bar.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_standard_filter: bool,
    /// Not copied when a document is duplicated or corrected.
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_copy: bool,
    /// The control the author prefers for this field (ADR-068 amendment A2): one of
    /// [`CONTROL_HINTS`], spelled as the app API does. A hint only: a client uses it when it
    /// offers that control and falls back otherwise, and it never changes what is stored.
    /// A control that cannot apply to the field's kind is kept and reported as a warning in the
    /// compile report, not refused. An unknown name is a load error that lists every valid one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_control"
    )]
    pub control: Option<String>,
    /// Permission level for field-level permissions; 0 when absent.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub permlevel: u8,
    /// How the field gets its candidate values (ADR-068 §3); absent means the platform
    /// default. Valid on `link`, `dynamic_link`, `select`, `table_multiselect` and `text`
    /// (`validate.rs` refuses it anywhere else). On a `secret` field the flag form
    /// `lookup = true` is valid instead: it compiles the field's blind-index column
    /// (WP-SEC-03.T2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookup: Option<FieldLookup>,
    /// The protection group this field belongs to (ADR-068 amendment A1). A field in a group
    /// is **closed by default**: no grant means hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protection_group: Option<String>,
    /// Classification tags such as `pii`, `financial`, `secret`, `health` (ADR-070 section 12,
    /// rule 10). They grant nothing; they set default protection and drive review and export
    /// rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classification: Vec<String>,
    /// How a masked value is shown (ADR-070 section 12, rule 2): `placeholder` (default) or
    /// `last_chars = n` with n from 1 to 8. Required on any field whose group lets a role
    /// hold `read_masked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masking: Option<MaskSpec>,
    /// The reviewed reason this field is deliberately declassified, removing its inherited
    /// protection (ADR-070 section 12, rule 5). A definition that sets this on an app field
    /// is refused by a site override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declassified: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u8) -> bool {
    *n == 0
}

impl FieldSpec {
    /// A field with the given name and type and every other property unset.
    pub fn new(name: &str, ty: FieldType) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            ty,
            options: None,
            required: false,
            read_only: false,
            allow_on_posted: false,
            unique: false,
            index: false,
            multiple: false,
            default: None,
            depends_on: None,
            mandatory_depends_on: None,
            read_only_depends_on: None,
            fetch_from: None,
            scale: None,
            precision: None,
            max_chars: None,
            format: None,
            syntax: None,
            symbology: None,
            max: None,
            currency_field: None,
            description: None,
            hidden: false,
            in_list_view: false,
            in_standard_filter: false,
            no_copy: false,
            control: None,
            permlevel: 0,
            lookup: None,
            protection_group: None,
            classification: Vec::new(),
            masking: None,
            declassified: None,
        }
    }

    /// The label, or the name when there is none.
    pub fn label_or_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// The values of a select field's `options`: one per non-blank line, trimmed.
    pub fn select_values(&self) -> Vec<String> {
        split_options(self.options.as_deref().unwrap_or(""))
    }

    /// The default's expression text (without the leading `=`) when the default is an
    /// expression, else `None`. `==` starts a literal, not an expression.
    pub fn default_expression(&self) -> Option<&str> {
        let text = self.default.as_deref()?;
        text.strip_prefix('=').filter(|rest| !rest.starts_with('='))
    }

    /// Whether the default is an expression (starts with a single `=`).
    pub fn default_is_expression(&self) -> bool {
        self.default_expression().is_some()
    }

    /// The default as a literal, with the `==` escape resolved, or `None` when there is no
    /// default or it is an expression.
    pub fn default_literal(&self) -> Option<&str> {
        let text = self.default.as_deref()?;
        match text.strip_prefix('=') {
            None => Some(text),
            Some(rest) if rest.starts_with('=') => Some(rest),
            Some(_) => None,
        }
    }

    /// The spec's seam, or the platform default (ADR-068 §3.1) when it names none.
    pub fn lookup_seam(&self) -> &str {
        match self.lookup_spec() {
            Some(s) => match &s.seam {
                Some(seam) => seam,
                None => PLATFORM_LINK_SEARCH,
            },
            None => PLATFORM_LINK_SEARCH,
        }
    }

    /// The seam table of the `lookup` property, when the field has one.
    pub fn lookup_spec(&self) -> Option<&LookupSpec> {
        self.lookup.as_ref().and_then(FieldLookup::spec)
    }

    /// Whether the field declares `lookup = true`: a `secret` that is found by its blind index.
    pub fn lookup_index(&self) -> bool {
        self.lookup.as_ref().is_some_and(FieldLookup::enabled)
    }
}

pub(crate) fn split_options(options: &str) -> Vec<String> {
    options
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A protection group declared by a DocType (ADR-068 amendment A1, ADR-070 section 12).
/// A field in a group is **closed by default**: no grant means hidden.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectionGroupSpec {
    /// The group's storage-style name, unique per DocType.
    pub name: String,
    /// Human label, for example "Compensation".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Every `read` and `reveal` of the group's fields writes a field access trace (ADR-070
    /// section 12, rule 9).
    #[serde(default, skip_serializing_if = "is_false")]
    pub trace: bool,
    /// The declared sync-class default; the tenant decides the real class (ADR-070 section 12,
    /// rule 13a). Absent means `everywhere`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_class_hint: Option<String>,
}

impl ProtectionGroupSpec {
    /// A group with the given name and label; `trace` off, hint absent (defaults to everywhere).
    pub fn new(name: &str, label: &str) -> Self {
        Self {
            name: name.to_owned(),
            label: Some(label.to_owned()),
            trace: false,
            sync_class_hint: None,
        }
    }

    /// The label, or the name when there is none.
    pub fn label_or_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

/// A multi-field index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexSpec {
    /// Indexed fields in order.
    pub fields: Vec<String>,
    /// Whether keys must be unique.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unique: bool,
}

/// A declared overlap exclusion (ADR-087 section 5.1): no two records may hold overlapping
/// `field` ranges within the same `scope`. The operator is always overlap, so it is not written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusionSpec {
    /// The `range` field of this DocType.
    pub field: String,
    /// Fields that scope the exclusion by equality (for example the property and the unit).
    /// Each must be a required scalar field: a record with a null scope never conflicts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
    /// Plain-language explanation shown when a write conflicts.
    pub message: String,
}

/// One searchable field of a [`SearchSpec`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchFieldSpec {
    /// A `text` or `long_text` field of this DocType.
    pub name: String,
    /// The relevance weight: `A`, `B`, `C` or `D`, nothing else. Kept as written and checked in
    /// `validate.rs`, so a wrong one is reported with its code instead of as a parse error.
    pub weight: String,
}

/// The `[search]` declaration of a DocType (ADR-087 section 7.2). Absent means no projection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchSpec {
    /// The default language of the content; absent is the tenant collation's language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The searchable fields with their weights.
    #[serde(default)]
    pub fields: Vec<SearchFieldSpec>,
    /// Fields the caller may count by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<String>,
    /// Typo tolerance (trigram similarity).
    #[serde(default, skip_serializing_if = "is_false")]
    pub typo_tolerance: bool,
}

/// One DocType as written in `doctypes/<name>.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    /// Storage name.
    pub name: String,
    /// Human label; defaults to the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The app that owns the DocType.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// The module within the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Draft / post / cancel (ADR-42).
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_posted: bool,
    /// Rows of a parent's table only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_child: bool,
    /// The field holding the human-readable name or number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_field: Option<String>,
    /// How documents are named: a series pattern such as `INV-.YYYY.-.#####`, `field:<name>`,
    /// `hash` or `prompt`. Kept as text; `spot-plan` validates the pattern. No site override:
    /// renaming a series after documents exist is a data migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub naming: Option<String>,
    /// Fields in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldSpec>,
    /// Multi-field indexes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<IndexSpec>,
    /// Protection groups the DocType declares (ADR-068 amendment A1, ADR-070 section 12).
    /// A field in a group is **closed by default**: no grant means hidden. Declaring grants
    /// nothing; it only lets fields name a group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protection_groups: Vec<ProtectionGroupSpec>,
    /// Overlap exclusions over range fields (ADR-087 section 5.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusions: Vec<ExclusionSpec>,
    /// What the DocType opts into for search (ADR-087 section 7.2); absent means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchSpec>,
}

impl Definition {
    /// A definition with no fields, for building one in code.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            app: None,
            module: None,
            is_posted: false,
            is_child: false,
            title_field: None,
            naming: None,
            fields: vec![],
            indexes: vec![],
            protection_groups: vec![],
            exclusions: vec![],
            search: None,
        }
    }

    /// The label, or the name when there is none.
    pub fn label_or_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// Parses one definition. `source` names the file in errors. An unknown key is an error that
    /// names the file and the key.
    pub fn parse(source: &str, text: &str) -> Result<Self, MetaError> {
        toml::from_str(text).map_err(|e| {
            MetaError::new(MetaErrorKind::Parse, e.to_string().trim_end().to_owned())
                .in_file(source)
        })
    }

    /// Writes the definition as TOML.
    pub fn to_toml(&self) -> Result<String, MetaError> {
        toml::to_string(self).map_err(|e| {
            MetaError::new(MetaErrorKind::Parse, format!("cannot write TOML: {e}"))
                .in_doctype(&self.name)
        })
    }
}

/// Reads every `*.toml` in `dir`, sorted by file name. A file `<name>.toml` must define the
/// DocType `<name>`. Reports every file that fails, not just the first.
pub fn load_dir(dir: &Path) -> Result<Vec<Definition>, Vec<MetaError>> {
    let io = |what: &str, e: std::io::Error| {
        vec![MetaError::new(
            MetaErrorKind::Io,
            format!("cannot {what} {}: {e}", dir.display()),
        )]
    };
    let entries = fs::read_dir(dir).map_err(|e| io("read directory", e))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| io("list", e))?.path();
        if path.extension().is_some_and(|x| x == "toml") && path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();

    let (mut defs, mut errors) = (Vec::new(), Vec::new());
    for path in paths {
        let file = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                errors.push(
                    MetaError::new(MetaErrorKind::Io, format!("cannot read file: {e}"))
                        .in_file(&file),
                );
                continue;
            }
        };
        match Definition::parse(&file, &text) {
            Ok(def) => {
                let stem = path
                    .file_stem()
                    .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                if def.name != stem {
                    errors.push(
                        MetaError::new(
                            MetaErrorKind::FileName,
                            format!("file must be named `{}.toml`", def.name),
                        )
                        .in_file(&file)
                        .in_doctype(&def.name),
                    );
                } else {
                    defs.push(def);
                }
            }
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(defs)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod geo_range_tests {
    use super::*;

    #[test]
    fn geo_and_range_parse_as_field_types() {
        let text = "name = \"booking\"\n\n[[fields]]\nname = \"place\"\ntype = \"geo\"\n\n[[fields]]\nname = \"validity\"\ntype = \"range\"\n";
        let def = Definition::parse("booking.toml", text).unwrap_or_else(|e| panic!("{e}"));
        let types: Vec<FieldType> = def.fields.iter().map(|f| f.ty).collect();
        assert_eq!(types, [FieldType::Geo, FieldType::Range]);
    }
}
