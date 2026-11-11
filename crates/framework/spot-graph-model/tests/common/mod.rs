//! Shared fixtures for the integration tests: a small metadata set with every
//! kind of field, built through `spot_meta::Registry` exactly as the platform
//! builds it.
//!
//! DocType names avoid the reserved system columns (`id`, `version`, `parent`,
//! `parent_field`, `idx`) and the engine fields (`doc_state`, `amended_from`);
//! custom field names must start with `custom_`.

#![allow(dead_code, missing_docs)]

use spot_meta::{Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, Registry};

/// `party`: a master DocType with a text code and a dynamic link whose type
/// field is free text (`about_type`), so its edge target is `Dynamic`.
pub fn party() -> Definition {
    let mut d = Definition::new("party");
    d.label = Some("Party".into());
    d.fields = vec![
        FieldSpec::new("code", FieldType::Text),
        FieldSpec::new("about_type", FieldType::Text),
        FieldSpec::new("about", FieldType::DynamicLink).with_options("about_type"),
    ];
    d
}

/// `project`: a DocType with two link fields to the same target, a self
/// reference and a table.
pub fn project() -> Definition {
    let mut d = Definition::new("project");
    d.fields = vec![
        text("name"),
        link("customer", "party"),
        link("manager", "party"),
        link("parent_project", "project"),
        FieldSpec::new("tasks", FieldType::Table).with_options("project_task"),
        FieldSpec::new("watchers", FieldType::TableMultiselect).with_options("project_watcher"),
    ];
    d
}

/// `project_task`: the child of `project.tasks`.
pub fn project_task() -> Definition {
    let mut d = Definition::new("project_task");
    d.is_child = true;
    d.fields = vec![text("title"), link("assignee", "party")];
    d
}

/// `project_watcher`: the child of `project.watchers`, one link field (the
/// multi-picked master).
pub fn project_watcher() -> Definition {
    let mut d = Definition::new("project_watcher");
    d.is_child = true;
    d.fields = vec![link("party", "party")];
    d
}

/// `task_link`: a relation DocType between `project` and `party` with a
/// `role` property field.
pub fn task_link() -> Definition {
    let mut d = Definition::new("task_link");
    d.fields = vec![
        link("project", "project"),
        link("party", "party"),
        text("role"),
    ];
    d
}

/// `order`: a DocType whose dynamic link's type field is a select naming two
/// known DocTypes, so the edge target is `AnyOf`.
pub fn order() -> Definition {
    let mut d = Definition::new("order");
    let mut kind = FieldSpec::new("ref_kind", FieldType::Select);
    kind.options = Some("party\nproject".into());
    d.fields = vec![
        text("number"),
        kind,
        FieldSpec::new("ref", FieldType::DynamicLink).with_options("ref_kind"),
    ];
    d
}

/// A definition set with every field shape the tests walk.
pub fn fixtures() -> Vec<Definition> {
    vec![
        party(),
        project(),
        project_task(),
        project_watcher(),
        order(),
    ]
}

/// Builds the effective metadata over `definitions` with no customisations.
///
/// # Panics
/// The fixtures are checked-in and valid; a failure is a setup bug.
pub fn effective_of(definitions: Vec<Definition>) -> EffectiveMeta {
    Registry::build(definitions, &Customisations::new())
        .unwrap_or_else(|errors| panic!("fixtures do not build: {errors:?}"))
}

/// The full fixture set's effective metadata.
///
/// # Panics
/// As `effective_of`.
pub fn effective() -> EffectiveMeta {
    effective_of(fixtures())
}

/// A registry over the full fixture set, for the custom-field tests.
///
/// # Panics
/// As `effective_of`.
pub fn registry() -> Registry {
    Registry::new(fixtures()).unwrap_or_else(|errors| panic!("fixtures do not build: {errors:?}"))
}

/// A required text field.
pub fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// A link field to `target`.
pub fn link(name: &str, target: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Link).with_options(target)
}

/// A custom link field (the name spot-meta requires for custom fields).
pub fn custom_link(name: &str, target: &str) -> FieldSpec {
    link(name, target)
}

/// One relation spec for `task_link`.
pub fn task_link_spec(edge_name: &str) -> spot_graph_model::RelationSpec {
    spot_graph_model::RelationSpec {
        doctype: "task_link".into(),
        from_field: "project".into(),
        to_field: "party".into(),
        edge_name: edge_name.into(),
        aggregate: None,
    }
}

/// Whether `errors` contains exactly one problem of `code`.
///
/// # Panics
/// When the count differs; the message lists what is there.
#[track_caller]
pub fn assert_one_problem<'a>(
    model: &'a spot_graph_model::GraphModel,
    code: &str,
) -> &'a spot_graph_model::Problem {
    let found: Vec<&spot_graph_model::Problem> = model
        .problems()
        .iter()
        .filter(|p| p.code() == code)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one {code}, got {}: {:?}",
        model.problems().len(),
        model.problems()
    );
    found[0]
}

pub trait WithOptions: Sized {
    fn with_options(self, options: &str) -> Self;
}

impl WithOptions for FieldSpec {
    fn with_options(mut self, options: &str) -> Self {
        self.options = Some(options.to_owned());
        self
    }
}
