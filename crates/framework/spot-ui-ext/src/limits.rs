//! The closed vocabularies: slots, icons, item kinds, composite limits and versions.

use std::fmt;

/// The kind of contribution an item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemKind {
    /// A button or menu item with exactly one effect.
    Action,
    /// A side panel of blocks.
    Panel,
    /// A home tile backed by a composite component.
    Tile,
    /// A tab in the document's tab strip, showing a panel.
    Tab,
}

impl ItemKind {
    /// The kind's name, as the contract and the resolved JSON spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::Panel => "panel",
            Self::Tile => "tile",
            Self::Tab => "tab",
        }
    }
}

/// One slot of the closed slot list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotSpec {
    /// The slot's family: `shell`, `home`, `doc` or `list`.
    pub family: &'static str,
    /// The slot's name within its family.
    pub name: &'static str,
    /// The only item kind the slot accepts.
    pub accepts: ItemKind,
    /// The most items one app may place in the slot (per DocType for `doc.*` and `list.*`).
    pub max_per_app: usize,
    /// The longest literal label the slot allows.
    pub label_max: usize,
}

impl SlotSpec {
    /// The slot's full id, `family.name`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}.{}", self.family, self.name)
    }

    /// Whether the slot sits on a document or list, so its items name a DocType.
    #[must_use]
    pub fn is_document_slot(&self) -> bool {
        matches!(self.family, "doc" | "list")
    }
}

/// One slot row, so the table above stays a table. Slot ids are stored in two parts and joined
/// on demand; the crate then writes no dotted contract id as a literal.
macro_rules! slot {
    ($family:literal, $name:literal, $accepts:ident, $max:literal, $label:literal) => {
        SlotSpec {
            family: $family,
            name: $name,
            accepts: ItemKind::$accepts,
            max_per_app: $max,
            label_max: $label,
        }
    };
}

/// The closed slot list, in contract order.
pub const SLOTS: &[SlotSpec] = &[
    slot!("shell", "sidebar", Action, 10, 32),
    slot!("shell", "menubar", Action, 10, 32),
    slot!("shell", "statusbar", Action, 3, 24),
    slot!("shell", "user-menu", Action, 5, 32),
    slot!("home", "tiles", Tile, 8, 40),
    slot!("doc", "toolbar", Action, 6, 24),
    slot!("doc", "sidepanel", Panel, 3, 32),
    slot!("doc", "tab", Tab, 3, 24),
    slot!("list", "toolbar", Action, 4, 24),
    slot!("list", "sidepanel", Panel, 3, 32),
];

/// Looks a slot up by its full id.
#[must_use]
pub fn slot(id: &str) -> Option<&'static SlotSpec> {
    SLOTS.iter().find(|spec| spec.id() == id)
}

/// The closed icon token set of `schema/common.json`.
pub const ICON_TOKENS: &[&str] = &[
    "add",
    "edit",
    "delete",
    "save",
    "search",
    "filter",
    "refresh",
    "download",
    "upload",
    "print",
    "share",
    "settings",
    "user",
    "users",
    "home",
    "folder",
    "file",
    "list",
    "grid",
    "calendar",
    "clock",
    "check",
    "close",
    "warning",
    "info",
    "star",
    "flag",
    "tag",
    "link",
    "mail",
    "phone",
    "chart",
    "money",
    "box",
    "truck",
    "cart",
    "wrench",
    "play",
    "pause",
    "archive",
    "lock",
    "unlock",
    "eye",
    "attach",
    "comment",
    "history",
    "more",
    "arrow_left",
    "arrow_right",
    "tasks",
    "project",
    "time",
];

/// Whether `token` is one of the closed icon tokens.
#[must_use]
pub fn is_icon_token(token: &str) -> bool {
    ICON_TOKENS.contains(&token)
}

/// The contract version this crate implements.
pub const CONTRACT_VERSION: &str = "0.1.0";

/// The deepest nesting a component expansion may have (the root node is depth 1).
pub const MAX_COMPONENT_DEPTH: usize = 8;

/// The most nodes a component expansion may have.
pub const MAX_COMPONENT_NODES: usize = 500;

/// The largest `repeat.max` the schema accepts.
pub const MAX_REPEAT: u32 = 50;

/// Whether `value` matches the contract's `name` pattern, `[a-z][a-z0-9_]*`, at most 64 chars.
#[must_use]
pub fn is_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    value.chars().count() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether `value` matches the contract's `app` pattern (the `name` pattern).
#[must_use]
pub fn is_app_name(value: &str) -> bool {
    is_name(value)
}

/// Whether `value` matches the contract's `id` pattern, `app.name`, at most 129 chars.
#[must_use]
pub fn is_id(value: &str) -> bool {
    if value.chars().count() > 129 {
        return false;
    }
    let mut parts = value.split('.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(app), Some(name), None) => is_name(app) && is_name(name),
        _ => false,
    }
}

/// The app part of a namespaced id, when the id is well formed.
#[must_use]
pub fn id_app(value: &str) -> Option<&str> {
    if !is_id(value) {
        return None;
    }
    value.split_once('.').map(|(app, _)| app)
}

/// Whether `value` matches the contract's `origin` pattern, `app:<name>`.
#[must_use]
pub fn is_origin(value: &str) -> bool {
    value
        .strip_prefix("app:")
        .is_some_and(|app| is_name(app) && value.chars().count() <= 68)
}

/// Whether `value` matches the contract's `permission` pattern, `<doctype>:<right>`.
#[must_use]
pub fn is_permission_ref(value: &str) -> bool {
    match value.split_once(':') {
        Some((doctype, right)) => {
            is_name(doctype) && is_name(right) && value.chars().count() <= 129
        }
        None => false,
    }
}

/// A version with three numeric components, ordered the usual way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    pub(crate) major: u64,
    pub(crate) minor: u64,
    pub(crate) patch: u64,
}

impl Version {
    /// Parses `MAJOR.MINOR.PATCH` of ASCII digits, with no sign and no extra text.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = number(parts.next()?)?;
        let minor = number(parts.next()?)?;
        let patch = number(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}
