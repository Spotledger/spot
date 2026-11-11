//! Property descriptors (ADR-068 §1): every property of a field and of a DocType described as
//! data, so validation, the override rules, the admin UI and the API schema read one table.
//!
//! The table is the only list of what a site may override. [`crate::Property::parse`] and the
//! registry's override step look a property up here; a property added to
//! [`crate::FieldSpec`] or [`crate::Definition`] without a row fails the guard test.

use crate::definition::FieldType;

/// Which of ADR-068 §1.3's layers a property belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// Identity and storage: fixed once a field exists.
    Storage,
    /// Rules: overridable under the rules of each property.
    Rules,
    /// Behaviour (`lookup`): overridable by a site and by a view.
    Behaviour,
    /// Presentation hints: never change what is stored.
    Presentation,
    /// Security: protection groups, tags, masking. A site may only add protection, never remove.
    Security,
}

impl Layer {
    /// The layer's name, as used in messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Rules => "rules",
            Self::Behaviour => "behaviour",
            Self::Presentation => "presentation",
            Self::Security => "security",
        }
    }
}

/// Who may override a property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overridable {
    /// Nobody: changing it would change stored data or the identity of the field.
    Never,
    /// A site (a customisation, ADR-05).
    Site,
    /// A site, and a view for one form (WP-UI-10).
    SiteAndView,
    /// A site may only add protection, never remove (ADR-068 amendment A1, ADR-070 section 12).
    SiteUpward,
}

/// What a property is a property of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A field.
    Field,
    /// A DocType.
    DocType,
}

/// The field types, and for the format parameters the formats, a property is valid on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliesTo {
    /// The base types; empty means every type.
    pub types: &'static [FieldType],
    /// The formats; empty means any format of those types.
    pub formats: &'static [&'static str],
}

impl AppliesTo {
    /// Whether the property is valid on a field of type `ty` with format `format` (`None` for
    /// the type's default format).
    pub fn accepts(&self, ty: FieldType, format: Option<&str>) -> bool {
        (self.types.is_empty() || self.types.contains(&ty))
            && (self.formats.is_empty() || format.is_some_and(|f| self.formats.contains(&f)))
    }

    /// A sentence naming where the property applies, for refusals.
    pub fn describe(&self) -> String {
        let types: Vec<String> = self
            .types
            .iter()
            .map(|t| format!("{t:?}").to_lowercase())
            .collect();
        match (self.types.is_empty(), self.formats.is_empty()) {
            (true, _) => "every field".into(),
            (false, true) => format!("{} fields", types.join(", ")),
            (false, false) => format!(
                "{} fields with format {}",
                types.join(", "),
                self.formats.join(" or ")
            ),
        }
    }
}

/// One property, described.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyDescriptor {
    /// The key as written in a definition.
    pub name: &'static str,
    /// Of a field or of a DocType.
    pub target: Target,
    /// The layer.
    pub layer: Layer,
    /// Where the property is valid.
    pub applies_to: AppliesTo,
    /// Who may override it.
    pub overridable: Overridable,
    /// One sentence describing the property, for the admin UI and the generated API schema.
    pub doc: &'static str,
    /// The work package that makes a reserved property real. A pending property is listed so
    /// its name is taken, but a definition does not accept it yet.
    pub pending: Option<&'static str>,
}

const ANY: AppliesTo = AppliesTo {
    types: &[],
    formats: &[],
};

const fn on(types: &'static [FieldType]) -> AppliesTo {
    AppliesTo {
        types,
        formats: &[],
    }
}

use FieldType as T;

/// Types whose `options` mean something.
const WITH_OPTIONS: &[FieldType] = &[
    T::Select,
    T::Link,
    T::DynamicLink,
    T::Table,
    T::TableMultiselect,
];
/// Types that may carry a `default` (the validator refuses the rest).
const WITH_DEFAULT: &[FieldType] = &[
    T::Bool,
    T::Int,
    T::Decimal,
    T::Text,
    T::LongText,
    T::Select,
    T::Date,
    T::Time,
    T::DateTime,
];
/// Types that may carry a `lookup` (ADR-068 §3.1). A secret takes the flag form only
/// (`lookup = true`, its blind index; WP-SEC-03.T2), which a site cannot set: an override
/// carries the seam table as text, so the flag is definition-only.
const WITH_LOOKUP: &[FieldType] = &[
    T::Link,
    T::DynamicLink,
    T::Select,
    T::TableMultiselect,
    T::Text,
    T::Secret,
];

/// Types that may carry a `fetch_from`: value types, not links, tables or secrets.
const WITH_FETCH: &[FieldType] = &[
    T::Bool,
    T::Int,
    T::Decimal,
    T::Text,
    T::LongText,
    T::Select,
    T::Date,
    T::Time,
    T::DateTime,
    T::Json,
    T::Bytes,
    T::Attach,
];

const fn field(
    name: &'static str,
    layer: Layer,
    applies_to: AppliesTo,
    overridable: Overridable,
    doc: &'static str,
) -> PropertyDescriptor {
    PropertyDescriptor {
        name,
        target: Target::Field,
        layer,
        applies_to,
        overridable,
        doc,
        pending: None,
    }
}

const fn doctype(
    name: &'static str,
    layer: Layer,
    overridable: Overridable,
    doc: &'static str,
) -> PropertyDescriptor {
    PropertyDescriptor {
        name,
        target: Target::DocType,
        layer,
        applies_to: ANY,
        overridable,
        doc,
        pending: None,
    }
}

use Layer::{Behaviour, Presentation, Rules, Security, Storage};
use Overridable::{Never, Site, SiteAndView, SiteUpward};

static TABLE: &[PropertyDescriptor] = &[
    // The properties of a field.
    field(
        "name",
        Storage,
        ANY,
        Never,
        "The storage name; a field cannot be renamed",
    ),
    field("type", Storage, ANY, Never, "The field's base type"),
    field(
        "format",
        Storage,
        ANY,
        Never,
        "How the base type's value is interpreted",
    ),
    field(
        "syntax",
        Storage,
        AppliesTo {
            types: &[T::LongText],
            formats: &["code"],
        },
        Never,
        "The language of a `code` format",
    ),
    field(
        "symbology",
        Storage,
        AppliesTo {
            types: &[T::Text],
            formats: &["barcode"],
        },
        Never,
        "The symbology of a `barcode` format",
    ),
    field(
        "max",
        Storage,
        AppliesTo {
            types: &[T::Int],
            formats: &["rating"],
        },
        Never,
        "The highest value of a `rating` format",
    ),
    field(
        "currency_field",
        Storage,
        AppliesTo {
            types: &[T::Decimal],
            formats: &["currency"],
        },
        Never,
        "The link field that holds a `currency` field's currency",
    ),
    field(
        "scale",
        Storage,
        on(&[T::Decimal]),
        Never,
        "The digits after the decimal point of a `decimal`",
    ),
    field(
        "precision",
        Storage,
        on(&[T::Decimal]),
        Never,
        "The total digits of a `decimal`",
    ),
    // A site may raise a limit that exists, never lower or add one (custom.rs).
    field(
        "max_chars",
        Storage,
        on(&[T::Text]),
        Site,
        "The longest text of a `text` field; a site may only raise it",
    ),
    field(
        "unique",
        Storage,
        ANY,
        Never,
        "Whether values must be unique",
    ),
    field("index", Storage, ANY, Never, "Whether the field is indexed"),
    // `multiple` changes the shape of the stored value, so it is definition-only.
    field(
        "multiple",
        Storage,
        on(&[T::Select]),
        Never,
        "Whether a select holds several of its values; definition-only, because it changes the stored shape",
    ),
    // `options` is storage for a link or table; only a select's values may grow, which is the
    // one case a site may touch (the registry checks that rule).
    field(
        "options",
        Rules,
        on(WITH_OPTIONS),
        Site,
        "A select's listed values, or a link, dynamic link, table or multi-pick's target",
    ),
    field(
        "required",
        Rules,
        ANY,
        Site,
        "Whether the field must be set when a document is saved",
    ),
    field(
        "read_only",
        Rules,
        ANY,
        Site,
        "Whether users may not edit the field",
    ),
    field(
        "allow_on_posted",
        Rules,
        ANY,
        Site,
        "Whether the field may change on a posted document",
    ),
    field(
        "default",
        Rules,
        on(WITH_DEFAULT),
        Site,
        "The value a new document starts with, parsed by type",
    ),
    field(
        "depends_on",
        Rules,
        ANY,
        Site,
        "The expression that decides whether the field is shown",
    ),
    field(
        "mandatory_depends_on",
        Rules,
        ANY,
        Site,
        "The expression that makes the field required while it holds",
    ),
    field(
        "read_only_depends_on",
        Rules,
        ANY,
        Site,
        "The expression that makes the field read-only while it holds",
    ),
    field(
        "fetch_from",
        Rules,
        on(WITH_FETCH),
        Site,
        "`<link_field>.<target_field>`: the value copied from the document the link points at",
    ),
    // A site may replace a field's lookup with a whole spec or clear it (ADR-068 §3.4); a
    // view may too once views exist (WP-UI-10).
    field(
        "lookup",
        Behaviour,
        on(WITH_LOOKUP),
        SiteAndView,
        "How the field gets its candidate values; on a secret, `true` for a blind-index lookup",
    ),
    field(
        "label",
        Presentation,
        ANY,
        SiteAndView,
        "The field's human label",
    ),
    field(
        "description",
        Presentation,
        ANY,
        SiteAndView,
        "Longer help text for the field",
    ),
    field(
        "hidden",
        Presentation,
        ANY,
        SiteAndView,
        "Whether the UI hides the field; hiding is not security",
    ),
    field(
        "in_list_view",
        Presentation,
        ANY,
        SiteAndView,
        "Whether the field is shown in the list view",
    ),
    field(
        "in_standard_filter",
        Presentation,
        ANY,
        SiteAndView,
        "Whether the field is offered in the standard filter bar",
    ),
    field(
        "no_copy",
        Presentation,
        ANY,
        SiteAndView,
        "Whether the field is left out when a document is copied",
    ),
    field(
        "control",
        Presentation,
        ANY,
        SiteAndView,
        "The control the author prefers; a hint a client may ignore and fall back from",
    ),
    // A permission level decides who sees the field; a view must not widen it, so it is the
    // one hint a view cannot override (ADR-068 §7.4).
    field(
        "permlevel",
        Presentation,
        ANY,
        Site,
        "The deprecated field-level permission level; ignored and always 0 in the app view",
    ),
    // Security properties (ADR-068 amendment A1, ADR-070 section 12).
    field(
        "protection_group",
        Security,
        ANY,
        SiteUpward,
        "The protection group the field belongs to; closed by default",
    ),
    field(
        "classification",
        Security,
        ANY,
        SiteUpward,
        "Classification tags such as `pii` or `financial`; they grant nothing",
    ),
    field(
        "masking",
        Security,
        ANY,
        SiteUpward,
        "How a masked value is shown to a role holding `read_masked`",
    ),
    field(
        "declassified",
        Security,
        ANY,
        Never,
        "The reviewed reason a field is deliberately declassified",
    ),
    // The properties of a DocType.
    doctype(
        "name",
        Storage,
        Never,
        "The DocType's storage name; it cannot be renamed",
    ),
    doctype("label", Presentation, Site, "The DocType's human label"),
    doctype("app", Storage, Never, "The app that owns the DocType"),
    doctype("module", Storage, Never, "The module within the app"),
    doctype(
        "is_posted",
        Storage,
        Never,
        "Whether documents have a draft stage and a Post action",
    ),
    doctype(
        "is_child",
        Storage,
        Never,
        "Whether documents exist only as child rows of a parent",
    ),
    doctype(
        "title_field",
        Presentation,
        Site,
        "The field holding the human-readable name or number",
    ),
    // Renaming a series after documents exist is a data migration, not a site setting.
    doctype(
        "naming",
        Storage,
        Never,
        "How documents are named: a series pattern, a field, `hash` or `prompt`",
    ),
    doctype(
        "fields",
        Storage,
        Never,
        "The DocType's fields, in display order",
    ),
    doctype(
        "indexes",
        Storage,
        Never,
        "The DocType's multi-field indexes",
    ),
    doctype(
        "protection_groups",
        Storage,
        Never,
        "The protection groups the DocType declares",
    ),
];

/// Every descriptor: the properties of a field, then those of a DocType.
pub fn descriptors() -> &'static [PropertyDescriptor] {
    TABLE
}

/// The descriptor of a field property.
pub fn descriptor(name: &str) -> Option<&'static PropertyDescriptor> {
    TABLE
        .iter()
        .find(|d| d.target == Target::Field && d.name == name)
}

/// The descriptor of a DocType property.
pub fn doctype_descriptor(name: &str) -> Option<&'static PropertyDescriptor> {
    TABLE
        .iter()
        .find(|d| d.target == Target::DocType && d.name == name)
}

impl PropertyDescriptor {
    /// Why a site may not override this property, or `None` when it may. Names the layer.
    pub fn refusal(&self) -> Option<String> {
        let key = self.name;
        if let Some(wp) = self.pending {
            return Some(format!(
                "`{key}` is reserved and not available yet (it arrives with {wp})"
            ));
        }
        match self.overridable {
            Never => Some(format!(
                "`{key}` cannot be overridden: it is in the {} layer and changing it would change stored data; add a custom field instead",
                self.layer.name()
            )),
            Site | SiteAndView | SiteUpward => None,
        }
    }
}
