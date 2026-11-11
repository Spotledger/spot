//! The contract's metadata model as Rust types: `DocTypeMeta`, `MetaField`, `FieldType`.
//! Contract 0.2.0 (WP-CON-03, ADR-068) completes the field description with `precision`,
//! `link_type_field`, `select` values, `max_chars`, `default`, `format`/`format_params`,
//! `lookup` and `hints`, so the meta endpoint describes every field fully; the additional
//! properties are optional and are omitted when the field carries none.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_storage_contract::Name;

use super::lookup::LookupSpec;

/// The value encoding of a field, from the Values table (the contract's `FieldType`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldType {
    /// Boolean.
    Bool,
    /// 64-bit signed integer.
    Int,
    /// Exact decimal string with this scale (digits after the point).
    Decimal {
        /// Digits after the point.
        scale: u8,
    },
    /// UTF-8 text.
    Text,
    /// `YYYY-MM-DD`.
    Date,
    /// `HH:MM:SS` with optional fractional seconds.
    Time,
    /// RFC 3339 instant (UTC in responses).
    DateTime,
    /// A link: a document id.
    Link,
    /// Embedded JSON.
    Json,
    /// Base64 bytes.
    Bytes,
    /// A child table.
    Table,
    /// Base64 bytes interpreted as attached-file text (the contract's `attach`).
    Attach,
}

impl FieldType {
    /// The contract's string form.
    pub fn as_str(&self) -> &'static str {
        match self {
            FieldType::Bool => "bool",
            FieldType::Int => "int",
            FieldType::Decimal { .. } => "decimal",
            FieldType::Text => "text",
            FieldType::Date => "date",
            FieldType::Time => "time",
            FieldType::DateTime => "datetime",
            FieldType::Link => "link",
            FieldType::Json => "json",
            FieldType::Bytes => "bytes",
            FieldType::Table => "table",
            FieldType::Attach => "attach",
        }
    }

    /// Parses the contract's string form.
    pub fn parse(s: &str) -> Option<FieldType> {
        Some(match s {
            "bool" => FieldType::Bool,
            "int" => FieldType::Int,
            "decimal" => FieldType::Decimal { scale: 0 },
            "text" => FieldType::Text,
            "date" => FieldType::Date,
            "time" => FieldType::Time,
            "datetime" => FieldType::DateTime,
            "link" => FieldType::Link,
            "json" => FieldType::Json,
            "bytes" => FieldType::Bytes,
            "table" => FieldType::Table,
            "attach" => FieldType::Attach,
            _ => return None,
        })
    }
}

/// One field in `DocTypeMeta.fields` (contract shape, 0.2.0 FieldMeta).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaField {
    /// Storage name.
    pub name: Name,
    /// Human label.
    pub label: String,
    /// Value type; drives the codec.
    pub field_type: FieldType,
    /// Must be non-null on save.
    pub required: bool,
    /// Cannot be changed in the document's state (on a posted DocType: not allowed after
    /// posting unless the field is `allow_on_posted`).
    pub read_only: bool,
    /// Decimal scale.
    pub scale: Option<u8>,
    /// Decimal total digits.
    pub precision: Option<u8>,
    /// Link target DocType.
    pub link_to: Option<Name>,
    /// For a dynamic link, the field holding the target DocType.
    pub link_type_field: Option<Name>,
    /// Child DocType, for tables.
    pub child_doctype: Option<Name>,
    /// For a `table_multiselect`, the row field holding the picked document.
    pub link_field: Option<Name>,
    /// The allowed values of a `select` field, in display order.
    pub select: Option<Vec<String>>,
    /// The limit of a `text` field, in characters.
    pub max_chars: Option<u32>,
    /// The value a new document starts with, in the value encoding; omitted when unset.
    pub default: Option<Json>,
    /// How the value is interpreted and shown (ADR-068 §2); absent means plain. The set
    /// grows additively, so a client treats an unknown format as plain.
    pub format: Option<String>,
    /// The format's parameters (`symbology`, `syntax`, `currency_field`, `max`).
    pub format_params: BTreeMap<String, Json>,
    /// The field's base lookup (ADR-068 §3.2), for information; a client never sends it
    /// back and the endpoint never accepts one.
    pub lookup: Option<LookupSpec>,
    /// Presentation hints (ADR-068 §1.3), never a security property; omitted when every
    /// hint is unset.
    pub hints: FieldHints,
    /// A `select` field that holds several of its values (contract 0.5.0); the value is
    /// then an array of the chosen texts. Omitted from the response when false.
    pub multiple: bool,
    /// Whether [`MetaField::hints`] differs from its zero value (drives the `hints`
    /// property's presence in the response).
    pub has_hints: bool,
}

impl MetaField {
    /// Whether the field's definition carries an **override** lookup — a spec naming a seam
    /// other than the platform default (ADR-068 §3.1). A field with no lookup, or with the
    /// platform default, needs no seam: the platform's link search answers it.
    pub fn has_lookup_override(&self) -> bool {
        self.lookup
            .as_ref()
            .and_then(|spec| spec.seam.as_deref())
            .is_some_and(|seam| seam != "platform:link_search")
    }
}

/// The presentation hints (ADR-068 §1.3). They never change what is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldHints {
    /// Longer offline help text.
    pub description: Option<String>,
    /// The UI hides it; hiding is not security.
    pub hidden: bool,
    /// Shown in the doc's list view.
    pub in_list_view: bool,
    /// Offered in the standard filter bar.
    pub in_standard_filter: bool,
    /// Not copied when a document is duplicated or corrected.
    pub no_copy: bool,
    /// Permission level for field-level permissions.
    pub permlevel: u8,
    /// Which control the author prefers (contract 0.5.0); a hint, never a restriction.
    pub control: Option<ControlHint>,
}

/// The closed set of control hints of contract 0.5.0 (`FieldHints.control`). The set grows
/// additively in the contract; a stored definition can only carry one of these values, so
/// this layer never produces an unknown one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlHint {
    /// A switch, for a bool.
    Switch,
    /// A checkbox, for a bool.
    Checkbox,
    /// A drop-down list, for a single or multiple select.
    Dropdown,
    /// Radio buttons, for a single select.
    Radio,
    /// A segmented control, for a single select.
    Segmented,
    /// An autocomplete box, for a link.
    Autocomplete,
    /// Chips, for a multiple select.
    Chips,
    /// A multi-line text box, for text.
    Textarea,
}

impl ControlHint {
    /// Every value, in the contract's order.
    pub const ALL: [ControlHint; 8] = [
        ControlHint::Switch,
        ControlHint::Checkbox,
        ControlHint::Dropdown,
        ControlHint::Radio,
        ControlHint::Segmented,
        ControlHint::Autocomplete,
        ControlHint::Chips,
        ControlHint::Textarea,
    ];

    /// The contract's string form.
    pub fn as_str(self) -> &'static str {
        match self {
            ControlHint::Switch => "switch",
            ControlHint::Checkbox => "checkbox",
            ControlHint::Dropdown => "dropdown",
            ControlHint::Radio => "radio",
            ControlHint::Segmented => "segmented",
            ControlHint::Autocomplete => "autocomplete",
            ControlHint::Chips => "chips",
            ControlHint::Textarea => "textarea",
        }
    }

    /// Parses the contract's string form; `None` for a value outside the closed set.
    pub fn parse(s: &str) -> Option<ControlHint> {
        ControlHint::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

impl FieldHints {
    /// Whether every hint is unset (drives the property's presence in the response).
    pub fn is_default(&self) -> bool {
        self.description.is_none()
            && !self.hidden
            && !self.in_list_view
            && !self.in_standard_filter
            && !self.no_copy
            && self.permlevel == 0
            && self.control.is_none()
    }

    /// A hint that only names the field in list views, the common positive case.
    pub fn in_list_view() -> Self {
        FieldHints {
            in_list_view: true,
            ..FieldHints::default()
        }
    }

    /// The contract's JSON form (`components.schemas.FieldHints`): properties are omitted
    /// when unset, so an all-default hint object is `{}` in a `hints` property.
    pub fn to_json(&self) -> Json {
        let mut map = serde_json::Map::new();
        if let Some(description) = &self.description {
            map.insert("description".to_owned(), Json::String(description.clone()));
        }
        if self.hidden {
            map.insert("hidden".to_owned(), Json::from(true));
        }
        if self.in_list_view {
            map.insert("in_list_view".to_owned(), Json::from(true));
        }
        if self.in_standard_filter {
            map.insert("in_standard_filter".to_owned(), Json::from(true));
        }
        if self.no_copy {
            map.insert("no_copy".to_owned(), Json::from(true));
        }
        if self.permlevel != 0 {
            map.insert(
                "permlevel".to_owned(),
                Json::from(u64::from(self.permlevel)),
            );
        }
        if let Some(control) = self.control {
            map.insert(
                "control".to_owned(),
                Json::String(control.as_str().to_owned()),
            );
        }
        Json::Object(map)
    }
}

/// A DocType's metadata as the native API v0.1 fixes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocTypeMeta {
    /// Storage name.
    pub doctype: Name,
    /// Human label.
    pub label: String,
    /// Metadata version; `ETag` for `meta_get` (ADR-05).
    pub version: u64,
    /// Documents have a draft stage and a Post action (ADR-42).
    pub is_posted: bool,
    /// Fields in display order.
    pub fields: Vec<MetaField>,
}

impl DocTypeMeta {
    /// A field by storage name.
    pub fn field(&self, name: &str) -> Option<&MetaField> {
        self.fields.iter().find(|f| f.name.as_str() == name)
    }

    /// The contract's JSON form of this metadata (`components.schemas.DocTypeMeta`).
    pub fn to_json(&self) -> Json {
        let mut map = serde_json::Map::new();
        map.insert(
            "doctype".to_owned(),
            Json::String(self.doctype.as_str().to_owned()),
        );
        map.insert("label".to_owned(), Json::String(self.label.clone()));
        map.insert("version".to_owned(), Json::from(self.version));
        map.insert("is_posted".to_owned(), Json::from(self.is_posted));
        let fields: Vec<Json> = self.fields.iter().map(meta_field_json).collect();
        map.insert("fields".to_owned(), Json::Array(fields));
        Json::Object(map)
    }
}

fn meta_field_json(f: &MetaField) -> Json {
    let mut map = serde_json::Map::new();
    map.insert("name".to_owned(), Json::String(f.name.as_str().to_owned()));
    map.insert("label".to_owned(), Json::String(f.label.clone()));
    map.insert(
        "type".to_owned(),
        Json::String(f.field_type.as_str().to_owned()),
    );
    map.insert("required".to_owned(), Json::from(f.required));
    map.insert("read_only".to_owned(), Json::from(f.read_only));
    if let Some(scale) = f.scale {
        map.insert("scale".to_owned(), Json::from(u64::from(scale)));
    }
    if let Some(precision) = f.precision {
        map.insert("precision".to_owned(), Json::from(u64::from(precision)));
    }
    if let Some(link_to) = &f.link_to {
        map.insert(
            "link_to".to_owned(),
            Json::String(link_to.as_str().to_owned()),
        );
    }
    if let Some(link_type_field) = &f.link_type_field {
        map.insert(
            "link_type_field".to_owned(),
            Json::String(link_type_field.as_str().to_owned()),
        );
    }
    if let Some(child) = &f.child_doctype {
        map.insert(
            "child_doctype".to_owned(),
            Json::String(child.as_str().to_owned()),
        );
    }
    if let Some(link_field) = &f.link_field {
        map.insert(
            "link_field".to_owned(),
            Json::String(link_field.as_str().to_owned()),
        );
    }
    if let Some(values) = &f.select {
        map.insert(
            "select".to_owned(),
            Json::Array(values.iter().cloned().map(Json::String).collect()),
        );
    }
    if f.multiple {
        map.insert("multiple".to_owned(), Json::from(true));
    }
    if let Some(max_chars) = f.max_chars {
        map.insert("max_chars".to_owned(), Json::from(u64::from(max_chars)));
    }
    if let Some(default) = &f.default
        && !default.is_null()
    {
        map.insert("default".to_owned(), default.clone());
    }
    if let Some(format) = &f.format {
        map.insert("format".to_owned(), Json::String(format.clone()));
    }
    if !f.format_params.is_empty() {
        let mut params = serde_json::Map::new();
        for (key, value) in &f.format_params {
            params.insert(key.clone(), value.clone());
        }
        map.insert("format_params".to_owned(), Json::Object(params));
    }
    if let Some(lookup) = &f.lookup {
        map.insert("lookup".to_owned(), lookup.to_json());
    }
    if f.has_hints {
        map.insert("hints".to_owned(), f.hints.to_json());
    }
    Json::Object(map)
}

/// A DocType's metadata, as the endpoint layer gets it from the metadata service (WP-MET-01).
/// The codec resolves child-table metas through it.
pub trait MetaResolver: Send + Sync + 'static {
    /// The metadata for a DocType name; unknown → `meta.unknown_doctype`.
    fn resolve(&self, doctype: &str) -> Result<DocTypeMeta, crate::api::service::ApiError>;
}
