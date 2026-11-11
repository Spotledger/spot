//! The form layout: the tabs, sections, columns and item order the designer builds, kept
//! apart from the fields (ADR-068 §7: where a field appears is the view's business, never the
//! field's).
//!
//! [`FormLayout`] holds the form parts of a [`ViewDef`](spot_views::ViewDef) without the
//! DocType and kind, which are fixed for a draft. [`FormLayout::to_view`] turns it into the
//! `ViewDef` the client renders and `spot-views` validates; [`FormLayout::from_view`] reads a
//! view back. The layout is the whole intended layout, not a diff: a designer adds a section,
//! a tab or a column and moves a field, and the result is exactly what the preview shows.
//!
//! A form carries **tabs or top-level sections, never both** (the rule `spot-views` enforces),
//! so [`FormLayout`]'s mutators refuse the shape that would break it.

use spot_meta::EffectiveMeta;
use spot_views::viewdef::Section;
use spot_views::{
    Action, Density, Item, ResolvedView, Tab, ViewDef, ViewError, ViewKind, validate_layer,
};

/// The form layout of a draft.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormLayout {
    /// The density hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density: Option<Density>,
    /// The form's tabs, in order (empty for a tabless form).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<Tab>,
    /// The form's top-level sections, in order (empty for a tabbed form).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
    /// The actions offered on the form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

impl FormLayout {
    /// An empty tabless layout.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A tabless layout with one section holding `items`.
    #[must_use]
    pub fn with_section(id: &str, title: &str, items: Vec<Item>) -> Self {
        Self {
            sections: vec![Section {
                id: id.to_owned(),
                title: Some(title.to_owned()),
                columns: None,
                items,
            }],
            ..Self::default()
        }
    }

    /// The layout as a form [`ViewDef`] for `doctype`.
    #[must_use]
    pub fn to_view(&self, doctype: &str) -> ViewDef {
        ViewDef {
            doctype: doctype.to_owned(),
            kind: ViewKind::Form,
            role: None,
            title: None,
            density: self.density,
            tabs: self.tabs.clone(),
            sections: self.sections.clone(),
            columns: Vec::new(),
            sort: None,
            actions: self.actions.clone(),
        }
    }

    /// Reads a form [`ViewDef`] back into a layout, dropping the DocType and kind.
    #[must_use]
    pub fn from_view(view: &ViewDef) -> Self {
        Self {
            density: view.density,
            tabs: view.tabs.clone(),
            sections: view.sections.clone(),
            actions: view.actions.clone(),
        }
    }

    /// Every section: the tab sections first, then the top-level ones.
    #[must_use]
    pub fn all_sections(&self) -> Vec<&Section> {
        let mut sections: Vec<&Section> =
            self.tabs.iter().flat_map(|t| t.sections.iter()).collect();
        sections.extend(self.sections.iter());
        sections
    }

    /// Whether a section id is used.
    #[must_use]
    pub fn has_section(&self, id: &str) -> bool {
        self.all_sections().iter().any(|s| s.id == id)
    }

    /// Whether a tab id is used.
    #[must_use]
    pub fn has_tab(&self, id: &str) -> bool {
        self.tabs.iter().any(|t| t.id == id)
    }

    /// Whether any section holds `field` as a direct item (a group is not searched).
    #[must_use]
    pub fn contains_field(&self, field: &str) -> bool {
        self.all_sections().iter().any(|section| {
            section
                .items
                .iter()
                .any(|item| matches!(item, Item::Field { field: name, .. } if name == field))
        })
    }

    /// The sections that hold `field` as a direct item, in order.
    #[must_use]
    pub fn field_sections(&self, field: &str) -> Vec<&str> {
        self.all_sections()
            .iter()
            .filter(|section| {
                section
                    .items
                    .iter()
                    .any(|item| matches!(item, Item::Field { field: name, .. } if name == field))
            })
            .map(|section| section.id.as_str())
            .collect()
    }

    /// Adds a section. With `tab`, the section lands in that tab (the form must be tabbed);
    /// without one it lands at the top level (the form must be tabless). Refuses a duplicate id.
    ///
    /// # Errors
    ///
    /// A sentence naming why the section cannot be added.
    pub fn add_section(&mut self, section: Section, tab: Option<&str>) -> Result<(), String> {
        if self.has_section(&section.id) {
            return Err(format!("section `{}` already exists", section.id));
        }
        match tab {
            Some(id) => {
                if self.sections.is_empty() && self.tabs.is_empty() {
                    return Err(
                        "the form has no tabs yet; add a tab before a section into one".to_owned(),
                    );
                }
                if !self.sections.is_empty() {
                    return Err(
                        "the form uses top-level sections; it cannot also hold tabs".to_owned()
                    );
                }
                let target = self
                    .tabs
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| format!("tab `{id}` does not exist"))?;
                target.sections.push(section);
                Ok(())
            }
            None => {
                if !self.tabs.is_empty() {
                    return Err(
                        "the form uses tabs; a new section must name one of them".to_owned()
                    );
                }
                self.sections.push(section);
                Ok(())
            }
        }
    }

    /// Adds a tab at the end. A form with top-level sections cannot also hold tabs.
    ///
    /// # Errors
    ///
    /// A sentence naming why the tab cannot be added.
    pub fn add_tab(&mut self, tab: Tab) -> Result<(), String> {
        if self.has_tab(&tab.id) {
            return Err(format!("tab `{}` already exists", tab.id));
        }
        if !self.sections.is_empty() {
            return Err("the form uses top-level sections; it cannot also hold tabs".to_owned());
        }
        self.tabs.push(tab);
        Ok(())
    }

    /// Sets a section's column count.
    ///
    /// # Errors
    ///
    /// A sentence when the section does not exist or the count is out of range.
    pub fn set_section_columns(&mut self, id: &str, columns: u8) -> Result<(), String> {
        if columns == 0 {
            return Err("a section must have at least one column".to_owned());
        }
        let section = self
            .section_mut(id)
            .ok_or_else(|| format!("section `{id}` does not exist"))?;
        section.columns = Some(columns);
        Ok(())
    }

    /// Appends an item to a section.
    ///
    /// # Errors
    ///
    /// A sentence when the section does not exist.
    pub fn push_item(&mut self, section: &str, item: Item) -> Result<(), String> {
        let target = self
            .section_mut(section)
            .ok_or_else(|| format!("section `{section}` does not exist"))?;
        target.items.push(item);
        Ok(())
    }

    /// Moves a field into a section, optionally anchored after another field in that section.
    /// A field the layout does not hold yet is appended (this is how a new field enters the
    /// form); a field held elsewhere is removed first.
    ///
    /// # Errors
    ///
    /// A sentence when the target section or the anchor field is not there.
    pub fn move_field(
        &mut self,
        field: &str,
        section: &str,
        after: Option<&str>,
    ) -> Result<(), String> {
        if !self.has_section(section) {
            return Err(format!("section `{section}` does not exist"));
        }
        let item = self.take_field(field).unwrap_or(Item::Field {
            field: field.to_owned(),
            label: None,
            control: None,
            lookup: None,
        });
        let target = self
            .section_mut(section)
            .ok_or_else(|| format!("section `{section}` does not exist"))?;
        match after {
            None => target.items.push(item),
            Some(anchor) => {
                let at = target
                    .items
                    .iter()
                    .position(|existing| {
                        matches!(existing, Item::Field { field: name, .. } if name == anchor)
                    })
                    .ok_or_else(|| format!("anchor field `{anchor}` is not in `{section}`"))?;
                target.items.insert(at + 1, item);
            }
        }
        Ok(())
    }

    /// Removes a field's item from every section, returning whether it was there.
    pub fn remove_field(&mut self, field: &str) -> bool {
        self.take_field(field).is_some()
    }

    /// Renames a field's item everywhere it appears, keeping its position. A field with no
    /// item is left alone.
    pub fn rename_field(&mut self, from: &str, to: &str) {
        let rename = |items: &mut Vec<Item>| {
            for item in items.iter_mut() {
                if let Item::Field { field, .. } = item
                    && field == from
                {
                    *field = to.to_owned();
                }
            }
        };
        for section in self.sections.iter_mut() {
            rename(&mut section.items);
        }
        for tab in self.tabs.iter_mut() {
            for section in tab.sections.iter_mut() {
                rename(&mut section.items);
            }
        }
    }

    /// Takes a field's item out of the section that holds it.
    fn take_field(&mut self, field: &str) -> Option<Item> {
        for section in self.sections.iter_mut() {
            if let Some(item) = take_from_items(&mut section.items, field) {
                return Some(item);
            }
        }
        for tab in self.tabs.iter_mut() {
            for section in tab.sections.iter_mut() {
                if let Some(item) = take_from_items(&mut section.items, field) {
                    return Some(item);
                }
            }
        }
        None
    }

    /// A mutable section by id.
    fn section_mut(&mut self, id: &str) -> Option<&mut Section> {
        if let Some(section) = self.sections.iter_mut().find(|s| s.id == id) {
            return Some(section);
        }
        self.tabs
            .iter_mut()
            .flat_map(|t| t.sections.iter_mut())
            .find(|s| s.id == id)
    }

    /// Validates the layout as a form view over `meta`, reporting every problem at once. The
    /// check reuses `spot-views`' own save-time rules (a hidden required field must not strand
    /// a save, every named field exists, every id is well-formed and unique).
    #[must_use]
    pub fn validate(&self, doctype: &str, meta: &EffectiveMeta) -> Vec<ViewError> {
        let resolved = ResolvedView {
            view: self.to_view(doctype),
            hidden: Vec::new(),
            warnings: Vec::new(),
        };
        validate_layer(&resolved, meta)
    }
}

/// Removes a field's item from an item list, returning it.
fn take_from_items(items: &mut Vec<Item>, field: &str) -> Option<Item> {
    let at = items
        .iter()
        .position(|item| matches!(item, Item::Field { field: name, .. } if name == field))?;
    Some(items.remove(at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_is_added_and_a_field_moves() {
        let mut layout = FormLayout::with_section("main", "Main", Vec::new());
        layout.move_field("notes", "main", None).unwrap_or_default();
        assert!(layout.contains_field("notes"));
        assert_eq!(layout.field_sections("notes"), vec!["main"]);
        assert_eq!(layout.to_view("thing").kind, ViewKind::Form);
    }

    #[test]
    fn a_field_moves_between_sections_with_an_anchor() {
        let mut layout = FormLayout::with_section("one", "One", Vec::new());
        layout
            .add_section(
                Section {
                    id: "two".to_owned(),
                    title: None,
                    columns: None,
                    items: Vec::new(),
                },
                None,
            )
            .unwrap_or_default();
        layout.move_field("a", "one", None).unwrap_or_default();
        layout.move_field("b", "one", None).unwrap_or_default();
        layout.move_field("c", "one", Some("a")).unwrap_or_default();
        let ids: Vec<&str> = layout.sections[0]
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Field { field, .. } => Some(field.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["a", "c", "b"]);
    }

    #[test]
    fn tabs_and_top_level_sections_never_mix() {
        let mut layout = FormLayout::with_section("main", "Main", Vec::new());
        let tab = Tab {
            id: "t".to_owned(),
            title: None,
            sections: Vec::new(),
        };
        assert!(layout.add_tab(tab).is_err());
    }

    #[test]
    fn duplicate_section_ids_are_refused() {
        let mut layout = FormLayout::with_section("main", "Main", Vec::new());
        let again = Section {
            id: "main".to_owned(),
            title: None,
            columns: None,
            items: Vec::new(),
        };
        assert!(layout.add_section(again, None).is_err());
    }
}
