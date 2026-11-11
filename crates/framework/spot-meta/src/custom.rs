//! Customisations made at runtime: custom fields and property overrides (ADR-05).
//!
//! They live in memory in this task; persisting them is WP-MET-01.T2. They never touch a
//! definition: the registry merges them over the definitions, so a standard update can always
//! replace a definition without colliding with a site's changes.

use crate::definition::{FieldSpec, LookupSpec, MaskSpec, split_options};
use crate::descriptor::{descriptor, doctype_descriptor};
use crate::error::{MetaError, MetaErrorKind};

/// Prefix every custom field's name must have.
pub const CUSTOM_PREFIX: &str = "custom_";

/// A field added to a DocType by a site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomField {
    /// The DocType it is added to.
    pub doctype: String,
    /// The field; its name must start with `custom_`.
    pub spec: FieldSpec,
    /// Place the field right after this one; at the end when `None`.
    pub insert_after: Option<String>,
}

/// One property a site may change. Anything else (a type change, a rename) is not expressible
/// and [`Property::parse`] refuses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Property {
    /// The label (DocType or field).
    Label(String),
    /// Whether the field is required.
    Required(bool),
    /// Whether the field is read-only.
    ReadOnly(bool),
    /// Whether the field may change on a posted document.
    AllowOnPosted(bool),
    /// The default, as text; `None` clears it.
    Default(Option<String>),
    /// The full list of a select field's values. Every existing value must stay.
    Options(Vec<String>),
    /// A new, higher `max_chars` for a limited text field.
    MaxChars(u32),
    /// The DocType's title field.
    TitleField(String),
    /// Longer help text; `None` clears it.
    Description(Option<String>),
    /// Whether the UI hides the field.
    Hidden(bool),
    /// Whether the field is shown in the list view.
    InListView(bool),
    /// Whether the field is offered in the standard filter bar.
    InStandardFilter(bool),
    /// Whether the field is left out when a document is copied.
    NoCopy(bool),
    /// The preferred control (ADR-068 amendment A2), one of [`crate::CONTROL_HINTS`]; `None`
    /// clears it and leaves the choice to the client. A hint, never a rule.
    Control(Option<String>),
    /// The permission level of the field.
    Permlevel(u8),
    /// The field's whole lookup spec (ADR-068 §3.4); `None` clears it, which restores the
    /// platform default. The override replaces the spec as a whole; there is no merge.
    Lookup(Option<LookupSpec>),
    /// The expression text that decides whether the field is shown; `None` clears it. Kept as
    /// text and shape-checked when the registry builds; never parsed here.
    DependsOn(Option<String>),
    /// The expression text that makes the field required; `None` clears it.
    MandatoryDependsOn(Option<String>),
    /// The expression text that makes the field read-only; `None` clears it.
    ReadOnlyDependsOn(Option<String>),
    /// `<link_field>.<target_field>`; `None` clears it.
    FetchFrom(Option<String>),
    /// The protection group this field belongs to (ADR-068 amendment A1). A site may only add
    /// protection, never remove (upward-only).
    ProtectionGroup(Option<String>),
    /// Classification tags (ADR-070 section 12). A site may only add tags, never remove them.
    Classification(Vec<String>),
    /// The field's masking format (ADR-070 section 12). A site may set a mask on a field that
    /// has none, or replace a mask with one that reveals no more (upward-only).
    Masking(Option<MaskSpec>),
}

impl Property {
    /// The key under which the property is named in text form.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Label(_) => "label",
            Self::Required(_) => "required",
            Self::ReadOnly(_) => "read_only",
            Self::AllowOnPosted(_) => "allow_on_posted",
            Self::Default(_) => "default",
            Self::Options(_) => "options",
            Self::MaxChars(_) => "max_chars",
            Self::TitleField(_) => "title_field",
            Self::Description(_) => "description",
            Self::Hidden(_) => "hidden",
            Self::InListView(_) => "in_list_view",
            Self::InStandardFilter(_) => "in_standard_filter",
            Self::NoCopy(_) => "no_copy",
            Self::Control(_) => "control",
            Self::Permlevel(_) => "permlevel",
            Self::DependsOn(_) => "depends_on",
            Self::MandatoryDependsOn(_) => "mandatory_depends_on",
            Self::ReadOnlyDependsOn(_) => "read_only_depends_on",
            Self::FetchFrom(_) => "fetch_from",
            Self::Lookup(_) => "lookup",
            Self::ProtectionGroup(_) => "protection_group",
            Self::Classification(_) => "classification",
            Self::Masking(_) => "masking",
        }
    }

    /// Parses a property from its key and a text value, as stored by a site. The descriptor
    /// table decides: a key it does not know, a reserved one and one a site may never override
    /// are refused, naming the key (and for the last the layer).
    pub fn parse(key: &str, value: &str) -> Result<Self, MetaError> {
        let bad = |why: String| MetaError::new(MetaErrorKind::BadOverride, why);
        let Some(d) = descriptor(key).or_else(|| doctype_descriptor(key)) else {
            return Err(bad(format!("`{key}` is not an overridable property")));
        };
        if let Some(why) = d.refusal() {
            return Err(bad(why));
        }
        let flag = |v: &str| match v {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(bad(format!("`{key}` must be `true` or `false`, not `{v}`"))),
        };
        let optional = || (!value.is_empty()).then(|| value.to_owned());
        match key {
            "label" => Ok(Self::Label(value.to_owned())),
            "required" => flag(value).map(Self::Required),
            "read_only" => flag(value).map(Self::ReadOnly),
            "allow_on_posted" => flag(value).map(Self::AllowOnPosted),
            "default" => Ok(Self::Default(optional())),
            "options" => Ok(Self::Options(split_options(value))),
            "max_chars" => value
                .parse()
                .map(Self::MaxChars)
                .map_err(|_| bad(format!("`{key}` must be a whole number, not `{value}`"))),
            "depends_on" => Ok(Self::DependsOn(optional())),
            "mandatory_depends_on" => Ok(Self::MandatoryDependsOn(optional())),
            "read_only_depends_on" => Ok(Self::ReadOnlyDependsOn(optional())),
            "fetch_from" => Ok(Self::FetchFrom(optional())),
            "title_field" => Ok(Self::TitleField(value.to_owned())),
            "description" => Ok(Self::Description(optional())),
            "hidden" => flag(value).map(Self::Hidden),
            "in_list_view" => flag(value).map(Self::InListView),
            "in_standard_filter" => flag(value).map(Self::InStandardFilter),
            "no_copy" => flag(value).map(Self::NoCopy),
            // An unknown control name is refused here too, with the same list of valid ones;
            // the value empty clears the hint.
            "control" => {
                if !value.is_empty() {
                    crate::definition::check_control_name(value).map_err(bad)?;
                }
                Ok(Self::Control(optional()))
            }
            "permlevel" => value.parse().map(Self::Permlevel).map_err(|_| {
                bad(format!(
                    "`{key}` must be a whole number from 0 to 255, not `{value}`"
                ))
            }),
            // The text form of a lookup is the TOML of a definition's `[fields.lookup]` table:
            // either key lines, or one line of `key = value, ...` (an inline table's body).
            // An empty value clears the field's spec.
            "lookup" if value.is_empty() => Ok(Self::Lookup(None)),
            // `lookup = true` (a secret's blind index) is a definition-only flag: an override
            // carries a text value and would change what is stored, not how it is found.
            "lookup" if value == "true" || value == "false" => Err(bad(
                "`lookup = true` on a secret field is definition-only; a site overrides only a \
                 lookup table"
                    .into(),
            )),
            "lookup" => parse_lookup(value)
                .map(|s| Self::Lookup(Some(s)))
                .map_err(|e| bad(format!("`{key}` is not a lookup: {}", trim_toml_error(&e)))),
            "protection_group" => Ok(Self::ProtectionGroup(optional())),
            "classification" => Ok(Self::Classification(split_options(value))),
            "masking" if value.is_empty() => Ok(Self::Masking(None)),
            "masking" => parse_masking(value)
                .map(|s| Self::Masking(Some(s)))
                .map_err(|e| bad(format!("`{key}` is not a masking spec: {e}"))),
            _ => Err(bad(format!("`{key}` is not an overridable property"))),
        }
    }
}

/// A property of a DocType (`field` is `None`) or of one of its fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Override {
    /// The DocType.
    pub doctype: String,
    /// The field, or `None` for a property of the DocType itself.
    pub field: Option<String>,
    /// What to set.
    pub property: Property,
}

/// Reads the text form of a lookup (see [`Property::parse`]).
fn parse_lookup(value: &str) -> Result<LookupSpec, toml::de::Error> {
    if value.contains('\n') {
        return toml::from_str(value);
    }
    #[derive(serde::Deserialize)]
    struct Inline {
        lookup: LookupSpec,
    }
    toml::from_str::<Inline>(&format!("lookup = {{ {value} }}")).map(|i| i.lookup)
}

/// Reads the text form of a masking spec (see [`Property::parse`]).
fn parse_masking(value: &str) -> Result<MaskSpec, String> {
    if value.contains('\n') {
        toml::from_str(value).map_err(|e| trim_toml_error(&e))
    } else {
        #[derive(serde::Deserialize)]
        struct Inline {
            masking: MaskSpec,
        }
        toml::from_str::<Inline>(&format!("masking = {{ {value} }}"))
            .map(|i| i.masking)
            .map_err(|e| trim_toml_error(&e))
    }
}

/// Drops the noisy tail of a `toml` error (the "expected one of ..." part and the internal
/// locations), which never names a field a site can act on.
fn trim_toml_error(e: &toml::de::Error) -> String {
    let text = e.to_string();
    match text.split_once('\n') {
        Some((first, _)) => first.trim_end().to_owned(),
        None => text.trim_end().to_owned(),
    }
}

/// The customisations of one site. Every change bumps the revision, so the registry's metadata
/// version rises whatever was changed, even a removal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Customisations {
    custom_fields: Vec<CustomField>,
    overrides: Vec<Override>,
    revision: u64,
}

impl Customisations {
    /// No customisations.
    pub fn new() -> Self {
        Self::default()
    }

    /// No customisations, at a given revision. A follower that holds only the definitions and
    /// an agreed metadata version `v` builds with `with_revision(v - 1)` and gets version `v`
    /// (`version = revision + 1`); later changes still raise it, so it stays monotonic.
    /// `with_revision(0)` equals [`Customisations::new`].
    #[must_use]
    pub fn with_revision(revision: u64) -> Self {
        Self {
            revision,
            ..Self::default()
        }
    }

    /// Adds a custom field. Checked when the registry is built.
    pub fn add_field(&mut self, doctype: &str, spec: FieldSpec, insert_after: Option<&str>) {
        self.custom_fields.push(CustomField {
            doctype: doctype.to_owned(),
            spec,
            insert_after: insert_after.map(str::to_owned),
        });
        self.revision += 1;
    }

    /// Sets a property of a field (`Some`) or of the DocType (`None`). A later override of the
    /// same property wins. Checked when the registry is built.
    pub fn set(&mut self, doctype: &str, field: Option<&str>, property: Property) {
        self.overrides.push(Override {
            doctype: doctype.to_owned(),
            field: field.map(str::to_owned),
            property,
        });
        self.revision += 1;
    }

    /// Removes a custom field and the overrides that name it. Returns whether it existed.
    pub fn remove_field(&mut self, doctype: &str, name: &str) -> bool {
        let before = self.custom_fields.len();
        self.custom_fields
            .retain(|c| !(c.doctype == doctype && c.spec.name == name));
        self.overrides
            .retain(|o| !(o.doctype == doctype && o.field.as_deref() == Some(name)));
        let removed = self.custom_fields.len() != before;
        if removed {
            self.revision += 1;
        }
        removed
    }

    /// Removes every override of a property of the DocType or field. Returns whether any existed.
    pub fn clear(&mut self, doctype: &str, field: Option<&str>, key: &str) -> bool {
        let before = self.overrides.len();
        self.overrides.retain(|o| {
            !(o.doctype == doctype && o.field.as_deref() == field && o.property.key() == key)
        });
        let removed = self.overrides.len() != before;
        if removed {
            self.revision += 1;
        }
        removed
    }

    /// Records that something outside the customisations changed (the definitions), so the
    /// next build gets a higher version.
    pub fn touch(&mut self) {
        self.revision += 1;
    }

    /// Sets the revision exactly, whatever it was. For the live-metadata layer, which owns the
    /// numbering: it stamps the revision so the registry's metadata version (`revision + 1`)
    /// equals the site version of the snapshot it is building. Nothing else should call it.
    pub fn stamp_revision(&mut self, revision: u64) {
        self.revision = revision;
    }

    /// The custom fields, in the order added.
    pub fn fields(&self) -> &[CustomField] {
        &self.custom_fields
    }

    /// The overrides, in the order set.
    pub fn overrides(&self) -> &[Override] {
        &self.overrides
    }

    /// How many changes have been made. Only ever grows.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}
