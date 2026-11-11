//! The draft and its edits: a DocType, its fields and its form layout, changed one pure
//! [`DraftOp`] at a time, with the guard rails that keep a business user's design safe.
//!
//! [`apply_op`] is pure: it takes a [`Draft`] and an op and returns the changed draft, or an
//! [`OpError`] that names what a designer must fix. Nothing is written and no site is touched.
//! [`DraftEditor`] holds a draft and its op log, so undo and redo are exact: the editor can
//! replay the log from the initial draft and reach the same state, which is what proves
//! [`apply_op`] pure.

use spot_meta::{Definition, ENGINE_FIELDS, FieldSpec, FieldType};
use spot_storage_contract::schema::system;

use crate::codes;
use crate::condition::Condition;
use crate::layout::FormLayout;
use crate::naming::is_storage_name;
use crate::ops::{
    add_child_table, add_field, add_link, change_type, check_naming, check_value_text, field_mut,
    field_path, layout_mut, remove_field, rename_field, reorder_field, set_depends_on, set_options,
};
use crate::typechange::SampleStats;

/// Most fields one DocType may hold.
pub const MAX_FIELDS: usize = 200;
/// Most layout sections one form may hold.
pub const MAX_SECTIONS: usize = 50;
/// How deep child tables may nest: one level (a child cannot hold a table, `spot-meta`'s rule).
pub const MAX_CHILD_DEPTH: usize = 1;

/// DocType names the designer may never edit or create.
const PROTECTED_DOCTYPES: [&str; 6] = [
    "custom_doctype",
    "custom_field",
    "property_setter",
    "doctype",
    "docfield",
    "migrate_progress",
];

/// Name prefixes reserved for the platform's own tables.
const PROTECTED_PREFIXES: [&str; 4] = ["spot_", "meta_", "pg_", "sql_"];

/// Where a draft is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    /// Being edited.
    #[default]
    Editing,
    /// Validated and ready to publish.
    Ready,
    /// Published to the site.
    Published,
    /// Discarded; it is never published.
    Discarded,
}

/// A child DocType a draft's table field points at: its definition and its own layout.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildDraft {
    /// The child's definition (`is_child` is set).
    pub doctype_def: Definition,
    /// The child's form layout.
    pub form_layout: FormLayout,
}

impl ChildDraft {
    /// A child DocType named `name` with no fields.
    #[must_use]
    pub fn new(name: &str, label: &str) -> Self {
        let mut definition = Definition::new(name);
        definition.is_child = true;
        definition.label = Some(label.to_owned());
        Self {
            doctype_def: definition,
            form_layout: FormLayout::new(),
        }
    }
}

/// The permissions template a draft chooses for its DocType.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionsTemplate {
    /// The platform default: the creator's role manages it, everyone else is closed out.
    #[default]
    Standard,
    /// Everyone with read access to the site may read, only designers may write.
    ReadOnly,
    /// Only the designer and administrators may read or write.
    Private,
}

impl PermissionsTemplate {
    /// The template's stable name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::ReadOnly => "read_only",
            Self::Private => "private",
        }
    }

    /// The inverse of [`as_str`](PermissionsTemplate::as_str).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "standard" => Some(Self::Standard),
            "read_only" => Some(Self::ReadOnly),
            "private" => Some(Self::Private),
            _ => None,
        }
    }
}

/// A draft workspace for one DocType: the definition being built, its form layout, the child
/// DocTypes its table fields point at, and where it is in its life.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    /// The draft's id.
    pub id: String,
    /// Who owns it.
    pub owner: String,
    /// The DocType being designed.
    pub doctype_def: Definition,
    /// Its form layout.
    pub form_layout: FormLayout,
    /// Where the draft is in its life.
    #[serde(default)]
    pub status: DraftStatus,
    /// The child DocTypes its table fields point at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildDraft>,
    /// The permissions template chosen for the DocType.
    #[serde(default)]
    pub permissions_template: PermissionsTemplate,
}

impl Draft {
    /// A draft for a new DocType named `name`, owned by `owner`.
    #[must_use]
    pub fn new(id: &str, owner: &str, name: &str, label: &str) -> Self {
        let mut definition = Definition::new(name);
        definition.label = Some(label.to_owned());
        definition.app = Some("custom".to_owned());
        definition.module = Some("custom".to_owned());
        Self {
            id: id.to_owned(),
            owner: owner.to_owned(),
            doctype_def: definition,
            form_layout: FormLayout::new(),
            status: DraftStatus::Editing,
            children: Vec::new(),
            permissions_template: PermissionsTemplate::Standard,
        }
    }

    /// A draft for editing an existing DocType, starting from its current definition.
    #[must_use]
    pub fn from_definition(id: &str, owner: &str, definition: Definition) -> Self {
        Self {
            id: id.to_owned(),
            owner: owner.to_owned(),
            doctype_def: definition,
            form_layout: FormLayout::new(),
            status: DraftStatus::Editing,
            children: Vec::new(),
            permissions_template: PermissionsTemplate::Standard,
        }
    }

    /// The draft's DocType name.
    #[must_use]
    pub fn doctype(&self) -> &str {
        &self.doctype_def.name
    }

    /// A child draft by name.
    #[must_use]
    pub fn child(&self, name: &str) -> Option<&ChildDraft> {
        self.children.iter().find(|c| c.doctype_def.name == name)
    }

    /// A child draft by name, mutably.
    pub fn child_mut(&mut self, name: &str) -> Option<&mut ChildDraft> {
        self.children
            .iter_mut()
            .find(|c| c.doctype_def.name == name)
    }

    /// A field by name on the root (`child` is `None`) or a child.
    #[must_use]
    pub fn field(&self, child: Option<&str>, name: &str) -> Option<&FieldSpec> {
        let definition = match child {
            None => &self.doctype_def,
            Some(child) => &self.child(child)?.doctype_def,
        };
        definition.fields.iter().find(|f| f.name == name)
    }

    /// The names of every field of the target, for collision handling.
    #[must_use]
    pub fn field_names(&self, child: Option<&str>) -> Vec<String> {
        let definition = match child {
            None => &self.doctype_def,
            Some(child) => match self.child(child) {
                Some(child) => &child.doctype_def,
                None => return Vec::new(),
            },
        };
        definition.fields.iter().map(|f| f.name.clone()).collect()
    }
}

/// One edit of a draft, applied by [`apply_op`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DraftOp {
    /// Adds a field to the root (`child` is `None`) or a child DocType.
    AddField {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The new field's storage name.
        name: String,
        /// Its label; defaults to the humanised name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Its type.
        ty: FieldType,
        /// Place it right after this field; at the end when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        insert_after: Option<String>,
    },
    /// Renames a field.
    RenameField {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field to rename.
        name: String,
        /// Its new name.
        to: String,
    },
    /// Removes a field. Needs the data-loss confirmation token.
    RemoveField {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field to remove.
        name: String,
        /// The confirmation token [`data_loss_token`] returns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirm: Option<String>,
    },
    /// Changes a field's type, refused when the change would lose or corrupt data.
    ChangeType {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field to retype.
        name: String,
        /// The new type.
        ty: FieldType,
        /// What the existing values look like (supplied by the caller; never read here).
        #[serde(default)]
        stats: SampleStats,
    },
    /// Sets a select field's values.
    SetOptions {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field.
        name: String,
        /// The values, in order.
        options: Vec<String>,
    },
    /// Sets whether a field is required.
    SetRequired {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field.
        name: String,
        /// The new value.
        required: bool,
    },
    /// Sets a field's default.
    SetDefault {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field.
        name: String,
        /// The default text; `None` clears it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// Reorders a field within its definition's field list.
    ReorderField {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field to move.
        name: String,
        /// Place it after this field; at the end when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    },
    /// Adds a child DocType and the table field that points at it.
    AddChildTable {
        /// The table field's name on the root.
        name: String,
        /// Its label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The child DocType and its layout.
        child: Box<ChildDraft>,
        /// Place the table field right after this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        insert_after: Option<String>,
    },
    /// Adds a link field pointing at another DocType.
    AddLink {
        /// The field's name.
        name: String,
        /// Its label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The DocType it links to.
        target: String,
        /// Place it right after this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        insert_after: Option<String>,
    },
    /// Sets the DocType's naming series pattern.
    SetNamingSeries {
        /// The pattern; `None` clears it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        naming: Option<String>,
    },
    /// Sets the permissions template.
    SetPermissionsTemplate {
        /// The template.
        template: PermissionsTemplate,
    },
    /// Adds a section, into a tab or at the top level.
    AddFormSection {
        /// The child DocType whose layout changes, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The section's id.
        id: String,
        /// Its title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Its column count.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<u8>,
        /// The tab to land in; the top level when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    /// Adds a tab at the end of the form.
    AddFormTab {
        /// The child DocType whose layout changes, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The tab's id.
        id: String,
        /// Its title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Sets a section's column count.
    SetSectionColumns {
        /// The child DocType whose layout changes, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The section.
        section: String,
        /// The new column count.
        columns: u8,
    },
    /// Moves a field in the form layout.
    MoveFieldInLayout {
        /// The child DocType whose layout changes, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field to move.
        field: String,
        /// The section to land in.
        section: String,
        /// Anchor it after this field in that section.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    },
    /// Sets a field's depends-on rule from a friendly condition.
    SetDependsOn {
        /// The child DocType, or `None` for the root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<String>,
        /// The field.
        name: String,
        /// The condition; `None` clears it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        condition: Option<Condition>,
    },
}

impl DraftOp {
    /// The op's stable name, for logs and audit.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::AddField { .. } => "add_field",
            Self::RenameField { .. } => "rename_field",
            Self::RemoveField { .. } => "remove_field",
            Self::ChangeType { .. } => "change_type",
            Self::SetOptions { .. } => "set_options",
            Self::SetRequired { .. } => "set_required",
            Self::SetDefault { .. } => "set_default",
            Self::ReorderField { .. } => "reorder_field",
            Self::AddChildTable { .. } => "add_child_table",
            Self::AddLink { .. } => "add_link",
            Self::SetNamingSeries { .. } => "set_naming_series",
            Self::SetPermissionsTemplate { .. } => "set_permissions_template",
            Self::AddFormSection { .. } => "add_form_section",
            Self::AddFormTab { .. } => "add_form_tab",
            Self::SetSectionColumns { .. } => "set_section_columns",
            Self::MoveFieldInLayout { .. } => "move_field_in_layout",
            Self::SetDependsOn { .. } => "set_depends_on",
        }
    }
}

/// The explicit confirmation token a field removal needs: `delete <doctype>.<field>`.
#[must_use]
pub fn data_loss_token(doctype: &str, field: &str) -> String {
    format!("delete {doctype}.{field}")
}

/// Why an op could not be applied. Every variant carries a stable `nocode_model.*` code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpError {
    /// The draft has no such field.
    #[error("`{path}` is not a field of this draft")]
    UnknownField {
        /// The path the op named.
        path: String,
    },
    /// The draft has no such child DocType.
    #[error("`{name}` is not a child DocType of this draft")]
    UnknownChild {
        /// The child named.
        name: String,
    },
    /// Two fields share a name.
    #[error("`{path}` already exists")]
    DuplicateField {
        /// The path that collided.
        path: String,
    },
    /// A name is reserved by the platform.
    #[error("`{name}` is reserved by the platform")]
    ReservedName {
        /// The reserved name.
        name: String,
    },
    /// A name is not a valid storage name.
    #[error("`{name}` is not a valid name")]
    InvalidName {
        /// The rejected name.
        name: String,
    },
    /// The DocType would hold more fields than allowed.
    #[error("a DocType may hold at most {limit} fields")]
    TooManyFields {
        /// The limit.
        limit: usize,
    },
    /// The form or child nesting is deeper than allowed.
    #[error("the draft is deeper than the platform allows")]
    TooDeep,
    /// A protected system DocType cannot be edited.
    #[error("`{name}` is a protected system DocType")]
    ProtectedDoctype {
        /// The protected name.
        name: String,
    },
    /// Removing a field needs the data-loss confirmation token.
    #[error("removing `{path}` needs the data-loss confirmation token")]
    NeedsDataLossConfirmation {
        /// The field path.
        path: String,
    },
    /// A value carries something that looks like a script.
    #[error("`{path}` may not carry script")]
    ScriptNotAllowed {
        /// Where the script was found.
        path: String,
    },
    /// A value carries something that looks like raw SQL.
    #[error("`{path}` may not carry a raw SQL statement")]
    RawSqlNotAllowed {
        /// Where the SQL was found.
        path: String,
    },
    /// A type change would lose or corrupt data.
    #[error("the type change on `{path}` is refused: {reason}")]
    TypeChangeRefused {
        /// The field path.
        path: String,
        /// Why it is refused.
        reason: String,
    },
    /// A friendly condition did not compile.
    #[error("the condition on `{path}` is not valid: {message}")]
    BadCondition {
        /// The field path.
        path: String,
        /// What went wrong.
        message: String,
    },
    /// A select field's options are empty or repeated.
    #[error("`{path}` has invalid options: {why}")]
    BadOptions {
        /// The field path.
        path: String,
        /// What is wrong.
        why: String,
    },
    /// The layout names a section it does not declare.
    #[error("section `{id}` does not exist")]
    UnknownSection {
        /// The section id.
        id: String,
    },
    /// The layout names a tab it does not declare.
    #[error("tab `{id}` does not exist")]
    UnknownTab {
        /// The tab id.
        id: String,
    },
    /// Two sections share an id.
    #[error("section `{id}` already exists")]
    DuplicateSection {
        /// The section id.
        id: String,
    },
    /// Two tabs share an id.
    #[error("tab `{id}` already exists")]
    DuplicateTab {
        /// The tab id.
        id: String,
    },
    /// The layout cannot take the shape asked of it.
    #[error("the layout is not valid: {why}")]
    BadLayout {
        /// Why the layout cannot take the shape.
        why: String,
    },
    /// A link or child table names a target that cannot be used.
    #[error("`{name}` cannot be used as a target")]
    BadTarget {
        /// The target named.
        name: String,
    },
    /// The naming series pattern is not accepted.
    #[error("`{naming}` is not a valid naming pattern")]
    BadNaming {
        /// The pattern.
        naming: String,
    },
    /// The op is not supported for the target it names.
    #[error("{what}")]
    Unsupported {
        /// What is not supported.
        what: String,
    },
    /// The field is still referred to by another field.
    #[error("`{name}` is still referred to by another field")]
    Referenced {
        /// The field that is still referred to.
        name: String,
    },
}

impl OpError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownField { .. } => codes::UNKNOWN_FIELD,
            Self::UnknownChild { .. } => codes::UNKNOWN_CHILD,
            Self::DuplicateField { .. } => codes::DUPLICATE_FIELD,
            Self::ReservedName { .. } => codes::RESERVED_NAME,
            Self::InvalidName { .. } => codes::BAD_NAME,
            Self::TooManyFields { .. } => codes::MAX_FIELDS,
            Self::TooDeep => codes::MAX_DEPTH,
            Self::ProtectedDoctype { .. } => codes::PROTECTED_DOCTYPE,
            Self::NeedsDataLossConfirmation { .. } => codes::NEEDS_DATA_LOSS_CONFIRMATION,
            Self::ScriptNotAllowed { .. } => codes::NO_SCRIPT,
            Self::RawSqlNotAllowed { .. } => codes::NO_SQL,
            Self::TypeChangeRefused { .. } => codes::TYPE_CHANGE_REFUSED,
            Self::BadCondition { .. } => codes::BAD_CONDITION,
            Self::BadOptions { .. } => codes::BAD_OPTIONS,
            Self::UnknownSection { .. } => codes::UNKNOWN_SECTION,
            Self::UnknownTab { .. } => codes::UNKNOWN_TAB,
            Self::DuplicateSection { .. } => codes::DUPLICATE_SECTION,
            Self::DuplicateTab { .. } => codes::DUPLICATE_TAB,
            Self::BadLayout { .. } => codes::BAD_LAYOUT,
            Self::BadTarget { .. } => codes::BAD_TARGET,
            Self::BadNaming { .. } => codes::BAD_NAMING,
            Self::Unsupported { .. } => codes::UNSUPPORTED_OP,
            Self::Referenced { .. } => codes::REFERENCED,
        }
    }

    /// The element path the error is about, when it names one.
    #[must_use]
    pub fn path(&self) -> Option<String> {
        match self {
            Self::UnknownField { path }
            | Self::DuplicateField { path }
            | Self::NeedsDataLossConfirmation { path }
            | Self::ScriptNotAllowed { path }
            | Self::RawSqlNotAllowed { path }
            | Self::TypeChangeRefused { path, .. }
            | Self::BadCondition { path, .. }
            | Self::BadOptions { path, .. } => Some(path.clone()),
            _ => None,
        }
    }
}

/// Whether `name` may not be a field of a DocType.
#[must_use]
pub fn is_reserved_field(name: &str) -> bool {
    system::ALL.contains(&name) || ENGINE_FIELDS.contains(&name)
}

/// Whether `name` is a protected system DocType.
#[must_use]
pub fn is_protected_doctype(name: &str) -> bool {
    PROTECTED_DOCTYPES.contains(&name) || PROTECTED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Whether `text` looks like executable script. The designer's values are data, never code
/// (the platform runs no script from a design), so a marker is refused rather than stored.
#[must_use]
pub fn looks_like_script(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["<script", "javascript:", "eval(", "exec(", "`+"]
        .iter()
        .any(|m| lower.contains(m))
}

/// Whether `text` looks like a raw SQL statement. ADR-45 keeps SQL out of everything but the
/// storage crates, so a design value may never carry one.
#[must_use]
pub fn looks_like_sql(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    [
        "select ",
        "insert ",
        "update ",
        "delete ",
        "drop ",
        "alter ",
        "create ",
        "grant ",
        "truncate ",
    ]
    .iter()
    .any(|m| lower.starts_with(m))
}

/// Applies one op to `draft`, returning the changed draft. Pure: `draft` is untouched, and an
/// error changes nothing.
///
/// # Errors
///
/// An [`OpError`] naming the guard rail or precondition that failed.
pub fn apply_op(draft: &Draft, op: &DraftOp) -> Result<Draft, OpError> {
    if is_protected_doctype(draft.doctype()) {
        return Err(OpError::ProtectedDoctype {
            name: draft.doctype().to_owned(),
        });
    }
    let mut next = draft.clone();
    match op {
        DraftOp::AddField {
            child,
            name,
            label,
            ty,
            insert_after,
        } => add_field(
            &mut next,
            child.as_deref(),
            name,
            label.clone(),
            *ty,
            insert_after.as_deref(),
        )?,
        DraftOp::RenameField { child, name, to } => {
            rename_field(&mut next, child.as_deref(), name, to)?;
        }
        DraftOp::RemoveField {
            child,
            name,
            confirm,
        } => remove_field(&mut next, child.as_deref(), name, confirm.as_deref())?,
        DraftOp::ChangeType {
            child,
            name,
            ty,
            stats,
        } => change_type(&mut next, child.as_deref(), name, *ty, stats)?,
        DraftOp::SetOptions {
            child,
            name,
            options,
        } => set_options(&mut next, child.as_deref(), name, options)?,
        DraftOp::SetRequired {
            child,
            name,
            required,
        } => {
            field_mut(&mut next, child.as_deref(), name)?.required = *required;
        }
        DraftOp::SetDefault {
            child,
            name,
            default,
        } => {
            let path = field_path(&next, child.as_deref(), name);
            if let Some(text) = default {
                check_value_text(text, &path)?;
            }
            field_mut(&mut next, child.as_deref(), name)?.default = default.clone();
        }
        DraftOp::ReorderField { child, name, after } => {
            reorder_field(&mut next, child.as_deref(), name, after.as_deref())?;
        }
        DraftOp::AddChildTable {
            name,
            label,
            child,
            insert_after,
        } => add_child_table(
            &mut next,
            name,
            label.clone(),
            child,
            insert_after.as_deref(),
        )?,
        DraftOp::AddLink {
            name,
            label,
            target,
            insert_after,
        } => add_link(
            &mut next,
            name,
            label.clone(),
            target,
            insert_after.as_deref(),
        )?,
        DraftOp::SetNamingSeries { naming } => {
            if let Some(pattern) = naming {
                check_naming(pattern)?;
            }
            next.doctype_def.naming = naming.clone();
        }
        DraftOp::SetPermissionsTemplate { template } => {
            next.permissions_template = *template;
        }
        DraftOp::AddFormSection {
            child,
            id,
            title,
            columns,
            tab,
        } => {
            let layout = layout_mut(&mut next, child.as_deref())?;
            if layout.all_sections().len() >= MAX_SECTIONS {
                return Err(OpError::TooDeep);
            }
            if !is_storage_name(id) {
                return Err(OpError::InvalidName { name: id.clone() });
            }
            if let Some(tab_id) = tab
                && !layout.has_tab(tab_id)
            {
                return Err(OpError::UnknownTab { id: tab_id.clone() });
            }
            let section = spot_views::viewdef::Section {
                id: id.clone(),
                title: title.clone(),
                columns: *columns,
                items: Vec::new(),
            };
            let has_section = layout.has_section(id);
            layout.add_section(section, tab.as_deref()).map_err(|why| {
                if has_section {
                    OpError::DuplicateSection { id: id.clone() }
                } else {
                    OpError::BadLayout { why }
                }
            })?;
        }
        DraftOp::AddFormTab { child, id, title } => {
            let layout = layout_mut(&mut next, child.as_deref())?;
            if layout.all_sections().len() >= MAX_SECTIONS {
                return Err(OpError::TooDeep);
            }
            if !is_storage_name(id) {
                return Err(OpError::InvalidName { name: id.clone() });
            }
            let tab = spot_views::Tab {
                id: id.clone(),
                title: title.clone(),
                sections: Vec::new(),
            };
            let has_tab = layout.has_tab(id);
            layout.add_tab(tab).map_err(|why| {
                if has_tab {
                    OpError::DuplicateTab { id: id.clone() }
                } else {
                    OpError::BadLayout { why }
                }
            })?;
        }
        DraftOp::SetSectionColumns {
            child,
            section,
            columns,
        } => {
            let layout = layout_mut(&mut next, child.as_deref())?;
            if !layout.has_section(section) {
                return Err(OpError::UnknownSection {
                    id: section.clone(),
                });
            }
            layout
                .set_section_columns(section, *columns)
                .map_err(|why| OpError::BadLayout { why })?;
        }
        DraftOp::MoveFieldInLayout {
            child,
            field,
            section,
            after,
        } => {
            let layout = layout_mut(&mut next, child.as_deref())?;
            if !layout.has_section(section) {
                return Err(OpError::UnknownSection {
                    id: section.clone(),
                });
            }
            layout
                .move_field(field, section, after.as_deref())
                .map_err(|why| OpError::BadLayout { why })?;
        }
        DraftOp::SetDependsOn {
            child,
            name,
            condition,
        } => set_depends_on(&mut next, child.as_deref(), name, condition.as_ref())?,
    }
    Ok(next)
}

/// A draft and its op log: undo and redo move a cursor over the states the log produced.
///
/// The editor keeps every state, so undo is exact and redo replays the same op;
/// [`DraftEditor::replayed`] recomputes the current state from the initial draft and the log,
/// which is what proves [`apply_op`] pure.
#[derive(Clone, Debug)]
pub struct DraftEditor {
    initial: Draft,
    states: Vec<Draft>,
    ops: Vec<DraftOp>,
    cursor: usize,
}

impl DraftEditor {
    /// An editor over `draft`, with no edits.
    #[must_use]
    pub fn new(draft: Draft) -> Self {
        Self {
            states: vec![draft.clone()],
            initial: draft,
            ops: Vec::new(),
            cursor: 0,
        }
    }

    /// The current draft.
    #[must_use]
    pub fn current(&self) -> &Draft {
        self.states.get(self.cursor).unwrap_or(&self.initial)
    }

    /// Applies an op, dropping any redoable future.
    ///
    /// # Errors
    ///
    /// The [`OpError`] the op raised; nothing changes.
    pub fn apply(&mut self, op: DraftOp) -> Result<&Draft, OpError> {
        let next = apply_op(self.current(), &op)?;
        self.states.truncate(self.cursor + 1);
        self.ops.truncate(self.cursor);
        self.states.push(next);
        self.ops.push(op);
        self.cursor += 1;
        Ok(self.current())
    }

    /// Whether undo would move back.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    /// Whether redo would move forward.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.cursor < self.ops.len()
    }

    /// Moves back one op. Returns whether it moved.
    pub fn undo(&mut self) -> bool {
        if !self.can_undo() {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Moves forward one op. Returns whether it moved.
    pub fn redo(&mut self) -> bool {
        if !self.can_redo() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// The ops applied so far, in order (the command log).
    #[must_use]
    pub fn log(&self) -> &[DraftOp] {
        &self.ops[..self.cursor]
    }

    /// How many ops the log holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cursor
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cursor == 0
    }

    /// The op log as data, for persistence and tests.
    #[must_use]
    pub fn ops(&self) -> &[DraftOp] {
        &self.ops
    }

    /// The current draft recomputed from the initial draft and the log. Equal to
    /// [`current`](DraftEditor::current) whenever [`apply_op`] is pure.
    ///
    /// # Errors
    ///
    /// The [`OpError`] a logged op raises on replay (which would mean the log was tampered with).
    pub fn replayed(&self) -> Result<Draft, OpError> {
        let mut draft = self.initial.clone();
        for op in self.log() {
            draft = apply_op(&draft, op)?;
        }
        Ok(draft)
    }
}
