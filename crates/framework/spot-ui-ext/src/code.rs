//! The stable `uiext.*` codes and the finding type they travel in.

use std::fmt;

/// Every stable code of `contracts/ui-ext/errors.md`, plus the three codes `resolve` and the parser need.
///
/// The strings are the contract: callers compare [`UiExtError::code`] against them, never the
/// message. `errors.md` documents 26 codes; `uiext.extension_without_dependency` is named by the
/// task brief and `uiext.dependency_cycle` covers the order `resolve` cannot build. Both are
/// recorded in the task report as gaps in the contract's table.
pub mod codes {
    /// The document does not fit the contract schema and no other code names the problem.
    pub const SCHEMA: &str = "uiext.schema";
    /// `slot` is not one of the closed slots of the contract.
    pub const UNKNOWN_SLOT: &str = "uiext.unknown_slot";
    /// The slot does not accept the item's kind.
    pub const SLOT_KIND_MISMATCH: &str = "uiext.slot_kind_mismatch";
    /// An item in a `doc.*` or `list.*` slot names no DocType.
    pub const DOCTYPE_REQUIRED: &str = "uiext.doctype_required";
    /// The app contributes more items to a slot and DocType than the slot allows.
    pub const SLOT_LIMIT: &str = "uiext.slot_limit";
    /// A literal label is longer than the slot allows.
    pub const LABEL_TOO_LONG: &str = "uiext.label_too_long";
    /// An icon is not a token of the closed icon set.
    pub const UNKNOWN_ICON: &str = "uiext.unknown_icon";
    /// An action does not have exactly one effect (`call`, `open`, `navigate_url`).
    pub const EFFECT_INVALID: &str = "uiext.effect_invalid";
    /// `navigate_url` is not an absolute path, or has a scheme or host.
    pub const EXTERNAL_URL: &str = "uiext.external_url";
    /// `navigate_url` is not covered by a route the app's manifest declares.
    pub const UNKNOWN_ROUTE: &str = "uiext.unknown_route";
    /// `call` does not name a method the manifest declares with that shape.
    pub const UNKNOWN_METHOD: &str = "uiext.unknown_method";
    /// A destination is not in the registry.
    pub const UNKNOWN_DESTINATION: &str = "uiext.unknown_destination";
    /// A permission reference does not name an existing `<doctype>:<right>`.
    pub const UNKNOWN_PERMISSION: &str = "uiext.unknown_permission";
    /// An expression does not parse, or reads a field the scope does not have.
    pub const EXPRESSION_INVALID: &str = "uiext.expression_invalid";
    /// A `text` block is outside the restricted markdown subset.
    pub const RAW_HTML: &str = "uiext.raw_html";
    /// A data block names no query.
    pub const QUERY_MISSING: &str = "uiext.query_missing";
    /// A block, tile prop or query reference names a query the manifest does not declare.
    pub const UNKNOWN_QUERY: &str = "uiext.unknown_query";
    /// A `table` names an aggregate query, or a `stat` a rows query.
    pub const QUERY_KIND: &str = "uiext.query_kind";
    /// A tab names a panel the app does not contribute.
    pub const UNKNOWN_PANEL: &str = "uiext.unknown_panel";
    /// A tile or a `use` node names a component the app does not contribute.
    pub const UNKNOWN_COMPONENT: &str = "uiext.unknown_component";
    /// A control's `on_click` names an action the app does not contribute.
    pub const UNKNOWN_ACTION: &str = "uiext.unknown_action";
    /// An item's origin or id namespace is not the contributing app.
    pub const ORIGIN_MISMATCH: &str = "uiext.origin_mismatch";
    /// Two items share an id.
    pub const DUPLICATE_ID: &str = "uiext.duplicate_id";
    /// Components reference each other in a cycle.
    pub const COMPONENT_CYCLE: &str = "uiext.component_cycle";
    /// An expansion nests deeper than eight nodes.
    pub const COMPONENT_TOO_DEEP: &str = "uiext.component_too_deep";
    /// An expansion has more than 500 nodes.
    pub const COMPONENT_TOO_LARGE: &str = "uiext.component_too_large";
    /// An app refers to another app's item without declaring the dependency (ADR-069 §2.3).
    pub const EXTENSION_WITHOUT_DEPENDENCY: &str = "uiext.extension_without_dependency";
    /// A document is larger than the size cap (`MAX_DOCUMENT_BYTES`), so it is not parsed.
    pub const DOCUMENT_TOO_LARGE: &str = "uiext.document_too_large";
    /// The apps' declared dependencies form a cycle, so no dependency order exists.
    pub const DEPENDENCY_CYCLE: &str = "uiext.dependency_cycle";
}

/// One reason a document cannot be installed, or a set of apps cannot be resolved.
///
/// The finding names the app, the file of the app's contribution, the item id (when the problem
/// belongs to one) and a JSON pointer into the document. A parse failure also carries the line and
/// column the parser reported. The code is the contract; the message is for the app author.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UiExtError {
    /// The app whose contribution failed, when the check runs for one.
    pub app: Option<String>,
    /// The contribution file, as the caller named it.
    pub file: String,
    /// A JSON pointer into the parsed document, such as `/actions/0/label`.
    pub pointer: String,
    /// One of [`codes`].
    pub code: &'static str,
    /// The item id the finding belongs to, when it belongs to one.
    pub id: Option<String>,
    /// What is wrong, naming the values involved.
    pub message: String,
    /// The 1-based line a parser stopped at, when the problem is a parse failure.
    pub line: Option<usize>,
    /// The 1-based column a parser stopped at, when the problem is a parse failure.
    pub column: Option<usize>,
}

impl UiExtError {
    /// A finding with a code, the file and the pointer (or the empty string when the whole file
    /// is affected).
    #[must_use]
    pub fn new(
        code: &'static str,
        file: impl Into<String>,
        pointer: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            app: None,
            file: file.into(),
            pointer: pointer.into(),
            code,
            id: None,
            message: message.into(),
            line: None,
            column: None,
        }
    }

    /// Names the app the contribution belongs to.
    #[must_use]
    pub fn with_app(mut self, app: &str) -> Self {
        self.app = Some(app.to_owned());
        self
    }

    /// Names the item the finding belongs to.
    #[must_use]
    pub fn with_id(mut self, id: &str) -> Self {
        self.id = Some(id.to_owned());
        self
    }

    /// Records where a parser stopped.
    #[must_use]
    pub fn with_position(mut self, line: usize, column: usize) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }
}

impl fmt::Display for UiExtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(app) = &self.app {
            write!(f, "{app}: ")?;
        }
        write!(f, "{}", self.file)?;
        if !self.pointer.is_empty() {
            write!(f, "{}", self.pointer)?;
        }
        write!(f, ": {}: {}", self.code, self.message)?;
        if let Some(id) = &self.id {
            write!(f, " (item {id})")?;
        }
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(f, " at line {line}, column {column}")?;
        }
        Ok(())
    }
}

impl std::error::Error for UiExtError {}

/// Orders findings the same way whatever order the checks met them in.
pub(crate) fn sort_findings(findings: &mut [UiExtError]) {
    findings.sort();
}
