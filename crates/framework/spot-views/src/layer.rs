//! The layer: a diff over a base view, not a copy (ADR-068 §7.2, ADR-069 §4.2).
//!
//! A layer stores only its differences, so an app update that changes the standard view still
//! shows through: a field the standard view adds keeps appearing, in the standard position,
//! for a site that customised the layout. A layer refers to fields **by name** and to
//! sections/tabs **by id**.
//!
//! ```toml
//! doctype = "sales_invoice"
//! kind = "site"
//!
//! [[ops]]
//! op = "move_field"
//! field = "customer"
//! section = "totals"
//! after = "date"
//!
//! [[ops]]
//! op = "hide_field"
//! field = "internal_note"
//! ```

use serde::{Deserialize, Serialize};

use crate::error::{ViewError, ViewErrorKind};
use crate::viewdef::{Column, Item, Sort, Tab};

/// Which kind of layer a layer is: the specificity comes from [`LayerName`], and the
/// role/step layers carry their name next to it — TOML-friendly plain fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerName {
    /// The app's standard view (or a generated default): the base.
    #[default]
    Standard,
    /// A site customisation.
    Site,
    /// A role layer.
    Role,
    /// A Process step context (ADR-068 §8).
    Step,
}

/// How specific a layer is; layers apply from least to most specific:
/// `standard` (the base itself) < `site` < `role` < `step` (ADR-068 §7.2).
///
/// A whole **role view** is not this — it is a [`ViewDef`] with `role` set, chosen by
/// [`select_role_view`](crate::roles::select_role_view) and never merged. A `role` **layer**
/// diffs over whatever base it is given, like a site layer one step more specific.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerKind {
    /// Which kind of layer this is.
    pub kind: LayerName,
    /// The role's name, for a `role` layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The step's name, for a `step` layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

impl LayerKind {
    /// A layer kind of `name` with no role or step.
    pub fn named(name: LayerName) -> Self {
        Self {
            kind: name,
            role: None,
            step: None,
        }
    }

    /// The specificity rank: 0 standard, 1 site, 2 role, 3 step.
    pub fn rank(&self) -> u8 {
        match self.kind {
            LayerName::Standard => 0,
            LayerName::Site => 1,
            LayerName::Role => 2,
            LayerName::Step => 3,
        }
    }
}

/// The anchored target of a [`Op::MoveField`] or [`Op::AddItem`]: the section or tab things
/// land in, and an optional before/after anchor field (ADR-069 §4.2: anchored operations).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewAnchor {
    /// The tab to land in (its last section); for a move, the current tab when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    /// The section to land in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// The field to land before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The field to land after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// One difference the layer makes to the base view, tagged `op` in TOML.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    /// Moves a field: into a tab or section, optionally anchored before/after another field.
    /// With no `tab` and no `section`, the field orders within its current section. The keys
    /// sit directly on the op (TOML-friendly, ADR-069 §4.2's anchored move):
    ///
    /// ```toml
    /// [[ops]]
    /// op = "move_field"
    /// field = "customer"
    /// section = "totals"
    /// after = "date"
    /// ```
    MoveField {
        /// The field to move, by name.
        field: String,
        /// The tab to land in (its last section); the current tab when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
        /// The section to land in.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        section: Option<String>,
        /// The field to land before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        /// The field to land after.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    },
    /// Hides a field from this view (it stays stored, in the API and checked by permissions,
    /// ADR-068 §7.4).
    HideField {
        /// The field to hide, by name.
        field: String,
    },
    /// Shows a field a more general layer hid. The field re-enters the view at the default
    /// position (definition order, appended), not its old one.
    ShowField {
        /// The field to show, by name.
        field: String,
    },
    /// Adds an unbound control, a field or a group into a section (a form view).
    AddItem {
        /// The section to land in.
        section: String,
        /// The optional anchor: before or after a named field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<ViewAnchor>,
        /// The item to add.
        item: Item,
    },
    /// Adds a tab at the end of the form.
    AddTab {
        /// The tab to add; its id must be new.
        tab: Tab,
    },
    /// Renames a section: its title only, never its id.
    RenameSection {
        /// The section to rename.
        section: String,
        /// The new title.
        title: String,
    },
    /// Sets the columns (and the default sort on a list) — the `set_columns` operation of
    /// ADR-069 §4.2, valid on list and grid layers.
    SetColumns {
        /// The columns, in order.
        columns: Vec<Column>,
        /// The new default sort (list only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sort: Option<Sort>,
    },
    /// Sets a field's presentation hints in this view: label, control binding or lookup seam
    /// (ADR-068 §7.6). Only the hints given change; storage properties cannot be touched.
    SetHint {
        /// The field whose hints change.
        field: String,
        /// The new label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The new control binding.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<String>,
        /// The new lookup seam.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lookup: Option<String>,
    },
}

impl Op {
    /// Whether the op fits the view kind it applies to: `set_columns` belongs to list and
    /// grid views (a sort only to a list), everything else to forms.
    pub fn fits(&self, kind: crate::viewdef::ViewKind) -> bool {
        use crate::viewdef::ViewKind;
        match self {
            Op::SetColumns { sort, .. } => match kind {
                ViewKind::List => true,
                ViewKind::Grid => sort.is_none(),
                ViewKind::Form => false,
            },
            _ => true,
        }
    }
}

/// The layer as stored: the DocType it diffs over, its specificity and its ordered ops.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewLayer {
    /// The DocType whose base view this layer diffs.
    pub doctype: String,
    /// How specific this layer is.
    #[serde(flatten)]
    pub kind: LayerKind,
    /// The differences, applied in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<Op>,
}

impl ViewLayer {
    /// Parses one layer file. Same error discipline as
    /// [`ViewDef::parse`](crate::viewdef::ViewDef::parse): position, stable `view.*` code,
    /// all semantic errors at once.
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
        Ok(parsed)
    }

    /// Writes the layer as TOML, keys in declaration order. The error is boxed because a
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
