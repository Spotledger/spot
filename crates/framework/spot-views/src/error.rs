//! View errors: a stable `view.*` code, a position where the TOML has one, every problem
//! reported at once.

use std::fmt;

/// A line and column (1-based) in the view or layer TOML source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    /// The line, starting at 1.
    pub line: usize,
    /// The column, starting at 1.
    pub column: usize,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// A position computed from a byte offset in `text`.
#[must_use]
pub fn position_of(text: &str, offset: usize) -> Position {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut line = 1;
    let mut column = 1;
    for byte in &bytes[..end] {
        match byte {
            b'\n' => {
                line += 1;
                column = 1;
            }
            _ => column += 1,
        }
    }
    Position { line, column }
}

/// What went wrong, carrying its stable code. Callers match on the kind, not the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewErrorKind {
    /// The view or layer file is not valid TOML, or has an unknown or mistyped key.
    Parse,
    /// A name (section, tab, item group or action) must be a lowercase storage id.
    BadId,
    /// The view names a field the DocType does not have.
    UnknownField,
    /// The view binds an unbound control kind the model does not know.
    UnknownControl,
    /// A diff or item targets a section that is not in the view.
    UnknownSection,
    /// A diff targets a tab that is not in the view.
    UnknownTab,
    /// A move anchors to a field that is not in the target section or tab.
    UnknownAnchor,
    /// Two entries share a name: a section id, tab id, group id or action name.
    Duplicate,
    /// The view's parts do not fit its kind (columns on a form, tabs on a list, ...).
    BadShape,
    /// The layer operation is not valid for the view kind it applies to.
    BadOp,
    /// A view hides a field that is `required` and has no `default` (ADR-068 §7.5).
    HiddenRequired,
}

impl ViewErrorKind {
    /// The stable dotted code.
    pub fn code(self) -> &'static str {
        match self {
            ViewErrorKind::Parse => "view.parse",
            ViewErrorKind::BadId => "view.bad_id",
            ViewErrorKind::UnknownField => "view.unknown_field",
            ViewErrorKind::UnknownControl => "view.unknown_control",
            ViewErrorKind::UnknownSection => "view.unknown_section",
            ViewErrorKind::UnknownTab => "view.unknown_tab",
            ViewErrorKind::UnknownAnchor => "view.unknown_anchor",
            ViewErrorKind::Duplicate => "view.duplicate",
            ViewErrorKind::BadShape => "view.bad_shape",
            ViewErrorKind::BadOp => "view.bad_op",
            ViewErrorKind::HiddenRequired => "view.hidden_required",
        }
    }
}

/// One view problem.
///
/// Like [`spot_meta::MetaError`]: the cause kind, an optional position in the source, the
/// DocType, the field, and one sentence of message. Display prefixes the code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewError {
    /// The cause, with the stable code.
    pub kind: ViewErrorKind,
    /// The source file, when the problem comes from one.
    pub file: Option<String>,
    /// The position in the source, when there is one.
    pub position: Option<Position>,
    /// The DocType the problem is about.
    pub doctype: Option<String>,
    /// The field the problem is about.
    pub field: Option<String>,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl ViewError {
    /// An error from `kind` and a message.
    pub fn new(kind: ViewErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            file: None,
            position: None,
            doctype: None,
            field: None,
            message: message.into(),
        }
    }

    /// Names the source file.
    #[must_use]
    pub fn in_file(mut self, file: &str) -> Self {
        self.file = Some(file.to_owned());
        self
    }

    /// Stamps the position computed from a byte offset in `text` — the same source the error
    /// came from.
    #[must_use]
    pub fn at_offset(mut self, text: &str, offset: usize) -> Self {
        self.position = Some(position_of(text, offset));
        self
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
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind.code())?;
        if let Some(position) = &self.position {
            write!(f, " at {position}")?;
        }
        write!(f, ": ")?;
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

impl std::error::Error for ViewError {}

/// Whether `name` is a lowercase storage-style id (`[a-z][a-z0-9_]*`) — the shape section,
/// tab, group and action names must keep so ids stay stable across TOML and the renderer.
pub fn is_id(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
