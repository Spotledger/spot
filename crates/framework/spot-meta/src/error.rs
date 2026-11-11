//! Metadata errors. Validation reports every problem at once, each naming the DocType and,
//! where there is one, the field.

use std::fmt;

/// What went wrong, for callers and tests that match on the cause rather than the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaErrorKind {
    /// A definition file could not be read.
    Io,
    /// A definition file is not valid TOML, or has an unknown or mistyped key.
    Parse,
    /// A file name does not match the DocType it defines.
    FileName,
    /// A name is not a storage name.
    InvalidName,
    /// A name is reserved: a system column or an engine-managed field.
    ReservedName,
    /// Two DocTypes or two fields share a name.
    Duplicate,
    /// A link or table points at a DocType that does not exist, or a DocType cannot be targeted.
    UnknownTarget,
    /// A table or child DocType is set up wrongly.
    BadTable,
    /// The title field is missing or not text-like.
    BadTitleField,
    /// A property is missing, not allowed for the field's type, or out of range.
    BadProperty,
    /// A default does not parse for the field's type.
    BadDefault,
    /// A select field has no options, or empty or repeated ones.
    BadOptions,
    /// An index names a field that does not exist, or one that cannot be indexed.
    BadIndex,
    /// A custom field breaks the custom-field rules.
    BadCustomField,
    /// A property override is refused.
    BadOverride,
    /// A lookup names a seam this build does not know.
    UnknownSeam,
    /// A lookup rule (`when`) waits for the shared expression language (WP-UI-11).
    NotYetSupported,
    /// A customisation names a DocType or field that does not exist.
    UnknownName,
    /// Compiling to the storage catalog failed (a bug in the metadata layer if validation passed).
    Compile,
    /// A geolocation format is on a field that is not a point (ADR-087 section 4).
    GeolocationNotAPoint,
    /// An exclusion declaration is wrong (ADR-087 section 5).
    BadExclusion,
    /// A search declaration is wrong (ADR-087 section 6).
    BadSearch,
    /// A search declaration names a protected field.
    SearchFieldProtected,
    /// A range field or its element type is wrong.
    BadRange,
    /// The configured backend cannot do what a declaration needs.
    BackendUnsupported,
}

impl MetaErrorKind {
    /// The catalogue code of the declaration kinds ADR-087 adds (WP-MET-05.T3), or `None` for a
    /// kind that is reported by its message only. Nothing emits these yet; WP-MET-05.T1 does.
    #[must_use]
    pub const fn code(self) -> Option<&'static str> {
        match self {
            Self::GeolocationNotAPoint => Some("meta.geolocation_not_a_point"),
            Self::BadExclusion => Some("meta.bad_exclusion"),
            Self::BadSearch => Some("meta.bad_search"),
            Self::SearchFieldProtected => Some("meta.search_field_protected"),
            Self::BadRange => Some("meta.bad_range"),
            Self::BackendUnsupported => Some("meta.backend_unsupported"),
            _ => None,
        }
    }
}

/// One metadata problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaError {
    /// The cause.
    pub kind: MetaErrorKind,
    /// The definition file, when the problem comes from one.
    pub file: Option<String>,
    /// The DocType the problem is about.
    pub doctype: Option<String>,
    /// The field the problem is about.
    pub field: Option<String>,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl MetaError {
    /// An error with only a kind and a message.
    pub fn new(kind: MetaErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            file: None,
            doctype: None,
            field: None,
            message: message.into(),
        }
    }

    /// Names the DocType.
    #[must_use]
    pub fn in_doctype(mut self, doctype: &str) -> Self {
        self.doctype = Some(doctype.to_owned());
        self
    }

    /// Names the field.
    #[must_use]
    pub fn in_field(mut self, field: &str) -> Self {
        self.field = Some(field.to_owned());
        self
    }

    /// Names the file.
    #[must_use]
    pub fn in_file(mut self, file: &str) -> Self {
        self.file = Some(file.to_owned());
        self
    }
}

impl fmt::Display for MetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{file}: ")?;
        }
        match (&self.doctype, &self.field) {
            (Some(d), Some(fl)) => write!(f, "`{d}.{fl}`: ")?,
            (Some(d), None) => write!(f, "`{d}`: ")?,
            (None, Some(fl)) => write!(f, "field `{fl}`: ")?,
            (None, None) => {}
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for MetaError {}
