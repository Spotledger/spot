//! The installed-app checks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{definition, field, meta, release, system};
use semver::Version;
use spot_manifest::AppManifest;
use spot_meta::{Customisations, FieldType};
use spot_migration_conflicts::{
    ArtefactKind, HostSpec, InstalledApp, ReleaseContext, codes, detect,
};

/// Parses a manifest, failing loudly on a bad sample.
#[track_caller]
fn manifest(toml: &str) -> AppManifest {
    match spot_manifest::parse(toml, "spot-app.toml") {
        Ok(manifest) => manifest,
        Err(errors) => panic!("the sample manifest must parse: {errors:?}"),
    }
}

fn host() -> HostSpec {
    HostSpec::new(Version::new(0, 5, 0), "spot:host@1")
}

#[test]
fn an_app_targeting_another_wit_world_is_reported() {
    let definitions = vec![definition("party", vec![field("name", FieldType::Text)])];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let customisations = Customisations::new();
    let meta = meta(&definitions, &customisations);
    let app = InstalledApp::new(
        manifest("[app]\nname = \"erp_selling\"\nversion = \"0.3.1\"\n"),
        "spot:host@2",
    );
    let apps = [app];
    let host = host();
    let context =
        ReleaseContext::new(&plan, &definitions, &meta, &customisations).with_apps(&apps, &host);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::APP_WIT_WORLD_MISMATCH);
    assert_eq!(item.artefacts[0].kind, ArtefactKind::App);
    assert_eq!(item.artefacts[0].id, "erp_selling");
}

#[test]
fn an_app_that_needs_a_removed_doctype_is_reported() {
    let from = vec![
        definition("party", vec![field("name", FieldType::Text)]),
        definition("legacy", vec![field("code", FieldType::Text)]),
    ];
    let to = vec![definition("party", vec![field("name", FieldType::Text)])];
    let new_standard = to.clone();
    let plan = release(&system(&from, 1), &system(&to, 2));
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let app = InstalledApp::new(
        manifest(
            "[app]\nname = \"erp_selling\"\nversion = \"0.3.1\"\n\n\
             [capabilities]\ndoctypes = { read = [\"Legacy\"] }\n",
        ),
        "spot:host@1",
    );
    let apps = [app];
    let host = host();
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_apps(&apps, &host);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::APP_DOCTYPE_REMOVED);
}

#[test]
fn an_app_watching_a_removed_field_is_reported() {
    let from = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("internal_note", FieldType::Text),
        ],
    );
    let to = definition("party", vec![field("name", FieldType::Text)]);
    let new_standard = vec![to.clone()];
    let plan = release(&system(&[from], 1), &system(&[to], 2));
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let app = InstalledApp::new(
        manifest(
            "[app]\nname = \"erp_selling\"\nversion = \"0.3.1\"\n\n\
             [[events]]\ndoctype = \"Party\"\nevent = \"field_changed:internal_note\"\n\
             step = \"erp_selling:recheck\"\n",
        ),
        "spot:host@1",
    );
    let apps = [app];
    let host = host();
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_apps(&apps, &host);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::APP_HOOK_REMOVED);
}

#[test]
fn an_app_that_rejects_the_new_host_is_reported() {
    let definitions = vec![definition("party", vec![field("name", FieldType::Text)])];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let customisations = Customisations::new();
    let meta = meta(&definitions, &customisations);
    let app = InstalledApp::new(
        manifest(
            "[app]\nname = \"erp_selling\"\nversion = \"0.3.1\"\nrequires_host = \">=0.1, <0.2\"\n",
        ),
        "spot:host@1",
    );
    let apps = [app];
    let host = host();
    let context =
        ReleaseContext::new(&plan, &definitions, &meta, &customisations).with_apps(&apps, &host);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::APP_HOST_INCOMPATIBLE);
}

#[test]
fn a_compatible_app_is_not_a_conflict() {
    let definitions = vec![definition("party", vec![field("name", FieldType::Text)])];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let customisations = Customisations::new();
    let meta = meta(&definitions, &customisations);
    let app = InstalledApp::new(
        manifest(
            "[app]\nname = \"erp_selling\"\nversion = \"0.3.1\"\nrequires_host = \">=0.1\"\n\n\
             [capabilities]\ndoctypes = { read = [\"Party\"] }\n",
        ),
        "spot:host@1",
    );
    let apps = [app];
    let host = host();
    let context =
        ReleaseContext::new(&plan, &definitions, &meta, &customisations).with_apps(&apps, &host);
    assert!(detect(&context).is_empty());
}
