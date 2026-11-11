//! The changes a site or an app update can make at runtime (ADR-05; ADR-068 §1, §7).

use std::fmt;

use spot_meta::FieldSpec;

/// A field the scenario adds or an update ships, as the host receives it. A plain input the
/// host turns into its own field type, so the suite pins behaviour, not a constructor shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldSpecInput {
    /// Storage name (a custom field's name must start with `custom_`, ADR-05).
    pub name: String,
    /// Short label.
    pub label: Option<String>,
    /// Field type key, for example `text`, `select`, `int`, `decimal`, `date`.
    pub ty: String,
    /// Select values (one per entry) or a link target.
    pub options: Option<Vec<String>>,
    /// Must be set when saved.
    pub required: bool,
    /// Not editable by users.
    pub read_only: bool,
    /// May change on a posted document.
    pub allow_on_posted: bool,
    /// Default value as text.
    pub default: Option<String>,
    /// Digits after the point (`decimal` only).
    pub scale: Option<u8>,
}

impl FieldSpecInput {
    /// A plain text field.
    pub fn text(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            ty: "text".to_owned(),
            options: None,
            required: false,
            read_only: false,
            allow_on_posted: false,
            default: None,
            scale: None,
        }
    }

    /// A select field with the given values.
    pub fn select(name: &str, values: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            ty: "select".to_owned(),
            options: Some(values.iter().map(|v| (*v).to_owned()).collect()),
            required: false,
            read_only: false,
            allow_on_posted: false,
            default: None,
            scale: None,
        }
    }

    /// An int field.
    pub fn int(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            ty: "int".to_owned(),
            options: None,
            required: false,
            read_only: false,
            allow_on_posted: false,
            default: None,
            scale: None,
        }
    }

    /// A decimal field with the given scale.
    pub fn decimal(name: &str, scale: u8) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            ty: "decimal".to_owned(),
            options: None,
            required: false,
            read_only: false,
            allow_on_posted: false,
            default: None,
            scale: Some(scale),
        }
    }

    /// Sets a label.
    #[must_use]
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    /// Sets `required`.
    #[must_use]
    pub fn with_required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Sets `allow_on_posted` (ADR-068 §1.3 rules layer).
    #[must_use]
    pub fn with_allow_on_posted(mut self, allow_on_posted: bool) -> Self {
        self.allow_on_posted = allow_on_posted;
        self
    }

    /// Sets a default.
    #[must_use]
    pub fn with_default(mut self, default: &str) -> Self {
        self.default = Some(default.to_owned());
        self
    }

    /// Converts to the `spot_meta::FieldSpec` the metadata registry takes. Only the types the
    /// suite uses (`text`, `select`, `int`, `decimal`) are expressible; anything else is a bug
    /// in the suite and panics (test infrastructure).
    #[must_use]
    pub fn into_meta_spec(self) -> FieldSpec {
        use spot_meta::FieldType as Ty;
        let ty = match self.ty.as_str() {
            "text" => Ty::Text,
            "select" => Ty::Select,
            "int" => Ty::Int,
            "decimal" => Ty::Decimal,
            other => panic!("suite uses an unknown field type {other:?}"),
        };
        let mut spec = FieldSpec::new(&self.name, ty);
        spec.label = self.label;
        if let Some(values) = &self.options {
            spec.options = Some(values.join("\n"));
        }
        spec.required = self.required;
        spec.read_only = self.read_only;
        spec.allow_on_posted = self.allow_on_posted;
        spec.default = self.default;
        spec.scale = self.scale;
        spec
    }
}

/// A property a site may change through the suite, restricted to what ADR-05/ADR-068 §1 allow
/// a site to change (the rules layer; storage-layer properties are never expressible here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetProperty {
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
    /// The full list of a select field's values; every existing value must stay.
    Options(Vec<String>),
    /// A new, higher `max_chars` for a limited text field.
    MaxChars(u32),
    /// The DocType's title field.
    TitleField(String),
}

impl SetProperty {
    /// The property key, as the metadata layer names it.
    #[must_use]
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
        }
    }

    /// The textual value, for hosts that store properties as text.
    #[must_use]
    pub fn value_text(&self) -> String {
        match self {
            Self::Label(v) | Self::TitleField(v) => v.clone(),
            Self::Required(v) | Self::ReadOnly(v) | Self::AllowOnPosted(v) => {
                if *v {
                    "true".to_owned()
                } else {
                    "false".to_owned()
                }
            }
            Self::Default(Some(v)) => v.clone(),
            Self::Default(None) => String::new(),
            Self::Options(values) => values.join("\n"),
            Self::MaxChars(n) => n.to_string(),
        }
    }
}

/// One runtime change, applied atomically ([`crate::LivenessHost::apply`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A site adds a custom field (ADR-05; the name must be site-prefixed).
    AddCustomField {
        /// The DocType to add it to.
        doctype: String,
        /// The field.
        spec: FieldSpecInput,
        /// Place the field right after this one; at the end when `None`.
        insert_after: Option<String>,
    },
    /// A site removes a custom field it added. Removing a field other things refer to is
    /// refused (ADR-05).
    RemoveCustomField {
        /// The DocType to remove it from.
        doctype: String,
        /// The field's name.
        name: String,
    },
    /// A site overrides a property (ADR-068 §1: the rules layer; §7.3: never storage).
    SetProperty {
        /// The DocType.
        doctype: String,
        /// The field, or `None` for a property of the DocType itself.
        field: Option<String>,
        /// What to set.
        property: SetProperty,
    },
    /// A runtime-created DocType (ADR-15: every DocType is exposed the moment it exists).
    AddDocType {
        /// The new DocType's definition. Fields may name engine-reserved names only as
        /// engine-managed: the host adds those.
        definition: AddDocTypeDef,
    },
    /// An app update replaces the standard definitions (ADR-05; ADR-068 §1.1: definitions are
    /// replaced wholesale so a site's customisations survive the merge).
    ReplaceDefinitions {
        /// The new standard set.
        definitions: Vec<DocTypeDefInput>,
    },
}

/// A DocType a runtime change creates, expressed in suite input form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddDocTypeDef {
    /// Storage name.
    pub name: String,
    /// Human label.
    pub label: Option<String>,
    /// Documents have a draft stage and a Post action (ADR-42).
    pub is_posted: bool,
    /// The field holding the human-readable name or number.
    pub title_field: Option<String>,
    /// Fields, engine-managed ones excluded (the host adds them).
    pub fields: Vec<FieldSpecInput>,
}

impl AddDocTypeDef {
    /// A posted DocType with the given name and fields.
    #[must_use]
    pub fn posted(name: &str, fields: Vec<FieldSpecInput>) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            is_posted: true,
            title_field: None,
            fields,
        }
    }

    /// Sets a label.
    #[must_use]
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    /// Sets the title field.
    #[must_use]
    pub fn with_title_field(mut self, title_field: &str) -> Self {
        self.title_field = Some(title_field.to_owned());
        self
    }
}

/// A standard DocType as an app update ships it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocTypeDefInput {
    /// Storage name.
    pub name: String,
    /// Human label.
    pub label: Option<String>,
    /// Documents have a draft stage and a Post action.
    pub is_posted: bool,
    /// The field holding the human-readable name or number.
    pub title_field: Option<String>,
    /// Fields.
    pub fields: Vec<FieldSpecInput>,
}

impl DocTypeDefInput {
    /// A standard DocType with the given name and fields.
    #[must_use]
    pub fn new(name: &str, fields: Vec<FieldSpecInput>) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            is_posted: false,
            title_field: None,
            fields,
        }
    }

    /// Sets a label.
    #[must_use]
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    /// Sets `is_posted`.
    #[must_use]
    pub fn posted(mut self) -> Self {
        self.is_posted = true;
        self
    }

    /// Sets the title field.
    #[must_use]
    pub fn with_title_field(mut self, title_field: &str) -> Self {
        self.title_field = Some(title_field.to_owned());
        self
    }
}

/// What a successful [`crate::LivenessHost::apply`] reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEffect {
    /// The metadata version after the change; it rises with every accepted change (ADR-05).
    pub new_version: u64,
}

/// Why a change was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The change names a DocType or field that does not exist.
    UnknownName,
    /// A name is reserved (an engine-managed field or a system column) and cannot be reused.
    ReservedName,
    /// A custom-field name breaks the site-prefix rule.
    BadCustomFieldName,
    /// A runtime DocType's name collides with a reserved or existing DocType.
    NameCollision {
        /// The name that collided.
        name: String,
    },
    /// A property override is refused by the override rules (select values only grow,
    /// `max_chars` only rises, storage properties never; ADR-068 §1). No ADR fixes *which*
    /// wording or property a host reports here, so the refusal carries none: scenarios assert
    /// the refusal and the untouched metadata, never the message.
    BadOverride,
    /// A custom field collides with a standard field of the same name.
    FieldCollision {
        /// The DocType the collision is in.
        doctype: String,
        /// The colliding field name.
        field: String,
    },
    /// A custom field is still referenced (removing it is refused).
    StillReferenced,
    /// The new definitions do not validate as a whole (an app update is atomic).
    InvalidDefinitions,
    /// The field or DocType definition is incomplete or mistyped.
    BadInput {
        /// What is wrong.
        why: String,
    },
}

impl Refusal {
    /// The colliding field name, when the refusal is a field collision. Scenarios use this to
    /// assert that a refused update names what collided.
    #[must_use]
    pub fn collision_field(&self) -> Option<&str> {
        match self {
            Self::FieldCollision { field, .. } => Some(field),
            _ => None,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownName => write!(f, "unknown DocType or field"),
            Self::ReservedName => write!(f, "the name is reserved"),
            Self::BadCustomFieldName => write!(f, "custom field names must be site-prefixed"),
            Self::NameCollision { name } => write!(f, "the DocType name `{name}` is taken"),
            Self::BadOverride => write!(f, "the override is refused by the override rules"),
            Self::FieldCollision { doctype, field } => {
                write!(f, "`{doctype}.{field}` collides with a standard field")
            }
            Self::StillReferenced => write!(f, "the custom field is still referenced"),
            Self::InvalidDefinitions => write!(f, "the new definitions do not validate"),
            Self::BadInput { why } => write!(f, "the change is malformed: {why}"),
        }
    }
}

/// A change the host refused. The host must also say what it refused, so scenarios can assert
/// the colliding name is reported (ADR-26's typed errors, applied to metadata).
#[derive(Clone, Debug)]
pub struct HostError {
    /// Why the change was refused.
    pub refusal: Refusal,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.refusal)
    }
}

impl std::error::Error for HostError {}
