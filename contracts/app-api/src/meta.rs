//! The read-only view of DocType metadata that apps and hosts need.
//!
//! The full metadata model (custom fields, property overrides, views, naming) is WP-MET-01.
//! This is the subset the app API fixes now: everything the document engine and the UI need to
//! type-check values and present a field (ADR-068 §6), enough to find child tables and know
//! which fields may change after posting. The metadata layer produces it; it is versioned
//! (ADR-05), so a change never reaches an operation already running.

use std::collections::BTreeMap;
use std::fmt;

use spot_storage_contract::{Name, Value};

use crate::error::{AppError, AppResult, ErrorClass, FieldPath};
use crate::lookup::LookupSpec;

/// How a `text` field is interpreted (ADR-068 §2). It never changes the storage type or what
/// a value may contain: the engine checks the format on save.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TextFormat {
    /// Free text.
    Plain,
    /// An e-mail address.
    Email,
    /// A telephone number.
    Phone,
    /// A URL.
    Url,
    /// A colour.
    Color,
    /// A barcode; the symbology (EAN-13, Code 128, …) when one is set.
    Barcode {
        /// The symbology to encode with.
        symbology: Option<String>,
    },
}

/// How a `long_text` field is interpreted (ADR-068 §2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LongTextFormat {
    /// Free text.
    Plain,
    /// Markdown.
    Markdown,
    /// HTML, sanitised on save.
    Html,
    /// Source code in the given language.
    Code {
        /// The language marker, for example `sql`.
        syntax: String,
    },
}

/// How an `int` field is interpreted (ADR-068 §2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IntFormat {
    /// A plain count or position.
    Plain,
    /// A duration in whole seconds.
    Duration,
    /// A rating; `max` (5 in the default control) tops it.
    Rating {
        /// The highest rating.
        max: u8,
    },
}

/// How a `decimal` field is interpreted (ADR-068 §2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecimalFormat {
    /// A plain number.
    Plain,
    /// Money in the currency of the named field (ADR-059).
    Currency {
        /// The link or text field holding the currency.
        currency_field: Name,
    },
    /// A percentage.
    Percent,
}

/// How a `json` field is interpreted (ADR-068 §2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JsonFormat {
    /// An unrestricted JSON document.
    Plain,
    /// GeoJSON.
    Geolocation,
}

/// How an `attach` field is interpreted (ADR-068 §2). The value is a reference to a file record
/// (WP-PLT-10), never the bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AttachFormat {
    /// Any file.
    File,
    /// An image.
    Image,
    /// A signature.
    Signature,
}

/// A protection group's name, validated like a storage [`Name`]: lowercase letters, digits and
/// underscores, starting with a letter (ADR-068 amendment A1). A group a DocType declares is a
/// security boundary in metadata (ADR-070 section 12); the grants themselves live in
/// `spot-permissions`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct GroupName(String);

impl GroupName {
    /// Validates and wraps a group name.
    pub fn new(s: &str) -> AppResult<Self> {
        if Name::new(s).is_ok() {
            Ok(Self(s.to_owned()))
        } else {
            Err(invalid_meta(
                format!(
                    "`{s}` is not a valid protection-group name: use lowercase letters, digits \
                     and underscores, starting with a letter"
                ),
                Some(FieldPath::field("name")),
            ))
        }
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GroupName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GroupName({:?})", self.0)
    }
}

impl fmt::Display for GroupName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A classification tag, validated like a storage [`Name`] (ADR-070 section 12, rule 10).
/// Tags such as `pii`, `financial`, `secret` and `health` grant nothing by themselves: they
/// drive default protection, access review and export rules, and the `spot-meta` tag lint.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct Tag(String);

impl Tag {
    /// Validates and wraps a tag.
    pub fn new(s: &str) -> AppResult<Self> {
        if Name::new(s).is_ok() {
            Ok(Self(s.to_owned()))
        } else {
            Err(invalid_meta(
                format!(
                    "`{s}` is not a valid classification tag: use lowercase letters, digits and \
                     underscores, starting with a letter"
                ),
                Some(FieldPath::field("name")),
            ))
        }
    }

    /// The tag as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tag({:?})", self.0)
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a masked value is shown to a role holding `read_masked` but not `read` (ADR-070
/// section 12, rule 2). A mask never touches what is stored: the stored value is the real one,
/// and the format decides what a reader without `read` sees. `LastChars` never counts as more
/// than the field may reveal, and every masked field must have one of these (the contract's
/// `meta.mask_missing`); the mask **application** is `spot-permissions`/WP-SEC-04, never here.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MaskFormat {
    /// A fixed placeholder (`▮▮▮`), revealing nothing.
    Placeholder,
    /// The value with only its last `n` characters shown, the rest masked.
    LastChars(u8),
}

impl MaskFormat {
    /// Whether the format is well-formed: a `LastChars(n)` count is 1–8 characters. The
    /// contract can check only the shape; whether the field may be masked at all is the
    /// definition layer's rule (`meta.mask_not_applicable`, WP-MET-01.T6).
    pub fn well_formed(&self) -> bool {
        match self {
            MaskFormat::Placeholder => true,
            MaskFormat::LastChars(n) => (1..=8).contains(n),
        }
    }
}

/// How a protection group's fields sync to branch nodes (ADR-070 section 12, rule 13a). The
/// declaration is only a **hint**: the tenant decides the class per group and node (rule 12.13),
/// and the sync layer enforces the tenant's choice, never this hint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SyncClassHint {
    /// The value syncs like any field; the node enforces the same grants offline. The platform
    /// default.
    #[default]
    Everywhere,
    /// Stored encrypted on the node; its key is released to the node's memory under a lease.
    Encrypted,
    /// The node can create values but can never read them back (sealed to a head-office key).
    Sealed,
    /// Does not sync to branch nodes; shows as unavailable there.
    HomeOnly,
}

/// One protection group a DocType declares (ADR-068 amendment A1, ADR-070 section 12). A field
/// in a group is **closed by default**: no grant means hidden. Declaring a group grants
/// nothing; the rights (`read_masked`, `read`, `reveal`, `create`, `update`, `report`, deny)
/// are `spot-permissions`' business.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProtectionGroupDef {
    /// The group's storage-style name, unique per DocType.
    pub name: GroupName,
    /// Human label, for example "Compensation".
    pub label: String,
    /// Every `read` and `reveal` of the group's fields writes a field access trace (rule 9).
    pub trace: bool,
    /// The declared sync-class default; the tenant decides the real class (rule 12.13).
    pub sync_class_hint: SyncClassHint,
}

impl ProtectionGroupDef {
    /// A group from its name and label; `trace` off, hint `Everywhere`.
    pub fn new(name: GroupName, label: String) -> Self {
        Self {
            name,
            label,
            trace: false,
            sync_class_hint: SyncClassHint::Everywhere,
        }
    }
}

/// The security properties of one field (ADR-068 amendment A1, the Security layer of §1).
/// Empty is the default: a field with none follows its DocType's ordinary rights.
///
/// **`inherited_groups` is computed by `spot-meta`, never authored.** A derived field's access
/// needs a grant in its own group and in every inherited group (the least access wins); a
/// definition that sets it by hand is refused (`meta.inherited_authored`). `declassified`
/// records a reviewed decision to drop a field's inherited groups; it needs a reason
/// (`meta.declassify_reason`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct FieldSecurity {
    /// The group the field belongs to; `None` is an unprotected field.
    pub protection_group: Option<GroupName>,
    /// Classification tags (`pii`, `financial`, `secret`, `health`, ...). They grant nothing.
    pub classification: Vec<Tag>,
    /// How a masked reader sees the value; required on any field whose group lets a role hold
    /// `read_masked` (`meta.mask_missing`).
    pub masking: Option<MaskFormat>,
    /// The reviewed reason a deliberately declassified field carries, when one does.
    pub declassified: Option<String>,
    /// Never authored here: computed by `spot-meta` from the field's inputs (setting it by
    /// hand is refused, `meta.inherited_authored`).
    pub inherited_groups: Vec<GroupName>,
}

impl FieldSecurity {
    /// Whether the security block carries nothing at all (the common, unprotected shape).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.protection_group.is_none()
            && self.classification.is_empty()
            && self.masking.is_none()
            && self.declassified.is_none()
            && self.inherited_groups.is_empty()
    }
}

/// The type of a field, as the app API sees it (ADR-068 §6).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FieldKind {
    /// True or false.
    Bool,
    /// 64-bit integer (counts, never money).
    Int {
        /// Displayed interpretation.
        format: IntFormat,
    },
    /// Exact decimal with a fixed scale (money, quantities, rates).
    Decimal {
        /// Digits after the point. Values with more are rejected, never rounded (ADR-12).
        scale: u8,
        /// Total digits.
        precision: u8,
        /// Displayed interpretation.
        format: DecimalFormat,
    },
    /// Text, optionally limited.
    Text {
        /// Maximum length in characters.
        max_chars: Option<u32>,
        /// Displayed interpretation.
        format: TextFormat,
    },
    /// Text without a limit.
    LongText {
        /// Displayed interpretation.
        format: LongTextFormat,
    },
    /// One of a listed set of values.
    Select {
        /// The allowed values.
        values: Vec<String>,
    },
    /// A calendar date.
    Date,
    /// A wall-clock time.
    Time,
    /// An instant (UTC).
    DateTime,
    /// A link to a document of `doctype`.
    Link {
        /// Target DocType.
        doctype: Name,
    },
    /// A link whose DocType each document names in `type_field`.
    DynamicLink {
        /// The text-like field holding the target DocType.
        type_field: Name,
    },
    /// JSON.
    Json {
        /// Displayed interpretation.
        format: JsonFormat,
    },
    /// Bytes.
    Bytes,
    /// A reference to a file record (WP-PLT-10).
    Attach {
        /// Displayed interpretation.
        format: AttachFormat,
    },
    /// A password or key held by the secrets service. The value never travels in a document
    /// (ADR-068 §2): the document holds only whether one is set.
    Secret,
    /// A reference to a consumer principal (app API 0.7.0, ADR-083 section 2.2 item 4): the
    /// consumer's opaque id, held as an id and encoded like a link's value. It is **not** a
    /// link: the consumer lives in the consumer plane's own store, not in a DocType, so there is
    /// no target DocType, no referential check, no lookup and no join. It is the owner scope of
    /// a consumer-caused record (a consumer reaches the records whose reference is theirs). A
    /// DocType declares at most one (`meta.consumer_ref_duplicate`), and it is `create_only`
    /// (`meta.consumer_ref_not_create_only`): moving a record to another consumer is not an
    /// edit.
    ConsumerRef,
    /// A child table whose rows are documents of `doctype`.
    Table {
        /// The child DocType.
        doctype: Name,
    },
    /// A child table whose rows pick documents of `doctype` through `link_field` (ADR-068 §2).
    TableMultiselect {
        /// The child DocType.
        doctype: Name,
        /// The row's field pointing back at the parent, which holds the picked document.
        link_field: Name,
    },
}

impl FieldKind {
    /// Whether `v` fits this kind (ignoring scale and length; hosts check those on write).
    /// `Null` fits every scalar kind; tables accept nothing here — their rows are checked
    /// against the child DocType.
    pub fn accepts(&self, v: &Value) -> bool {
        match (self, v) {
            // A secret never travels in a document; tables accept nothing here — their rows
            // are checked against the child DocType.
            (FieldKind::Secret, Value::Null) => true,
            (FieldKind::Secret, _) => false,
            (FieldKind::Table { .. }, _) | (FieldKind::TableMultiselect { .. }, _) => false,
            (_, Value::Null) => true,
            (FieldKind::Bool, Value::Bool(_))
            | (FieldKind::Int { .. }, Value::Int(_))
            | (FieldKind::Decimal { .. }, Value::Decimal(_))
            | (FieldKind::Text { .. }, Value::Text(_))
            | (FieldKind::LongText { .. }, Value::Text(_))
            // The listed-values check is the engine's; `values` is available for it.
            | (FieldKind::Select { .. }, Value::Text(_))
            | (FieldKind::Date, Value::Date(_))
            | (FieldKind::Time, Value::Time(_))
            | (FieldKind::DateTime, Value::DateTime(_))
            | (FieldKind::Link { .. }, Value::Id(_))
            | (FieldKind::DynamicLink { .. }, Value::Id(_))
            | (FieldKind::Json { .. }, Value::Json(_))
            | (FieldKind::Bytes, Value::Bytes(_))
            | (FieldKind::ConsumerRef, Value::Id(_))
            | (FieldKind::Attach { .. }, Value::Id(_)) => true,
            // Any other pair does not fit.
            _ => false,
        }
    }
}

/// Which control the author prefers for a field (ADR-068 amendment A2). A hint, never a rule: a
/// client uses it when it offers that control for the field's type and falls back to its default
/// otherwise; it never changes what is stored or allowed, so `validate` does not refuse an
/// unsuitable one. The string forms are the native API's `FieldHints.control` enum, exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ControlHint {
    /// A switch, for a bool.
    Switch,
    /// A checkbox, for a bool.
    Checkbox,
    /// A dropdown, for a select (single or multiple).
    Dropdown,
    /// Radio buttons, for a single select.
    Radio,
    /// A segmented control, for a single select.
    Segmented,
    /// An autocomplete, for a link.
    Autocomplete,
    /// Chips, for a multiple select.
    Chips,
    /// A multi-line text box, for text.
    Textarea,
}

impl ControlHint {
    /// The wire and storage string, equal to the native API enum value.
    pub fn as_str(&self) -> &'static str {
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

    /// Parses the string form; `None` for a value this version does not know (a loader keeps
    /// the field and drops the hint).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "switch" => ControlHint::Switch,
            "checkbox" => ControlHint::Checkbox,
            "dropdown" => ControlHint::Dropdown,
            "radio" => ControlHint::Radio,
            "segmented" => ControlHint::Segmented,
            "autocomplete" => ControlHint::Autocomplete,
            "chips" => ControlHint::Chips,
            "textarea" => ControlHint::Textarea,
            _ => return None,
        })
    }
}

/// The presentation hints (ADR-068 §1.3). They never change what is stored, and none of them
/// is ever a security property (§7.4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct FieldHints {
    /// Longer offline help text.
    pub description: Option<String>,
    /// The UI hides it; hiding is not security (ADR-068 §7.4).
    pub hidden: bool,
    /// Shown in the doc's list view.
    pub in_list_view: bool,
    /// Offered in the standard filter bar.
    pub in_standard_filter: bool,
    /// Not copied when a document is duplicated or corrected.
    pub no_copy: bool,
    /// **Deprecated since 0.3.0** (in the docs, not by `#[deprecated]`: a compiler attribute would
    /// break every consumer that still copies the field under `-D warnings`, and the loader in
    /// `spot-meta` does until WP-MET-01.T6 maps it). Field-level permission level: ADR-068
    /// amendment A1 replaced `permlevel` with
    /// protection groups (ADR-070 section 12). **Ignored by every consumer**; nothing may grant
    /// or deny access by it. Kept only so existing definitions still load at this additive
    /// version. On import a loader maps `permlevel n >= 1` to the protection group `level_n`
    /// (created on the DocType if missing) and logs a deprecation message; `permlevel 0` means
    /// nothing. Removed at the next breaking version.
    pub permlevel: u8,
    /// The control the author prefers (ADR-068 amendment A2); `None` leaves it to the client.
    pub control: Option<ControlHint>,
}

/// One field.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FieldDef {
    /// Storage name.
    pub name: Name,
    /// Type.
    pub kind: FieldKind,
    /// Must be non-null when saved.
    pub required: bool,
    /// May change on a posted document (the `update_posted` operation).
    pub allow_on_posted: bool,
    /// Human label; the name when the definition has none.
    pub label: String,
    /// Not editable by users; the engine decides per state.
    pub read_only: bool,
    /// The value a new document starts with, in the ADR-055 value encoding.
    pub default: Option<Value>,
    /// How the field gets its candidate values (ADR-068 §3); `None` is the platform default.
    pub lookup: Option<LookupSpec>,
    /// Presentation hints (ADR-068 §1.3), never a security property.
    pub hints: FieldHints,
    /// The field's security declaration (ADR-068 amendment A1): its protection group, tags,
    /// mask and declassification. Empty (the default) is an unprotected field. Access itself
    /// is `spot-permissions`' business; inherited groups are computed by `spot-meta`.
    pub security: FieldSecurity,
    /// A `Select` that holds several of its values (ADR-068 amendment A2). The stored value is
    /// an array of the chosen texts, each one of the listed values. Refused on any other kind
    /// (`meta.multiple_not_select`).
    pub multiple: bool,
    /// Settable only when the document is created (app API 0.7.0, ADR-068 amendment A4,
    /// ADR-083 barrier 4). No operation on an existing document may change the stored value:
    /// not an update or a bulk update, not an import, not a sync apply, not an SCIM write, not
    /// a method that writes through the document service. A write that carries the value the
    /// field already has is not a change and is accepted, so a client that sends the whole
    /// document back is not refused. Refused with `document.read_only_field` on that field.
    /// Meaningless on a child table (`meta.create_only_table`) and contradictory with
    /// `allow_on_posted` (`meta.create_only_allow_on_posted`). The default is `false`.
    pub create_only: bool,
}

impl FieldDef {
    /// A field from the four properties every definition has: name, kind, `required` and
    /// `allow_on_posted` (the v0.1 shape). The label is the name; `read_only`, `default`,
    /// `lookup`, `hints`, `security`, `multiple` and `create_only` are unset. Metadata compilers start here and set the
    /// rest through the public fields; the type is `#[non_exhaustive]`, so out-of-crate code
    /// cannot build it with a struct expression.
    pub fn new(name: Name, kind: FieldKind, required: bool, allow_on_posted: bool) -> Self {
        Self {
            label: name.as_str().to_owned(),
            name,
            kind,
            required,
            allow_on_posted,
            read_only: false,
            default: None,
            lookup: None,
            hints: FieldHints::default(),
            security: FieldSecurity::default(),
            multiple: false,
            create_only: false,
        }
    }
}

/// A DocType as the app API sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DocTypeDef {
    /// Storage name, for example `sales_invoice`.
    pub name: Name,
    /// Human label, for example "Sales Invoice".
    pub label: String,
    /// Documents have a draft stage and a Post action (ADR-42).
    pub is_posted: bool,
    /// A child DocType: its documents exist only as rows of a parent.
    pub is_child: bool,
    /// The field holding the human-readable name or number, if any.
    pub title_field: Option<Name>,
    /// Fields in display order.
    pub fields: Vec<FieldDef>,
    /// The protection groups the DocType declares (ADR-068 amendment A1, ADR-070 section 12).
    /// Empty is an unprotected DocType. Declaring grants nothing; it only lets fields name a
    /// group and roles later hold rights against it (`spot-permissions`).
    pub protection_groups: Vec<ProtectionGroupDef>,
    /// The metadata version (ADR-05).
    pub version: u64,
}

impl DocTypeDef {
    /// A DocType from its v0.1 shape: name, label, the posted and child flags, the title field
    /// and the fields in display order. Out-of-crate compilers start here (the type is
    /// `#[non_exhaustive]`, so a struct expression cannot) and stamp the version on the result.
    /// `protection_groups` starts empty; set it through the public field when the DocType
    /// declares protection.
    pub fn new(
        name: Name,
        label: String,
        is_posted: bool,
        is_child: bool,
        title_field: Option<Name>,
        fields: Vec<FieldDef>,
    ) -> Self {
        Self {
            name,
            label,
            is_posted,
            is_child,
            title_field,
            fields,
            protection_groups: Vec::new(),
            version: 0,
        }
    }

    /// The protection group with this name, when the DocType declares one.
    pub fn protection_group(&self, name: &str) -> Option<&ProtectionGroupDef> {
        self.protection_groups
            .iter()
            .find(|g| g.name.as_str() == name)
    }

    /// A field by name.
    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name.as_str() == name)
    }

    /// The child tables, by field name.
    pub fn tables(&self) -> BTreeMap<&str, &Name> {
        self.fields
            .iter()
            .filter_map(|f| match &f.kind {
                FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                    Some((f.name.as_str(), doctype))
                }
                _ => None,
            })
            .collect()
    }

    /// Validates the definition's protection declarations (ADR-068 amendment A1): every
    /// violation is reported at once, as field errors over the `meta.*` codes.
    ///
    /// For an **authored** definition only: `spot-meta` records computed `inherited_groups` on
    /// the effective definition, which this check would refuse as hand-authored, so the effective
    /// view is never passed through `validate`.
    ///
    /// What the contract checks here:
    /// - a field names a group the DocType declares (`meta.unknown_group`) — a field in a
    ///   group is closed by default, so an undeclared group would silently protect nothing;
    /// - a group's masking requirement (`meta.mask_missing`): whether a group lets a role hold
    ///   `read_masked` is a grant question (`spot-permissions`), which the contract cannot
    ///   ask. The contract-visible evidence is a declared mask itself — a member carrying a
    ///   `MaskFormat` proves the group exposes a masked form — so when one member of a group
    ///   carries a mask, every member of it must carry one. Only the **shape** of a format is
    ///   checkable here (`meta.mask_invalid`); whether the field's type may be masked at all
    ///   is the definition layer's rule (WP-MET-01.T6);
    /// - `declassified` needs a non-empty reason (`meta.declassify_reason`); and
    /// - `inherited_groups` is never authored (`meta.inherited_authored`): `spot-meta`
    ///   computes it from the field's inputs (the least access wins).
    ///
    /// What it deliberately does **not** check: the derived-field and tag lints
    /// (`meta.derived_unprotected`, `meta.tag_unprotected`) — they need compiled expressions
    /// and live in `spot-meta` (WP-MET-01.T6) — and everything about rights, which is
    /// `spot-permissions`.
    pub fn validate(&self) -> AppResult<()> {
        let mut err = AppError::new(
            ErrorClass::Validation,
            "meta.validation_failed",
            format!("the definition of `{}` is not valid", self.name),
        );
        // A member that carries a mask makes the group a masked one: every member of it must
        // then carry a well-formed mask. (Whether a group allows `read_masked` at all is a
        // grant question the contract cannot ask; spot-permissions enforces that.)
        let mut masked_groups: Vec<&str> = Vec::new();
        for f in &self.fields {
            if let Some(fmt) = f.security.masking.as_ref() {
                if !fmt.well_formed() {
                    err = err.with_field(
                        FieldPath::field(f.name.as_str()),
                        "meta.mask_invalid",
                        format!(
                            "the mask on `{}` is malformed: LastChars counts 1..=8 characters",
                            f.name
                        ),
                    );
                }
                if let Some(g) = f.security.protection_group.as_ref() {
                    masked_groups.push(g.as_str());
                }
            }
            if let Some(reason) = f.security.declassified.as_ref()
                && reason.trim().is_empty()
            {
                err = err.with_field(
                    FieldPath::field(f.name.as_str()),
                    "meta.declassify_reason",
                    format!(
                        "`{}` is declassified: the reviewed decision needs a reason",
                        f.name
                    ),
                );
            }
            if !f.security.inherited_groups.is_empty() {
                err = err.with_field(
                    FieldPath::field(f.name.as_str()),
                    "meta.inherited_authored",
                    format!(
                        "`{}` sets inherited_groups by hand: spot-meta computes it from the \
                         field's inputs, a definition never authors it",
                        f.name
                    ),
                );
            }
        }
        for f in &self.fields {
            if f.multiple && !matches!(f.kind, FieldKind::Select { .. }) {
                err = err.with_field(
                    FieldPath::field(f.name.as_str()),
                    "meta.multiple_not_select",
                    format!(
                        "`{}` is marked multiple, but only a select can hold several values",
                        f.name
                    ),
                );
            }
        }
        let mut consumer_refs = 0usize;
        for f in &self.fields {
            if f.create_only
                && matches!(
                    f.kind,
                    FieldKind::Table { .. } | FieldKind::TableMultiselect { .. }
                )
            {
                err = err.with_field(
                    FieldPath::field(f.name.as_str()),
                    "meta.create_only_table",
                    format!(
                        "`{}` is a child table: create_only applies to a stored value, and \
                         rows are documents of their own",
                        f.name
                    ),
                );
            }
            if f.create_only && f.allow_on_posted {
                err = err.with_field(
                    FieldPath::field(f.name.as_str()),
                    "meta.create_only_allow_on_posted",
                    format!(
                        "`{}` is both create_only and allow_on_posted: a field that is never \
                         changed after creation cannot be one that a posted document may change",
                        f.name
                    ),
                );
            }
            if matches!(f.kind, FieldKind::ConsumerRef) {
                consumer_refs += 1;
                if consumer_refs > 1 {
                    err = err.with_field(
                        FieldPath::field(f.name.as_str()),
                        "meta.consumer_ref_duplicate",
                        format!(
                            "`{}` is a second consumer reference: a DocType has one owner scope",
                            f.name
                        ),
                    );
                }
                if !f.create_only {
                    err = err.with_field(
                        FieldPath::field(f.name.as_str()),
                        "meta.consumer_ref_not_create_only",
                        format!(
                            "`{}` is a consumer reference and must be create_only: changing it \
                             would move the record to another consumer",
                            f.name
                        ),
                    );
                }
            }
        }
        masked_groups.sort_unstable();
        masked_groups.dedup();
        for f in &self.fields {
            if let Some(g) = f.security.protection_group.as_ref() {
                let declared = self.protection_group(g.as_str());
                if declared.is_none() {
                    err = err.with_field(
                        FieldPath::field(f.name.as_str()),
                        "meta.unknown_group",
                        format!(
                            "`{}` names the protection group `{}`, which `{}` does not declare",
                            f.name, g, self.name
                        ),
                    );
                }
                if masked_groups.contains(&g.as_str()) && f.security.masking.is_none() {
                    err = err.with_field(
                        FieldPath::field(f.name.as_str()),
                        "meta.mask_missing",
                        format!(
                            "`{}` is in the masked group `{}`: it needs a MaskFormat so a \
                             `read_masked` role has a form to read",
                            f.name, g
                        ),
                    );
                }
            }
        }
        if err.fields.is_empty() {
            return Ok(());
        }
        Err(err)
    }
}

/// A `meta.*` definition error: always a metadata-authoring fault, reported as validation.
fn invalid_meta(message: String, field: Option<FieldPath>) -> AppError {
    let mut e = AppError::new(ErrorClass::Validation, "meta.invalid_name", message);
    if let Some(f) = field {
        e = e.with_field(
            f,
            "meta.invalid_name",
            "the name is not a valid storage name",
        );
    }
    e
}
