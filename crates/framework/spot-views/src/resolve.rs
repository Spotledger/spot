//! The merge: [`resolve`] applies layers over the base view and reports what fell away.
//!
//! Layers apply from least to most specific: `standard` (the base itself) < `site` < `role` <
//! `step` (ADR-068 §7.2). Layers of equal kind apply in the order they are listed — which is
//! observable (two unanchored moves to the same section land in op order, and the last hint
//! wins), so determinism is: **rank first, listed order second, always.**
//!
//! The merge is forgiving by design (ADR-069 §4.5): a diff that no longer fits — a field the
//! app removed, a section that was renamed away — is **dropped with a warning** in the
//! result, never an error; the customisations that still fit keep applying. After every
//! layer, each effective field nobody mentioned is appended in definition order into the
//! last section of a form view — except fields a layer explicitly hid — so no stored field
//! silently disappears from the default form (ADR-068 §7.1).
//!
//! Which diffs **commute** (so layers of equal kind may be listed in either order):
//! - `hide_field`/`show_field` over a *different* field, `rename_section` on a different
//!   id, `add_item`/`move_field` into a different section or anchored differently, and
//!   `set_hint` on a different field all commute;
//! - two moves into the *same* section without anchors, two `set_hint`s on the same field,
//!   and `hide_field`/`show_field` on the *same* field do not — listed order decides.
//!
//! `tests/commutation.rs` pins both halves.

use std::collections::BTreeSet;

use spot_meta::EffectiveMeta;

use crate::error::ViewErrorKind;
use crate::layer::{Op, ViewAnchor, ViewLayer};
use crate::viewdef::{Item, Section, ViewDef, ViewKind};

/// A diff that could not apply, dropped with a warning in the result — never an error at
/// resolve time (ADR-068 §7.2, ADR-069 §4.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveWarning {
    /// The stable `view.*` code; the same values [`ViewErrorKind::code`] carries.
    pub code: &'static str,
    /// Which layer it came from, by index into the caller's slice.
    pub layer: usize,
    /// Which op of that layer, by index.
    pub op: usize,
    /// What the diff named (a field, a section, a tab), when it did.
    pub subject: Option<String>,
    /// What happened, in a sentence.
    pub message: String,
}

/// The merged result of [`resolve`]: the view, the fields layers explicitly hid, and the
/// diffs that fell away.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedView {
    /// The merged view: the base after every layer, with unmentioned fields appended.
    pub view: ViewDef,
    /// The fields a layer explicitly hid and no layer showed: still stored, still in the
    /// API, still permission-checked (ADR-068 §7.4).
    pub hidden: Vec<String>,
    /// The diffs that could not apply, dropped with a warning each.
    pub warnings: Vec<ResolveWarning>,
}

/// The pending hints of a `set_hint` whose field had no item yet when the op ran; applied
/// when the field is appended (and dropped if it ends up hidden).
struct PendingHint {
    label: Option<String>,
    control: Option<String>,
    lookup: Option<String>,
}

/// The mutable merge state.
struct Working {
    view: ViewDef,
    hidden: BTreeSet<String>,
    mentioned: BTreeSet<String>,
    pending: Vec<(String, PendingHint, usize, usize)>,
    warnings: Vec<ResolveWarning>,
}

impl Working {
    fn warn(
        &mut self,
        layer: usize,
        op: usize,
        code: ViewErrorKind,
        subject: Option<&str>,
        message: String,
    ) {
        self.warnings.push(ResolveWarning {
            code: code.code(),
            layer,
            op,
            subject: subject.map(str::to_owned),
            message,
        });
    }

    /// Removes `field`'s item from the sections it sits in (recursing through groups) and
    /// returns the removed [`Item`], if it was there.
    fn take_field(&mut self, field: &str) -> Option<Item> {
        fn take_from_sections(sections: &mut [Section], field: &str) -> Option<Item> {
            for section in sections.iter_mut() {
                if let Some(item) = take_from_items(&mut section.items, field) {
                    return Some(item);
                }
            }
            None
        }

        fn take_from_items(items: &mut Vec<Item>, field: &str) -> Option<Item> {
            let mut outer: Option<usize> = None;
            let mut nested: Option<usize> = None;
            for (index, item) in items.iter().enumerate() {
                let mention = match item {
                    Item::Field { field: name, .. } => Some(name.as_str() == field),
                    Item::Group(group) => {
                        let at = group.items.iter().position(|nested_item| {
                            matches!(nested_item, Item::Field { field: name, .. } if name.as_str() == field)
                        });
                        if at.is_some() {
                            outer = Some(index);
                            nested = at;
                        }
                        None
                    }
                    _ => None,
                };
                if mention == Some(true) {
                    outer = Some(index);
                    break;
                }
                if nested.is_some() {
                    break;
                }
            }
            let outer = outer?;
            match nested {
                Some(at) => match items.get_mut(outer) {
                    Some(Item::Group(group)) => Some(group.items.remove(at)),
                    _ => None,
                },
                None if matches!(items.get(outer), Some(Item::Field { .. })) => {
                    Some(items.remove(outer))
                }
                None => None,
            }
        }
        let mut item = take_from_sections(&mut self.view.sections, field);
        if item.is_none() {
            for tab in &mut self.view.tabs {
                item = take_from_sections(&mut tab.sections, field);
                if item.is_some() {
                    break;
                }
            }
        }
        item
    }

    /// The section the field's item sits in, for in-place reorders.
    fn section_of_field(&self, field: &str) -> Option<String> {
        let present = |items: &[Item]| field_in(items, field);
        self.view
            .sections
            .iter()
            .find(|section| present(&section.items))
            .or_else(|| {
                self.view
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.sections.iter())
                    .find(|section| present(&section.items))
            })
            .map(|section| section.id.clone())
    }

    /// The mutable section with this id, wherever it lives.
    fn section_mut(&mut self, id: &str) -> Option<&mut Section> {
        self.view
            .sections
            .iter_mut()
            .find(|section| section.id == id)
            .into_iter()
            .chain(
                self.view
                    .tabs
                    .iter_mut()
                    .flat_map(|tab| tab.sections.iter_mut())
                    .filter(|section| section.id == id),
            )
            .next()
    }

    /// Whether the section id exists.
    fn has_section(&self, id: &str) -> bool {
        self.view
            .sections
            .iter()
            .chain(self.view.tabs.iter().flat_map(|tab| tab.sections.iter()))
            .any(|section| section.id == id)
    }

    /// The last section in view order (tab sections first, then top-level).
    fn last_section_mut(&mut self) -> Option<&mut Section> {
        if let Some(last) = self.view.sections.last_mut() {
            return Some(last);
        }
        self.view
            .tabs
            .last_mut()
            .and_then(|tab| tab.sections.last_mut())
    }

    /// Whether the field sits in the section (recursing through groups).
    fn section_has_field(&self, id: &str, field: &str) -> bool {
        self.view
            .all_sections()
            .into_iter()
            .find(|section| section.id == id)
            .is_some_and(|section| field_in(&section.items, field))
    }

    /// The section a tab lands in when a move names no section: its last, when the tab has
    /// any; `None` when it has none (the move falls to no section at all).
    fn tab_last_section(&self, id: &str) -> Option<String> {
        self.view
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.sections.last().map(|s| s.id.clone()))
    }

    /// Inserts the item into Section `id`, anchored by `anchor` when it holds; an anchor
    /// naming a field that is not there falls back to the section end with a warning
    /// (ADR-069 §4.5: never silent loss). A section that is not there drops the item with a
    /// warning.
    fn insert_into_section(
        &mut self,
        section_id: &str,
        anchor: &Option<ViewAnchor>,
        item: Item,
        layer: usize,
        op: usize,
    ) {
        let before = anchor.as_ref().and_then(|anchor| anchor.before.clone());
        let after = anchor.as_ref().and_then(|anchor| anchor.after.clone());
        if before.is_some() && after.is_some() {
            self.warn(
                layer,
                op,
                ViewErrorKind::UnknownAnchor,
                None,
                "a move cannot anchor before and after at once; the item is dropped".to_owned(),
            );
            return;
        }
        let anchored = before
            .as_ref()
            .is_none_or(|anchor| self.section_has_field(section_id, anchor))
            && after
                .as_ref()
                .is_none_or(|anchor| self.section_has_field(section_id, anchor));
        if !anchored {
            let subject = before.as_ref().or(after.as_ref()).cloned();
            self.warn(
                layer,
                op,
                ViewErrorKind::UnknownAnchor,
                subject.as_deref(),
                format!(
                    "the anchor is not in section '{section_id}'; the item lands at the section end"
                ),
            );
        }
        let position = match self.section_mut(section_id) {
            Some(section) => match (&before, &after) {
                (Some(anchor), _) if anchored => section
                    .items
                    .iter()
                    .position(field_is(anchor.as_str()))
                    .unwrap_or(section.items.len()),
                (_, Some(anchor)) if anchored => section
                    .items
                    .iter()
                    .position(field_is(anchor.as_str()))
                    .map_or(section.items.len(), |at| at + 1),
                _ => section.items.len(),
            },
            None => {
                self.warn(
                    layer,
                    op,
                    ViewErrorKind::UnknownSection,
                    Some(section_id),
                    format!("section '{section_id}' is not in the view; the item is dropped"),
                );
                return;
            }
        };
        mark_mentioned(&item, &mut self.mentioned);
        if let Some(section) = self.section_mut(section_id) {
            section.items.insert(position, item);
        }
    }

    /// Applies one op. Every failure mode warns and drops — resolve never errors.
    fn apply_op(&mut self, layer: usize, index: usize, op: &Op, meta_fields: &[String]) {
        match op {
            Op::MoveField {
                field,
                tab,
                section,
                before,
                after,
            } => self.move_field(
                field,
                &MoveTarget {
                    tab,
                    section,
                    before,
                    after,
                },
                layer,
                index,
            ),
            Op::HideField { field } => {
                if !meta_fields.iter().any(|known| known == field) {
                    self.warn(
                        layer,
                        index,
                        ViewErrorKind::UnknownField,
                        Some(field),
                        format!("the diff hides field '{field}', which the DocType does not have; dropped"),
                    );
                    return;
                }
                self.take_field(field);
                self.hidden.insert(field.clone());
                // The hide wins over any earlier mention: a shown field re-enters through the
                // definition-order append, not through a stale mention.
                self.mentioned.remove(field);
            }
            Op::ShowField { field } => {
                if !meta_fields.iter().any(|known| known == field) {
                    self.warn(
                        layer,
                        index,
                        ViewErrorKind::UnknownField,
                        Some(field),
                        format!("the diff shows field '{field}', which the DocType does not have; dropped"),
                    );
                    return;
                }
                self.hidden.remove(field);
            }
            Op::AddItem {
                section,
                anchor,
                item,
            } => {
                self.insert_into_section(section, anchor, item.clone(), layer, index);
            }
            Op::AddTab { tab } => {
                if self.view.tabs.iter().any(|existing| existing.id == tab.id) {
                    self.warn(
                        layer,
                        index,
                        ViewErrorKind::Duplicate,
                        Some(&tab.id),
                        format!(
                            "tab '{}' is already in the view; the diff is dropped",
                            tab.id
                        ),
                    );
                    return;
                }
                self.view.tabs.push(tab.clone());
            }
            Op::RenameSection { section, title } => {
                let Some(target) = self.section_mut(section) else {
                    self.warn(
                        layer,
                        index,
                        ViewErrorKind::UnknownSection,
                        Some(section),
                        format!("the diff renames section '{section}', which is not in the view; dropped"),
                    );
                    return;
                };
                target.title = Some(title.clone());
            }
            Op::SetColumns { columns, sort } => {
                self.view.columns = columns.clone();
                self.view.sort = sort.clone();
            }
            Op::SetHint {
                field,
                label,
                control,
                lookup,
            } => {
                if !meta_fields.iter().any(|known| known == field) {
                    self.warn(
                        layer,
                        index,
                        ViewErrorKind::UnknownField,
                        Some(field),
                        format!("the diff hints field '{field}', which the DocType does not have; dropped"),
                    );
                    return;
                }
                if !self.apply_hint_to_item(field, label, control, lookup) {
                    self.pending.push((
                        field.clone(),
                        PendingHint {
                            label: label.clone(),
                            control: control.clone(),
                            lookup: lookup.clone(),
                        },
                        layer,
                        index,
                    ));
                }
            }
        }
    }

    /// Applies a hint to the field's item where it sits; `false` when there is no item yet.
    fn apply_hint_to_item(
        &mut self,
        field: &str,
        label: &Option<String>,
        control: &Option<String>,
        lookup: &Option<String>,
    ) -> bool {
        fn hint_in(
            items: &mut [Item],
            field: &str,
            label: &Option<String>,
            control: &Option<String>,
            lookup: &Option<String>,
        ) -> bool {
            for item in items.iter_mut() {
                let touched = match item {
                    Item::Field {
                        field: name,
                        label: item_label,
                        control: item_control,
                        lookup: item_lookup,
                    } if name.as_str() == field => {
                        if label.is_some() {
                            *item_label = label.clone();
                        }
                        if control.is_some() {
                            *item_control = control.clone();
                        }
                        if lookup.is_some() {
                            *item_lookup = lookup.clone();
                        }
                        true
                    }
                    Item::Group(group) => hint_in(&mut group.items, field, label, control, lookup),
                    _ => false,
                };
                if touched {
                    return true;
                }
            }
            false
        }
        let mut touched = self
            .view
            .sections
            .iter_mut()
            .any(|section| hint_in(&mut section.items, field, label, control, lookup));
        if !touched {
            for tab in &mut self.view.tabs {
                touched = tab
                    .sections
                    .iter_mut()
                    .any(|section| hint_in(&mut section.items, field, label, control, lookup));
                if touched {
                    break;
                }
            }
        }
        touched
    }

    /// Moves the field: the landing is validated first, so a diff that does not fit leaves
    /// the field exactly where it was (never silent loss); only then the item is taken and
    /// re-placed, anchored as asked, with a broken anchor falling back to the section end.
    fn move_field(&mut self, field: &str, target: &MoveTarget<'_>, layer: usize, index: usize) {
        let MoveTarget {
            tab,
            section,
            before,
            after,
        } = *target;
        if before.is_some() && after.is_some() {
            self.warn(
                layer,
                index,
                ViewErrorKind::UnknownAnchor,
                Some(field),
                "a move cannot anchor before and after at once; the field stays where it is"
                    .to_owned(),
            );
            return;
        }
        let landing = if let Some(section) = section {
            if !self.has_section(section) {
                self.warn(
                    layer,
                    index,
                    ViewErrorKind::UnknownSection,
                    Some(section),
                    format!(
                        "the diff moves into section '{section}', which is not in the view; the field stays"
                    ),
                );
                return;
            }
            section.clone()
        } else if let Some(tab) = tab {
            match self.tab_last_section(tab) {
                Some(landed) => landed,
                None => {
                    self.warn(
                        layer,
                        index,
                        ViewErrorKind::UnknownTab,
                        Some(tab),
                        format!("tab '{tab}' has no section to move into; the field stays"),
                    );
                    return;
                }
            }
        } else {
            match self.section_of_field(field) {
                Some(landed) => landed,
                None => {
                    self.warn(
                        layer,
                        index,
                        ViewErrorKind::UnknownField,
                        Some(field),
                        format!(
                            "the diff moves field '{field}', which is not in the view; dropped"
                        ),
                    );
                    return;
                }
            }
        };
        let Some(item) = self.take_field(field) else {
            self.warn(
                layer,
                index,
                ViewErrorKind::UnknownField,
                Some(field),
                format!("the diff moves field '{field}', which is not in the view; dropped"),
            );
            return;
        };
        let anchor = ViewAnchor {
            tab: None,
            section: Some(landing.clone()),
            before: before.clone(),
            after: after.clone(),
        };
        self.insert_into_section(&landing, &Some(anchor), item, layer, index);
    }
}

/// The landing and anchor parts of a move, grouped so the helper stays within the
/// argument-count lint limit.
struct MoveTarget<'a> {
    /// The tab to land in (its last section); absent means the field's current tab.
    tab: &'a Option<String>,
    /// The section to land in; absent means the tab or the field's current section.
    section: &'a Option<String>,
    /// The field to land before.
    before: &'a Option<String>,
    /// The field to land after.
    after: &'a Option<String>,
}

/// Appends every effective field nobody mentioned — in definition order, into the last
/// section, applying pending hints — and drops the ones a layer hid. A stale item for a
/// hidden field is likewise removed: the hide wins.
fn append_unmentioned(working: &mut Working, meta_fields: &[String]) {
    let mut to_append: Vec<String> = Vec::new();
    for field in meta_fields {
        let mentioned = working.mentioned.contains(field);
        let hidden = working.hidden.contains(field);
        if hidden && mentioned {
            working.take_field(field);
            working.mentioned.remove(field);
        }
        if !mentioned && !hidden {
            to_append.push(field.clone());
        }
    }
    // A form with no sections at all gets one to hold its fields (documented default).
    if !to_append.is_empty()
        && working.view.kind == ViewKind::Form
        && working.last_section_mut().is_none()
    {
        working.view.sections.push(Section {
            id: "general".to_owned(),
            title: None,
            columns: None,
            items: Vec::new(),
        });
    }
    for field in to_append {
        let pending = working
            .pending
            .iter()
            .find(|(target, ..)| target == &field)
            .map(|(_, hint, ..)| {
                (
                    hint.label.clone(),
                    hint.control.clone(),
                    hint.lookup.clone(),
                )
            });
        let (label, control, lookup) = pending.unwrap_or((None, None, None));
        let item = Item::Field {
            field: field.clone(),
            label,
            control,
            lookup,
        };
        let Some(section) = working.last_section_mut() else {
            break;
        };
        let position = section.items.len();
        section.items.insert(position, item);
        working.mentioned.insert(field);
    }
    // Whatever remains of the pending hints either lands now (its field got an item during
    // the ops) or never lands (hidden, or out of the view): warned, never silent.
    let rest: Vec<(String, PendingHint, usize, usize)> = std::mem::take(&mut working.pending);
    for (field, hint, layer, op) in rest {
        if working.mentioned.contains(&field)
            && working.apply_hint_to_item(&field, &hint.label, &hint.control, &hint.lookup)
        {
            continue;
        }
        let why = if working.hidden.contains(&field) {
            "the field is hidden"
        } else {
            "the field is not in the view"
        };
        working.warn(
            layer,
            op,
            ViewErrorKind::UnknownField,
            Some(&field),
            format!("the hint for '{field}' never lands: {why}; dropped"),
        );
    }
}

/// Whether the field is mentioned among the items (recursing through groups).
fn field_in(items: &[Item], field: &str) -> bool {
    items.iter().any(|item| match item {
        Item::Field { field: name, .. } => name.as_str() == field,
        Item::Group(group) => field_in(&group.items, field),
        _ => false,
    })
}

/// A predicate matching a field item by name.
fn field_is(field: &str) -> impl Fn(&Item) -> bool + '_ {
    move |item: &Item| matches!(item, Item::Field { field: name, .. } if name.as_str() == field)
}

/// Marks one item's field as mentioned.
fn mark_mentioned(item: &Item, mentioned: &mut BTreeSet<String>) {
    if let Item::Field { field, .. } = item {
        mentioned.insert(field.clone());
    }
}

/// The op's name, for warning messages.
fn op_name(op: &Op) -> &'static str {
    match op {
        Op::MoveField { .. } => "move_field",
        Op::HideField { .. } => "hide_field",
        Op::ShowField { .. } => "show_field",
        Op::AddItem { .. } => "add_item",
        Op::AddTab { .. } => "add_tab",
        Op::RenameSection { .. } => "rename_section",
        Op::SetColumns { .. } => "set_columns",
        Op::SetHint { .. } => "set_hint",
    }
}

/// The kind's name, for warning messages.
fn kind_name(kind: ViewKind) -> &'static str {
    match kind {
        ViewKind::Form => "form",
        ViewKind::List => "list",
        ViewKind::Grid => "grid",
    }
}

/// Merges the `layers` over `base` against `meta`, in specificity order (rank, then listed
/// order within a rank), then appends every effective field nobody mentioned into the last
/// section of a form view — except the fields a layer hid. A layer naming a DocType other
/// than the base's is skipped with a warning.
///
/// Pure and deterministic: the same inputs give the same [`ResolvedView`].
pub fn resolve(base: &ViewDef, layers: &[ViewLayer], meta: &EffectiveMeta) -> ResolvedView {
    let mut working = Working {
        view: base.clone(),
        hidden: BTreeSet::new(),
        mentioned: BTreeSet::new(),
        pending: Vec::new(),
        warnings: Vec::new(),
    };
    record_initial_mentions(base, &mut working.mentioned);

    let meta_fields: Vec<String> = match meta.definition(&base.doctype) {
        Some(definition) => definition
            .fields
            .iter()
            .map(|field| field.name.clone())
            .collect(),
        None => {
            working.warnings.push(ResolveWarning {
                code: ViewErrorKind::UnknownField.code(),
                layer: 0,
                op: 0,
                subject: Some(base.doctype.clone()),
                message: format!(
                    "the DocType '{}' is not in the metadata; nothing appends",
                    base.doctype
                ),
            });
            Vec::new()
        }
    };

    let mut ordered: Vec<(usize, &ViewLayer)> = layers.iter().enumerate().collect();
    ordered.sort_by_key(|(position, layer)| (layer.kind.rank(), *position));
    for (layer_index, layer) in ordered {
        if layer.doctype != base.doctype {
            working.warn(
                layer_index,
                0,
                ViewErrorKind::BadShape,
                Some(&layer.doctype),
                format!(
                    "the layer is for '{}', not '{}'; skipped",
                    layer.doctype, base.doctype
                ),
            );
            continue;
        }
        for (op_index, op) in layer.ops.iter().enumerate() {
            if !op.fits(base.kind) {
                working.warn(
                    layer_index,
                    op_index,
                    ViewErrorKind::BadOp,
                    None,
                    format!(
                        "{} does not fit a {} view; dropped",
                        op_name(op),
                        kind_name(base.kind)
                    ),
                );
                continue;
            }
            working.apply_op(layer_index, op_index, op, &meta_fields);
        }
    }
    append_unmentioned(&mut working, &meta_fields);

    let hidden: Vec<String> = working.hidden.into_iter().collect();
    ResolvedView {
        view: working.view,
        hidden,
        warnings: working.warnings,
    }
}

/// A layer applied over an already-resolved view once more — the Process step context
/// (ADR-068 §8), applied when the document opens from the step. Same rules as [`resolve`].
pub fn resolve_over(
    resolved: &ResolvedView,
    layer: &ViewLayer,
    meta: &EffectiveMeta,
) -> ResolvedView {
    resolve(&resolved.view, std::slice::from_ref(layer), meta)
}

/// Marks the fields the base already mentions.
fn record_initial_mentions(base: &ViewDef, mentioned: &mut BTreeSet<String>) {
    base.visit_items(&mut |item| mark_mentioned(item, mentioned));
}

// APPEND_FN_MARKER
