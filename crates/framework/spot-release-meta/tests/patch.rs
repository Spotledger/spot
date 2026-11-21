#![allow(missing_docs)]

//! The patch declaration: parse, render, and the bridge to the plan engine's patch row.

use spot_migration_plan::{
    AppVersion, ManifestSummary, PatchDecl as PlanPatchDecl, Risk, SystemDescription,
};
use spot_release_meta::{Error, PatchDecl, PatchRef, codes, patches_on_disk};

const VALID: &str = "\
id = \"acme_0001\"
intent = \"Add the Acme field to the invoice\"
doctypes = [\"Sales Invoice\"]
class = \"additive\"
reversible = true
idempotent = true
";

fn expect_err(result: Result<PatchDecl, Error>) -> Error {
    match result {
        Ok(decl) => panic!("expected an error, got {decl:?}"),
        Err(error) => error,
    }
}

#[test]
fn parses_a_valid_declaration() {
    let decl = match PatchDecl::parse(VALID) {
        Ok(decl) => decl,
        Err(error) => panic!("valid declaration refused: {error}"),
    };
    assert_eq!(decl.id, "acme_0001");
    assert_eq!(decl.intent, "Add the Acme field to the invoice");
    assert_eq!(decl.doctypes, vec!["Sales Invoice".to_owned()]);
    assert_eq!(decl.class, Risk::Additive);
    assert!(decl.reversible);
    assert!(decl.idempotent);
    assert_eq!(decl.down, None);
    assert!(!decl.requires_epoch);
    assert!(!decl.irreversible_ack);
    assert_eq!(decl.guard, None);
}

#[test]
fn render_is_deterministic_and_round_trips() {
    let decl = match PatchDecl::parse(VALID) {
        Ok(decl) => decl,
        Err(error) => panic!("valid declaration refused: {error}"),
    };
    let text = decl.render();
    let again = match PatchDecl::parse(&text) {
        Ok(again) => again,
        Err(error) => panic!("rendered declaration refused: {error}"),
    };
    assert_eq!(decl, again);
    assert_eq!(text, again.render(), "render is not stable");
}

#[test]
fn doctypes_are_sorted_and_deduplicated() {
    let decl = match PatchDecl::parse(
        "id = \"p\"\nintent = \"x\"\ndoctypes = [\"b\", \"a\", \"b\"]\nclass = \"additive\"\nreversible = true\nidempotent = true\n",
    ) {
        Ok(decl) => decl,
        Err(error) => panic!("valid declaration refused: {error}"),
    };
    assert_eq!(decl.doctypes, vec!["a".to_owned(), "b".to_owned()]);
}

#[test]
fn an_empty_intent_is_refused() {
    let error = expect_err(PatchDecl::parse(
        "id = \"p\"\nintent = \"  \"\nclass = \"additive\"\nreversible = true\nidempotent = true\n",
    ));
    assert_eq!(error.code(), codes::BAD_PATCH);
}

#[test]
fn an_unknown_field_is_refused() {
    let error = expect_err(PatchDecl::parse(
        "id = \"p\"\nintent = \"x\"\nclass = \"additive\"\nreversible = true\nidempotent = true\nsurprise = 1\n",
    ));
    assert_eq!(error.code(), codes::BAD_PATCH);
}

#[test]
fn a_bad_class_is_refused() {
    let error = expect_err(PatchDecl::parse(
        "id = \"p\"\nintent = \"x\"\nclass = \"spicy\"\nreversible = true\nidempotent = true\n",
    ));
    assert_eq!(error.code(), codes::BAD_PATCH);
}

#[test]
fn a_file_name_must_match_the_id() {
    let error = expect_err(PatchDecl::from_file(
        "release-meta/patches/other.toml",
        VALID,
    ));
    assert_eq!(error.code(), codes::BAD_PATCH);
    let ok = PatchDecl::from_file("release-meta/patches/acme_0001.toml", VALID);
    assert!(ok.is_ok());
}

#[test]
fn a_down_step_and_a_guard_are_read() {
    let decl = match PatchDecl::parse(
        "id = \"p\"\nintent = \"x\"\nclass = \"needs_review\"\nreversible = true\nidempotent = false\n\
         down = { id = \"p_down\" }\nguard = \"insert only when the marker is absent\"\nrequires_epoch = true\n",
    ) {
        Ok(decl) => decl,
        Err(error) => panic!("valid declaration refused: {error}"),
    };
    assert_eq!(decl.down, Some(PatchRef::new("p_down")));
    assert_eq!(
        decl.guard.as_deref(),
        Some("insert only when the marker is absent")
    );
    assert!(decl.requires_epoch);
}

#[test]
fn the_plan_row_round_trips() {
    let decl = match PatchDecl::parse(VALID) {
        Ok(decl) => decl,
        Err(error) => panic!("valid declaration refused: {error}"),
    };
    let row = decl.to_plan_decl();
    assert_eq!(row.id, decl.id);
    assert_eq!(row.intent.as_deref(), Some(decl.intent.as_str()));
    assert_eq!(row.reversible, decl.reversible);
    assert_eq!(row.idempotent, decl.idempotent);

    let rebuilt = PatchDecl::from_plan_decl(&row, decl.class, decl.doctypes.clone(), None, false);
    assert_eq!(rebuilt.id, decl.id);
    assert_eq!(rebuilt.intent, decl.intent);
    assert_eq!(rebuilt.class, decl.class);
    assert_eq!(rebuilt.doctypes, decl.doctypes);
}

#[test]
fn patches_on_disk_reads_the_manifests() {
    let row = PlanPatchDecl {
        id: "acme_0001".to_owned(),
        intent: Some("schema".to_owned()),
        reversible: true,
        idempotent: true,
        deps: Vec::new(),
    };
    let mut system = SystemDescription::new();
    system.apps.push(AppVersion {
        name: "acme".to_owned(),
        version: "1.0.0".to_owned(),
        manifest: ManifestSummary {
            patches: vec![row.clone()],
            ..ManifestSummary::default()
        },
    });
    assert_eq!(patches_on_disk(&system), vec![row]);
}
