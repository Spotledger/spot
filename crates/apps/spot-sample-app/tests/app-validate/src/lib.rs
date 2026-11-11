//! The detached validation suite (WP-DX-05.T1): the checks the app crate cannot
//! run because gate G4 keeps framework crates out of app dependencies.
//!
//! Run `cargo test` in this directory. The suite proves:
//!
//! 1. all five DocTypes (plus the `user` fixture standing in for the platform
//!    app) build under `spot_meta::Registry` with no errors, and compile to a
//!    storage catalog and the app view with the field shapes intact;
//! 2. the permission grants load and the decision table holds: the three roles
//!    over the five DocTypes and the six lifecycle actions, the `if_owner` rows,
//!    the scoped manager assignment, and the `confidential` field security;
//! 3. the typed-view generator produces the views for the app;
//! 4. the seed parses and validates against the metadata and the app's own
//!    rules (counts, hour bounds, select values, references);
//! 5. the `sample.*` codes sit in the error catalogue file.

#![cfg(test)]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use spot_app_api::prelude::Decimal;
use spot_permissions::engine::Action;
use spot_permissions::model::{AssignmentContext, Principal, Role};
use spot_permissions::{DocFacts, Snapshot};

// ---------------------------------------------------------------- fixtures

/// The five definitions of the app, parsed from the embedded TOML, sorted by
/// name (`load_dir` order).
pub fn definitions() -> Vec<spot_meta::Definition> {
    let mut defs: Vec<spot_meta::Definition> = spot_sample_app::DEFINITION_TEXTS
        .iter()
        .map(|(name, text)| {
            spot_meta::Definition::parse(name, text)
                .unwrap_or_else(|e| panic!("{name} no longer parses: {e:?}"))
        })
        .collect();
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    defs
}

/// The five definitions plus the minimal `user` and `file` DocTypes the app's
/// links and `attach` fields name (the platform app ships the real ones;
/// WP-IAM-02.T2 and WP-PLT-10 are still pending).
pub fn definitions_with_user() -> Vec<spot_meta::Definition> {
    let mut defs = definitions();
    defs.push(
        spot_meta::Definition::parse(
            "user",
            r#"
name = "user"
label = "User"
app = "spot_platform"
module = "directory"
title_field = "user_name"

[[fields]]
name = "user_name"
label = "User Name"
type = "text"
max_chars = 140
required = true
unique = true

[[fields]]
name = "enabled"
label = "Enabled"
type = "bool"
default = "true"
"#,
        )
        .unwrap_or_else(|e| panic!("user fixture no longer parses: {e:?}")),
    );
    defs.push(
        spot_meta::Definition::parse(
            "file",
            r#"
name = "file"
label = "File"
app = "spot_platform"
module = "files"
title_field = "name"

[[fields]]
name = "name"
label = "Name"
type = "text"
max_chars = 140
required = true
"#,
        )
        .unwrap_or_else(|e| panic!("file fixture no longer parses: {e:?}")),
    );
    defs
}

/// The app's effective metadata over its definitions plus the `user` fixture.
pub fn registry() -> spot_meta::EffectiveMeta {
    spot_meta::Registry::new(definitions_with_user())
        .unwrap_or_else(|e| panic!("the app definitions no longer build: {e:?}"))
        .effective()
        .as_ref()
        .clone()
}

/// The app's permission model, loaded from the embedded texts.
pub fn permission_model() -> spot_permissions::model::Model {
    spot_permissions::load(&spot_permissions::model::ModelTexts {
        grants: spot_sample_app::permissions::PERMISSION_TEXTS
            .grants
            .to_owned(),
        field_grants: spot_sample_app::permissions::PERMISSION_TEXTS
            .field_grants
            .to_owned(),
        groups: spot_sample_app::permissions::PERMISSION_TEXTS
            .groups
            .to_owned(),
        shares: spot_sample_app::permissions::PERMISSION_TEXTS
            .shares
            .to_owned(),
    })
    .unwrap_or_else(|e| panic!("the app permission model no longer loads: {e:?}"))
}

/// The confidential-group field facts, the way the platform's metadata adapter
/// will stamp them (the metadata-side group declaration is WP-MET-01.T6's).
fn confidential_details() -> Vec<spot_permissions::model::FieldFacts> {
    vec![spot_permissions::model::FieldFacts {
        path: "details".to_owned(),
        groups: vec![spot_permissions::model::GroupName(
            "confidential".to_owned(),
        )],
        tags: vec![],
        mask: None,
    }]
}

/// The engine over the app's snapshot, with the confidential facts stamped.
pub fn engine() -> spot_permissions::PermissionEngine {
    let meta = registry();
    let model = permission_model();
    let snapshot = spot_permissions::Snapshot::build(&meta, &model)
        .unwrap_or_else(|e| panic!("the app grants must fit the app metadata: {e:?}"));
    let snapshot = snapshot
        .with_field_facts("task", confidential_details())
        .unwrap_or_else(|e| panic!("the confidential facts must fit: {e:?}"));
    spot_permissions::PermissionEngine::new(std::sync::Arc::new(snapshot))
}

// ----------------------------------------------------------------- metadata

fn name_of(s: &str) -> spot_storage_contract::Name {
    spot_storage_contract::Name::new(s).unwrap()
}

/// All five DocTypes load under `Registry` with no errors, with the `user`
/// fixture standing in for the platform app; the brief calls this out.
#[test]
fn the_app_definitions_build_against_the_registry() {
    let meta = registry();
    let names: Vec<&str> = meta.doctypes().map(|d| d.name.as_str()).collect();
    for expected in ["checklist_item", "label", "project", "task", "time_entry"] {
        assert!(names.contains(&expected), "{expected} must load: {names:?}");
    }
    assert!(names.contains(&"user"), "the user fixture loads");
}

/// The storage catalog compiles: the shared decimal shape, the multiselect
/// child, the unique indexes and the link targets reach the storage view.
#[test]
fn the_app_compiles_to_a_storage_catalog() {
    let meta = registry();
    let cat = meta
        .catalog(spot_storage_contract::schema::Collation::new("en").unwrap())
        .unwrap_or_else(|e| panic!("catalog: {e}"));
    let task = cat
        .entity(&name_of("task"))
        .unwrap_or_else(|e| panic!("task entity: {e}"));
    let logged = task.field(&name_of("logged_hours")).unwrap();
    assert_eq!(
        logged.ty,
        spot_storage_contract::schema::FieldType::Decimal {
            precision: 18,
            scale: 2
        }
    );
    let project = cat
        .entity(&name_of("project"))
        .unwrap_or_else(|e| panic!("project: {e}"));
    assert!(
        project
            .children
            .iter()
            .any(|c| c.field.as_str() == "labels" && c.entity.as_str() == "label_pick"),
        "labels rides the label_pick child: {:?}",
        project.children
    );
}

/// The app view carries the field shapes the brief fixes: the read-only derived
/// field, the dynamic link, the filtered assignee lookup, and the shared
/// decimal between `estimated_hours` and `hours`.
#[test]
fn the_app_view_carries_the_field_shapes() {
    let meta = registry();
    let task = meta.doctype("task").unwrap_or_else(|| panic!("task"));
    let logged = task
        .field("logged_hours")
        .unwrap_or_else(|| panic!("logged"));
    assert!(logged.read_only);
    let related = task.field("related_to").map(|f| f.kind.clone());
    use spot_app_api::meta::FieldKind as K;
    assert!(
        matches!(&related, Some(K::DynamicLink { type_field }) if type_field.as_str() == "related_type"),
        "related_to is the dynamic link over related_type: {related:?}"
    );
    let est = task.field("estimated_hours").map(|f| f.kind.clone());
    let te = meta.doctype("time_entry").unwrap_or_else(|| panic!("te"));
    let hours = te.field("hours").map(|f| f.kind.clone());
    assert_eq!(
        est, hours,
        "estimated_hours and hours share the number format"
    );
    assert!(
        matches!(
            &hours,
            Some(K::Decimal {
                scale: 2,
                precision: 18,
                ..
            })
        ),
        "{hours:?}"
    );
    let assignee = task.field("assignee").unwrap_or_else(|| panic!("assignee"));
    let lookup = assignee.lookup.as_ref().unwrap_or_else(|| panic!("lookup"));
    assert_eq!(
        lookup.params.get("enabled_only").map(String::as_str),
        Some("true")
    );
}

// ------------------------------------------------------- seeds and catalogue

/// The seed row counts match the brief (8 labels, 2 projects, 6 tasks, 5 time
/// entries) and every table parses.
#[test]
fn the_seed_counts_match_the_brief() {
    #[derive(serde::Deserialize)]
    struct Seed {
        labels: Vec<toml::Table>,
        projects: Vec<toml::Table>,
        tasks: Vec<toml::Table>,
        time_entries: Vec<toml::Table>,
    }
    let seed: Seed =
        toml::from_str(spot_sample_app::SEED_TEXT).unwrap_or_else(|e| panic!("seed: {e}"));
    assert_eq!(seed.labels.len(), 8);
    assert_eq!(seed.projects.len(), 2);
    assert_eq!(seed.tasks.len(), 6);
    assert_eq!(seed.time_entries.len(), 5);
}

/// Every seed value the metadata knows fits: the hour bounds, the select
/// values, the colour format, the ISO dates, and the references that resolve
/// inside the file; a Blocked task carries its reason, others none.
#[test]
fn the_seed_validates_against_the_metadata() {
    let seed: BTreeMap<String, toml::Value> =
        toml::from_str(spot_sample_app::SEED_TEXT).unwrap_or_else(|e| panic!("seed: {e}"));
    let labels: Vec<&toml::Table> = seed["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(toml::Value::as_table)
        .collect::<Option<Vec<_>>>()
        .unwrap();
    let label_ids: Vec<&str> = labels.iter().map(|l| l["id"].as_str().unwrap()).collect();
    let colour = labels
        .iter()
        .all(|l| l["colour"].as_str().unwrap().starts_with('#'));

    let projects: Vec<&toml::Table> = seed["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(toml::Value::as_table)
        .collect::<Option<Vec<_>>>()
        .unwrap();
    let project_ids: Vec<&str> = projects.iter().map(|p| p["id"].as_str().unwrap()).collect();
    let project_statuses = ["Planning", "Active", "On Hold", "Done"];
    let currencies = ["USD", "EUR", "GBP", "PKR"];
    assert!(projects.iter().all(|p| {
        let ok_statuses = project_statuses.contains(&p["status"].as_str().unwrap());
        let ok_money = p["budget"].as_str().unwrap().parse::<Decimal>().is_ok();
        let ok_cur = currencies.contains(&p["currency"].as_str().unwrap());
        let ok_labels = p["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .all(|l| label_ids.contains(&l));
        ok_statuses && ok_money && ok_cur && ok_labels
    }));

    let tasks: Vec<&toml::Table> = seed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(toml::Value::as_table)
        .collect::<Option<Vec<_>>>()
        .unwrap();
    let task_ids: Vec<&str> = tasks.iter().map(|t| t["id"].as_str().unwrap()).collect();
    let task_statuses = ["Open", "In Progress", "Blocked", "Review", "Done"];
    let priorities = ["Low", "Medium", "High", "Urgent"];
    let ok = tasks.iter().all(|t| {
        let ok_status = task_statuses.contains(&t["status"].as_str().unwrap());
        let ok_priority = priorities.contains(&t["priority"].as_str().unwrap());
        let ok_project = project_ids.contains(&t["project"].as_str().unwrap());
        let ok_estimate = t["estimated_hours"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .is_ok();
        let blocked = t["status"].as_str().unwrap() == "Blocked";
        let ok_reason = blocked == t.contains_key("blocked_reason");
        let ok_labels = t["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .all(|l| label_ids.contains(&l));
        ok_status && ok_priority && ok_project && ok_estimate && ok_reason && ok_labels
    });
    assert!(ok, "every task's values fit its metadata");
    // Dynamic links point at a row of project or task.
    assert!(tasks.iter().all(|t| match t.get("related_type") {
        None => true,
        Some(rt) => {
            let target = t.get("related_to").map(|v| v.as_str().unwrap());
            match rt.as_str().unwrap() {
                "project" => target.is_some_and(|x| project_ids.contains(&x)),
                "task" => target.is_some_and(|x| task_ids.contains(&x)),
                other => panic!("unknown related_type {other}"),
            }
        }
    }));

    let entries: Vec<&toml::Table> = seed["time_entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(toml::Value::as_table)
        .collect::<Option<Vec<_>>>()
        .unwrap();
    let (low, high) = (
        spot_sample_app::constants::HOURS_BOUNDS.0,
        spot_sample_app::constants::HOURS_BOUNDS.1,
    );
    let ok_entries = entries.iter().all(|e| {
        let hours = e["hours"].as_str().unwrap().parse::<Decimal>().unwrap();
        let ok_bounds =
            hours >= low.parse::<Decimal>().unwrap() && hours <= high.parse::<Decimal>().unwrap();
        let ok_task = task_ids.contains(&e["task"].as_str().unwrap());
        let ok_date = e["work_date"].as_str().unwrap().len() == 10;
        ok_bounds && ok_task && ok_date
    });
    assert!(ok_entries, "every entry's values fit its metadata");
    assert!(colour, "every label colour is a colour literal");
}

/// The sample codes sit in the catalogue file (the repo-wide scan proves the
/// raised direction; this proves the file carries the app's vocabulary).
#[test]
fn the_sample_codes_sit_in_the_catalogue() {
    let text = include_str!("../../../../../../crates/framework/spot-errors/catalogue/sample.toml");
    for code in [
        spot_sample_app::catalogue::HOURS_OUT_OF_RANGE,
        spot_sample_app::catalogue::HOURS_REQUIRED,
        spot_sample_app::catalogue::OWNER_FROM_PROJECT,
        "sample.invalid_name",
        "sample.roll_up_no_version",
    ] {
        assert!(text.contains(code), "{code} is in the catalogue file");
    }
}

// --------------------------------------------------------------- permissions

/// The document facts of one document of `doctype`, created by `creator`, in
/// `state` (a Time Entry runs the posted lifecycle; the others have none).
#[cfg(test)]
fn facts(doctype: &str, creator: Option<&str>, state: Option<&str>) -> DocFacts {
    DocFacts {
        doctype: doctype.to_owned(),
        id: None,
        created_by: creator.map(str::to_owned),
        state: state.map(str::to_owned),
        values: BTreeMap::new(),
    }
}

#[cfg(test)]
fn state_of(doctype: &str) -> Option<&'static str> {
    (doctype == "time_entry").then_some("draft")
}

/// The decision table over the lifecycle actions: the manager holds everything
/// on all five, the viewer reads only, the member reads all and creates Tasks
/// and Time Entries, submits and edits only their own entries, and deletes
/// nothing. Six principals (manager, member, viewer, nobody, system, and the
/// scoped manager of the next test) cross five DocTypes x six actions.
#[test]
fn the_decision_table_holds() {
    let engine = engine();
    let manager = Principal::with_roles("manager1", &["Project Manager"]);
    let member = Principal::with_roles("member1", &["Team Member"]);
    let viewer = Principal::with_roles("viewer1", &["Viewer"]);
    let nobody = Principal::with_roles("ghost", &[]);
    let system = Principal::system();
    let doctypes = ["project", "task", "time_entry", "label", "checklist_item"];
    let create = Action::Op(spot_app_api::Operation::Create);
    let update = Action::Op(spot_app_api::Operation::Update);
    let del = Action::Op(spot_app_api::Operation::Delete);
    let submit = Action::Op(spot_app_api::Operation::Post);
    let cancel = Action::Op(spot_app_api::Operation::Cancel);
    let actions = [Action::Read, create, update, del, submit, cancel];

    for &dt in &doctypes {
        let someone_elses = facts(dt, Some("someone_else"), state_of(dt));
        // System: allowed everywhere (ADR-51 phases 3–4 run committed steps as
        // the system principal).
        for &a in &actions {
            assert!(
                engine.can(&system, a, &someone_elses).allowed,
                "system may {a:?} on {dt}"
            );
        }
        // Nobody: denied everywhere.
        for &a in &actions {
            assert!(
                !engine.can(&nobody, a, &someone_elses).allowed,
                "nobody may not {a:?} on {dt}"
            );
        }
        // Viewer: read, nothing else, on any owner's documents.
        assert!(
            engine.can(&viewer, Action::Read, &someone_elses).allowed,
            "{dt}"
        );
        for &a in &actions[1..] {
            assert!(
                !engine.can(&viewer, a, &someone_elses).allowed,
                "viewer may not {a:?} on {dt}"
            );
        }
        // Manager: everything, on anyone's documents — with the state rules
        // (ADR-42): post needs a draft, cancel needs a posted document, and
        // only `time_entry` has a lifecycle at all.
        for &a in &actions {
            let expected = match a {
                Action::Op(spot_app_api::Operation::Post) => dt == "time_entry",
                // Cancel on a draft is the state refusal; the manager's cancel
                // right is proven against posted documents in the field tests.
                Action::Op(spot_app_api::Operation::Cancel) => false,
                _ => true,
            };
            assert_eq!(
                engine.can(&manager, a, &someone_elses).allowed,
                expected,
                "the manager {a:?} on {dt}"
            );
        }
        // The manager cancels a posted Time Entry (the ADR-42 state that holds
        // the cancel right) — only Time Entry carries the cancel right and a
        // lifecycle at all, so this check belongs to its branch.
        if dt == "time_entry" {
            let posted = DocFacts {
                state: Some("posted".to_owned()),
                ..someone_elses.clone()
            };
            assert!(
                engine
                    .can(
                        &manager,
                        Action::Op(spot_app_api::Operation::Cancel),
                        &posted
                    )
                    .allowed,
                "the manager cancels a posted entry"
            );
        }
        // Member: read everywhere; create only Tasks and Time Entries; never
        // delete; update and submit only their own Time Entries.
        let own = facts(dt, Some("member1"), state_of(dt));
        if matches!(dt, "task" | "time_entry") {
            assert!(
                engine.can(&member, create, &own).allowed,
                "member creates {dt}"
            );
        } else {
            assert!(
                !engine.can(&member, create, &own).allowed,
                "member creates no {dt}"
            );
        }
        assert!(
            !engine.can(&member, del, &own).allowed,
            "member deletes no {dt}"
        );
        if dt == "time_entry" {
            assert!(
                engine.can(&member, update, &own).allowed,
                "member edits own entry"
            );
            assert!(
                engine.can(&member, submit, &own).allowed,
                "member submits own entry"
            );
            assert!(
                !engine.can(&member, update, &someone_elses).allowed,
                "member edits no other's entry"
            );
            assert!(
                !engine.can(&member, submit, &someone_elses).allowed,
                "member submits no other's entry"
            );
            assert!(
                !engine.can(&member, cancel, &someone_elses).allowed,
                "member cancels nothing"
            );
        } else if dt == "task" {
            // Tasks: the member writes any task (the grant carries no owner
            // rule); submit and cancel are not the Task's operations.
            assert!(
                engine.can(&member, update, &someone_elses).allowed,
                "member edits tasks"
            );
            assert!(
                !engine.can(&member, submit, &someone_elses).allowed,
                "tasks do not post"
            );
        } else {
            assert!(!engine.can(&member, update, &someone_elses).allowed, "{dt}");
        }
    }
    let _ = spot_permissions::engine::FIELD_DENIED;
}

/// The scoped manager: an assignment whose scope is `task.project =
/// prj_platform` contributes nothing on a task of another project and grants
/// inside it — scope belongs to the assignment (ADR-070 section 3).
#[test]
fn the_scoped_manager_assignment_covers_only_its_project() {
    let engine = engine();
    let scoped = Principal::user(
        "manager_a",
        vec![AssignmentContext {
            id: "a".to_owned(),
            roles: vec![Role("Project Manager".to_owned())],
            scope: vec![spot_permissions::model::UserPermission {
                user: String::new(),
                doctype: None,
                field_or_link_doctype: "task.project".to_owned(),
                allowed: vec!["prj_platform".to_owned()],
                applies_to: spot_permissions::model::AppliesTo::AllDoctypes,
                include_descendants: false,
            }],
        }],
    );
    let update = Action::Op(spot_app_api::Operation::Update);
    let inside = DocFacts {
        doctype: "task".to_owned(),
        id: None,
        created_by: None,
        state: None,
        values: BTreeMap::from([(
            "project".to_owned(),
            spot_app_api::prelude::Value::Text("prj_platform".to_owned()),
        )]),
    };
    assert!(engine.can(&scoped, update, &inside).allowed);
    let outside = DocFacts {
        values: BTreeMap::from([(
            "project".to_owned(),
            spot_app_api::prelude::Value::Text("prj_other".to_owned()),
        )]),
        ..inside
    };
    assert!(!engine.can(&scoped, update, &outside).allowed);
    assert!(!engine.can(&scoped, Action::Read, &outside).allowed);
}

/// The field security of `task.details`: only the manager reads (and writes)
/// the `confidential` group; the member and the viewer hold nothing in it.
#[test]
fn the_confidential_group_hides_details_from_everyone_but_managers() {
    let engine = engine();
    let member = Principal::with_roles("member1", &["Team Member"]);
    let viewer = Principal::with_roles("viewer1", &["Viewer"]);
    let manager = Principal::with_roles("manager1", &["Project Manager"]);
    let details = &engine
        .snapshot()
        .facts("task")
        .and_then(|f| f.fields.iter().find(|x| x.path == "details").cloned())
        .unwrap_or_else(|| panic!("details facts"));
    let doc = facts("task", Some("member1"), None);
    for p in [&member, &viewer] {
        let d = engine.field_access(p, "task", details, &doc);
        assert!(
            matches!(d.level, spot_permissions::engine::FieldLevel::Hidden),
            "{} must not read details: {:?}",
            p.user,
            d
        );
    }
    let d = engine.field_access(&manager, "task", details, &doc);
    assert!(
        matches!(d.level, spot_permissions::engine::FieldLevel::Read),
        "the manager reads details: {:?}",
        manager.user
    );
}

/// The grant rows hold the `if_owner` shape: exactly one Team Member write
/// grant on Time Entry, and it is `if_owner`.
#[test]
fn the_grant_rows_hold_the_if_owner_shape() {
    let model = permission_model();
    let writes: Vec<&spot_permissions::model::RoleGrant> = model
        .grants
        .iter()
        .filter(|g| {
            g.doctype == "time_entry"
                && g.role.as_str() == "Team Member"
                && g.rights.contains(spot_permissions::model::Right::Write)
        })
        .collect();
    assert_eq!(writes.len(), 1, "exactly one write grant on time_entry");
    assert!(writes[0].if_owner, "and it is if_owner");
    let submits: Vec<&spot_permissions::model::RoleGrant> = model
        .grants
        .iter()
        .filter(|g| {
            g.doctype == "time_entry"
                && g.role.as_str() == "Team Member"
                && g.rights.contains(spot_permissions::model::Right::Submit)
        })
        .collect();
    assert_eq!(submits.len(), 1, "exactly one submit grant on time_entry");
    assert!(submits[0].if_owner, "and it is if_owner too");
}

/// The snapshot builds with the group declared.
#[test]
fn the_permissions_snapshot_builds() {
    let meta = registry();
    let model = permission_model();
    let snapshot = Snapshot::build(&meta, &model).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(snapshot.doctypes().count() >= 6);
    assert!(snapshot.has_group(&spot_permissions::model::GroupName(
        "confidential".to_owned()
    )));
}

// ----------------------------------------------------------------- typeviews

/// The typed-view generator produces the views for the app's DocTypes.
#[test]
fn the_typed_views_generate() {
    let defs = definitions_with_user();
    let source = spot_typed_views::generate(&defs).unwrap_or_else(|e| panic!("generate: {e:?}"));
    for dt in [
        "Project",
        "Task",
        "TimeEntry",
        "Label",
        "ChecklistItem",
        "User",
    ] {
        assert!(
            source.contains(&format!("pub struct {dt}(Document)")),
            "the view of {dt} is in the generated source"
        );
    }
    assert!(source.contains("pub fn hours(&self)"), "the hours getter");
    assert!(
        source.contains("pub fn logged_hours(&self)"),
        "the logged-hours getter"
    );
}
