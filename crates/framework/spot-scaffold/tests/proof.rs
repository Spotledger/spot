//! The proof (WP-DX-01.T1 deliverable 5): 24 argument combinations — every flag, odd names,
//! a name at the length limit, a label with spaces and a unicode label — are generated into a
//! temporary directory and then run through `spot-pack`, `spot-meta`, `spot-permissions`,
//! `spot-controllers` and `spot-ts-transpile`. A template that rots when a format changes
//! fails this test.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;

use spot_scaffold::generate::app::AppSpec;
use spot_scaffold::generate::controller::ControllerSpec;
use spot_scaffold::generate::doctype::DoctypeSpec;
use spot_scaffold::generate::script::ScriptSpec;
use spot_scaffold::names::{AppName, DocTypeName};
use support::{prove, refused, temp_project, write};

/// A spec for a valid app name.
fn app_spec(name: &str) -> AppSpec {
    AppSpec::new(AppName::parse(name).expect("a valid app name"))
}

/// Generates and proves one good case.
fn good(build: impl FnOnce(&Path)) {
    let dir = temp_project();
    build(dir.path());
    prove(dir.path());
}

/// Writes a plain app into `project`.
fn plain(project: &Path, name: &str) {
    let spec = app_spec(name);
    let tree = spot_scaffold::generate::app::generate(&spec).expect("generates");
    write(project, &tree, false);
}

/// Writes an app with the given flags.
fn app_with(project: &Path, name: &str, controller: bool, script: bool, examples: bool) {
    let mut spec = app_spec(name);
    spec.with_controller = controller;
    spec.with_script = script;
    spec.example_fields = examples;
    let tree = spot_scaffold::generate::app::generate(&spec).expect("generates");
    write(project, &tree, false);
}

/// Adds a DocType to the app in `project`.
fn add_doctype(project: &Path, app: &str, label: &str, name: Option<&str>, examples: bool) {
    let spec = DoctypeSpec {
        app: AppName::parse(app).expect("valid app"),
        label: label.to_owned(),
        name: name.map(str::to_owned),
        example_fields: examples,
        role: None,
    };
    let tree = spot_scaffold::generate::doctype::generate(project, &spec).expect("generates");
    write(project, &tree, true);
}

#[test]
fn twenty_four_combinations_generate_and_prove() {
    // 1. plain.
    good(|p| plain(p, "acme"));
    // 2. with a controller.
    good(|p| app_with(p, "acme", true, false, false));
    // 3. with a script.
    good(|p| app_with(p, "acme", false, true, false));
    // 4. with both.
    good(|p| app_with(p, "acme", true, true, false));
    // 5. example fields.
    good(|p| app_with(p, "acme", false, false, true));
    // 6. controller and example fields.
    good(|p| app_with(p, "acme", true, false, true));
    // 7. the longest valid app name.
    good(|p| plain(p, &"a".repeat(63)));
    // 8. the longest valid app name with both flags.
    good(|p| app_with(p, &"a".repeat(63), true, true, false));
    // 9. the shortest app name.
    good(|p| plain(p, "a"));
    // 10. underscores, both flags and example fields.
    good(|p| app_with(p, "app_one_two", true, true, true));
    // 11. a DocType with a spaced label.
    good(|p| {
        plain(p, "acme");
        add_doctype(p, "acme", "Sales Invoice", None, false);
    });
    // 12. a DocType with example fields.
    good(|p| {
        plain(p, "acme");
        add_doctype(p, "acme", "Sales Invoice", None, true);
    });
    // 13. a controller for the starter DocType.
    good(|p| {
        plain(p, "acme");
        let spec = ControllerSpec {
            app: AppName::parse("acme").unwrap(),
            doctype: DocTypeName::parse("note").unwrap(),
        };
        let tree = spot_scaffold::generate::controller::generate(p, &spec).expect("generates");
        write(p, &tree, true);
    });
    // 14. a script.
    good(|p| {
        plain(p, "acme");
        let spec = ScriptSpec {
            app: AppName::parse("acme").unwrap(),
            name: "ping".to_owned(),
        };
        let tree = spot_scaffold::generate::script::generate(p, &spec).expect("generates");
        write(p, &tree, true);
    });
    // 15. a DocType and its controller.
    good(|p| {
        plain(p, "acme");
        add_doctype(p, "acme", "Sales Invoice", None, false);
        let spec = ControllerSpec {
            app: AppName::parse("acme").unwrap(),
            doctype: DocTypeName::parse("sales_invoice").unwrap(),
        };
        let tree = spot_scaffold::generate::controller::generate(p, &spec).expect("generates");
        write(p, &tree, true);
    });
    // 16. a controller app that also gets a script.
    good(|p| {
        app_with(p, "acme", true, false, false);
        let spec = ScriptSpec {
            app: AppName::parse("acme").unwrap(),
            name: "ping".to_owned(),
        };
        let tree = spot_scaffold::generate::script::generate(p, &spec).expect("generates");
        write(p, &tree, true);
    });

    // The refusals: odd names and labels that must be refused, not silently folded.
    refused(
        AppName::parse("Sales").map(|_| unreachable_tree()),
        "scaffold.invalid_name",
    );
    refused(
        AppName::parse("1abc").map(|_| unreachable_tree()),
        "scaffold.invalid_name",
    );
    refused(
        AppName::parse("order").map(|_| unreachable_tree()),
        "scaffold.reserved_name",
    );
    refused(
        AppName::parse("doc_thing").map(|_| unreachable_tree()),
        "scaffold.reserved_name",
    );
    refused(
        AppName::parse("a-b").map(|_| unreachable_tree()),
        "scaffold.invalid_name",
    );
    refused(
        AppName::parse(&"a".repeat(64)).map(|_| unreachable_tree()),
        "scaffold.invalid_name",
    );
    // A unicode label cannot be derived to a storage name.
    {
        let dir = temp_project();
        plain(dir.path(), "acme");
        let spec = DoctypeSpec {
            app: AppName::parse("acme").unwrap(),
            label: "Café".to_owned(),
            name: None,
            example_fields: false,
            role: None,
        };
        refused(
            spot_scaffold::generate::doctype::generate(dir.path(), &spec),
            "scaffold.invalid_name",
        );
    }
    // A label that derives onto a reserved word.
    {
        let dir = temp_project();
        plain(dir.path(), "acme");
        let spec = DoctypeSpec {
            app: AppName::parse("acme").unwrap(),
            label: "Order".to_owned(),
            name: None,
            example_fields: false,
            role: None,
        };
        refused(
            spot_scaffold::generate::doctype::generate(dir.path(), &spec),
            "scaffold.reserved_name",
        );
    }
}

/// A tree that a test never reaches; keeps `refused` generic over the name parse.
fn unreachable_tree() -> spot_scaffold::tree::FileTree {
    spot_scaffold::tree::FileTree::new()
}
