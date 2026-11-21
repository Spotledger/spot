//! The view definition: one per DocType and kind, TOML in and out.
//!
//! A view holds groups, tabs, columns, the order of fields and unbound controls within them,
//! grid columns, density and actions (ADR-068 §7.1). Layout is never a field: a section, tab,
//! heading or divider has no storage and no entry in the DocType.
//!
//! ```toml
//! doctype = "sales_invoice"
//! kind = "form"
//!
//! [[tabs]]
//! id = "totals"
//! [[tabs.sections]]
//! id = "totals_grid"
//! [[tabs.sections.items]]
//! type = "field"
//! field = "invoice_items"
//!
//! [[sections]]           # a tabless form carries its sections at the top
//! id = "general"
//! [[sections.items]]
//! type = "field"
//! field = "customer"
//! ```
//!
//! Keys serialise in declaration order (stable key order); `#[serde(deny_unknown_fields)]`
//! turns a mistyped key into `view.parse` naming its position.

use serde::{Deserialize, Serialize};

use crate::error::{ViewError, ViewErrorKind};

/// Which face of a DocType the view defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    /// The document form: tabs, sections, items, actions.
    Form,
    /// The list: columns and a default sort.
    List,
    /// The child-table grid: its columns.
    Grid,
}

/// How compact the form is laid out — a hint, never a rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// Compact lists and narrow spacing.
    Compact,
    /// The normal spacing.
    Comfortable,
    /// Wide spacing for touch-heavy forms.
    Spacious,
}

/// One line of an ordered view body: a field, an unbound control, or a nested group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Item {
    /// A field of the DocType, with optional presentation hints.
    Field {
        /// The field's storage name.
        field: String,
        /// Replaces the field's label in this view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The catalog name of the control that renders the field (ADR-068 §5); validated
        /// against the catalog when it exists, not here.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<String>,
        /// Switches the field's lookup to this seam (ADR-068 §3.4, §7.6).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lookup: Option<String>,
    },
    /// An unbound control: `heading`, `divider`, `spacer` or `help` — the render-neutral set
    /// this crate owns (the catalog itself is not this task, ADR-068 §5).
    Control {
        /// The catalog name of the unbound control.
        control: String,
        /// The control's text (a heading's title, a help line).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// A nested group of items.
    Group(Box<Group>),
}

/// A nested group of items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// The group's stable id.
    pub id: String,
    /// The group's heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The grouped items, in order.
    pub items: Vec<Item>,
}

/// An ordered section of a form: the layout unit a set of items belongs to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    /// The section's stable id, unique across the view.
    pub id: String,
    /// The section's heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// How many columns the section lays out in; 1 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u8>,
    /// The section's items, in order.
    pub items: Vec<Item>,
}

/// An ordered tab of a form, carrying its own sections.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    /// The tab's stable id, unique across the view.
    pub id: String,
    /// The tab's caption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The tab's sections, in order.
    pub sections: Vec<Section>,
}

/// One column of a list or grid: the field and an optional label override.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    /// The field's storage name.
    pub field: String,
    /// Replaces the column's label in this view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The default sort of a list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    /// The field the list sorts by.
    pub field: String,
    /// Largest first; defaults to ascending.
    #[serde(default, skip_serializing_if = "crate::viewdef::is_false")]
    pub descending: bool,
}

/// A named action on a view, with its visibility condition kept as an opaque expression
/// string — evaluated later by `spot-expr`, never here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    /// The action's stable name, unique across the view.
    pub name: String,
    /// The action's label.
    pub label: String,
    /// The visibility condition as an opaque expression; shown always when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// One view: its DocType, its kind, and the kind-gated layout parts.
///
/// `role` is set only on a **role view**: whole views are chosen per user by role priority
/// (ADR-068 §7.2) and are never merged; the merged diff layers live in
/// [`ViewLayer`](crate::ViewLayer).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewDef {
    /// The DocType the view renders.
    pub doctype: String,
    /// The view's kind.
    pub kind: ViewKind,
    /// The role this view is for; unset on standard, site and step views.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The view's title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The density hint (forms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density: Option<Density>,
    /// The form's tabs, in order. A form carries tabs or top-level sections, not both.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<Tab>,
    /// The form's top-level sections, in order (a tabless form).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
    /// The list's or grid's columns, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<Column>,
    /// The list's default sort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Sort>,
    /// The actions, in order, any kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

impl ViewDef {
    /// Parses one view. `source` names the file in errors; every semantic problem is
    /// reported at once, each with a position where the TOML has one and a stable
    /// `view.*` code. A syntax error keeps only that first error — the parser cannot see
    /// past it.
    pub fn parse(source: &str, text: &str) -> Result<Self, Vec<ViewError>> {
        let parsed: Self = match toml::from_str(text) {
            Ok(parsed) => parsed,
            Err(e) => {
                let mut error = ViewError::new(
                    ViewErrorKind::Parse,
                    format!("invalid TOML: {}", e.message().trim_end()),
                );
                if let Some(span) = e.span() {
                    error = error.at_offset(text, span.start);
                }
                return Err(vec![error.in_file(source)]);
            }
        };
        let mut errors = Vec::new();
        parsed.check_shape(source, &mut errors);
        if errors.is_empty() {
            Ok(parsed)
        } else {
            Err(errors)
        }
    }

    /// Writes the view as TOML, keys in declaration order. The error is boxed because a
    /// `ViewError` is large and this result crosses the public API.
    pub fn to_toml(&self) -> Result<String, Box<ViewError>> {
        toml::to_string_pretty(self).map_err(|e| {
            Box::new(ViewError::new(
                ViewErrorKind::Parse,
                format!("cannot write TOML: {e}"),
            ))
        })
    }
}

impl ViewDef {
    /// The unbound control kinds this model knows (ADR-068 §5 keeps the full catalog apart).
    pub const UNBOUND_CONTROLS: [&str; 4] = ["heading", "divider", "spacer", "help"];

    /// Whether `control` is one of the render-neutral unbound kinds.
    pub fn is_unbound_control(control: &str) -> bool {
        Self::UNBOUND_CONTROLS.contains(&control)
    }

    /// Every section: the tab sections first, then the top-level ones.
    pub fn all_sections(&self) -> Vec<&Section> {
        let mut sections: Vec<&Section> =
            self.tabs.iter().flat_map(|t| t.sections.iter()).collect();
        sections.extend(self.sections.iter());
        sections
    }

    /// Visits every item in order (recursing through groups) with `visits`.
    pub fn visit_items<'a>(&'a self, visits: &mut impl FnMut(&'a Item)) {
        for section in self.all_sections() {
            for item in &section.items {
                visit(item, visits);
            }
        }
    }

    /// Collects the structural problems of `self` — kind/shape mismatches, id shapes and
    /// duplicates, unknown unbound controls — into `errors`.
    fn check_shape(&self, source: &str, errors: &mut Vec<ViewError>) {
        let name = |e: ViewError| e.in_file(source).in_doctype(&self.doctype);
        match self.kind {
            ViewKind::Form => {
                if !self.tabs.is_empty() && !self.sections.is_empty() {
                    errors.push(name(ViewError::new(
                        ViewErrorKind::BadShape,
                        "a form carries tabs or top-level sections, not both",
                    )));
                }
                if self.tabs.is_empty() && self.sections.is_empty() {
                    errors.push(name(ViewError::new(
                        ViewErrorKind::BadShape,
                        "a form needs tabs or top-level sections",
                    )));
                }
                if !self.columns.is_empty() {
                    errors.push(name(ViewError::new(
                        ViewErrorKind::BadShape,
                        "columns belong to a list or grid view, not a form",
                    )));
                }
            }
            ViewKind::List => {
                self.reject_form_parts(
                    "tabs and sections belong to a form view, not a list",
                    errors,
                    source,
                );
            }
            ViewKind::Grid => {
                self.reject_form_parts(
                    "tabs and sections belong to a form view, not a grid",
                    errors,
                    source,
                );
            }
        }
        if let Some(sort) = &self.sort {
            if self.kind != ViewKind::List {
                errors.push(name(ViewError::new(
                    ViewErrorKind::BadShape,
                    "a default sort belongs to a list view",
                )));
            } else if sort.field.is_empty() {
                errors.push(name(ViewError::new(
                    ViewErrorKind::BadShape,
                    "a sort needs a field",
                )));
            }
        }
        for tab in &self.tabs {
            if !crate::error::is_id(&tab.id) {
                errors.push(name(
                    ViewError::new(
                        ViewErrorKind::BadId,
                        format!("tab id '{}' is not a lowercase id", tab.id),
                    )
                    .in_field(&tab.id),
                ));
            }
        }
        for section in self.all_sections() {
            if !crate::error::is_id(&section.id) {
                errors.push(name(
                    ViewError::new(
                        ViewErrorKind::BadId,
                        format!("section id '{}' is not a lowercase id", section.id),
                    )
                    .in_field(&section.id),
                ));
            }
        }
        let mut ids: Vec<&str> = self.tabs.iter().map(|t| t.id.as_str()).collect();
        ids.extend(self.all_sections().iter().map(|s| s.id.as_str()));
        for id in ids.iter().collect::<std::collections::BTreeSet<_>>() {
            if ids.iter().filter(|other| *other == id).count() > 1 {
                errors.push(name(
                    ViewError::new(ViewErrorKind::Duplicate, format!("id '{id}' is used twice"))
                        .in_field(id),
                ));
            }
        }
        let mut action_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for action in &self.actions {
            if !crate::error::is_id(&action.name) {
                errors.push(name(
                    ViewError::new(
                        ViewErrorKind::BadId,
                        format!("action name '{}' is not a lowercase id", action.name),
                    )
                    .in_field(&action.name),
                ));
            }
            action_names.insert(action.name.as_str());
        }
        for action_name in action_names {
            if self
                .actions
                .iter()
                .filter(|other| other.name == action_name)
                .count()
                > 1
            {
                errors.push(name(
                    ViewError::new(
                        ViewErrorKind::Duplicate,
                        format!("action '{action_name}' is declared twice"),
                    )
                    .in_field(action_name),
                ));
            }
        }
        self.check_controls(errors, source);
    }

    /// Rejects tab/section parts on a list or grid, once per kind.
    fn reject_form_parts(&self, why: &str, errors: &mut Vec<ViewError>, source: &str) {
        if !self.tabs.is_empty() || !self.sections.is_empty() {
            errors.push(
                ViewError::new(ViewErrorKind::BadShape, why)
                    .in_file(source)
                    .in_doctype(&self.doctype),
            );
        }
    }

    /// Checks the unbound control names of every item.
    fn check_controls(&self, errors: &mut Vec<ViewError>, source: &str) {
        let mut check = |item: &Item| {
            if let Item::Control { control, .. } = item
                && !Self::is_unbound_control(control)
            {
                errors.push(
                    ViewError::new(
                        ViewErrorKind::UnknownControl,
                        format!("control '{control}' is not in the unbound set"),
                    )
                    .in_file(source)
                    .in_doctype(&self.doctype)
                    .in_field(control),
                );
            }
        };
        self.visit_items(&mut check);
    }
}

/// Walks one item (recursing through groups), calling `visits` on each.
fn visit<'a>(item: &'a Item, visits: &mut impl FnMut(&'a Item)) {
    visits(item);
    if let Item::Group(group) = item {
        for nested in &group.items {
            visit(nested, visits);
        }
    }
}
