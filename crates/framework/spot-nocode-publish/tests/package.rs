//! Packaging acceptance: a draft becomes a deterministic, validator-approved `.spotapp` that
//! carries only the selected content, and bad drafts are refused with plain messages.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle::{Bundle, Policy};
use spot_nocode_publish::{EMPTY_COMPONENT, Error, ItemRef, package, package_with_policy};

#[test]
fn a_draft_packages_deterministically_and_carries_only_its_items() {
    let site = fixture_site();
    let draft = full_draft();
    let source = package(&draft, &site).expect("the draft packages");

    let manifest = source.manifest();
    assert_eq!(manifest.app.name.as_str(), "no_code_crm");
    assert_eq!(manifest.app.version.to_string(), "1.0.0");
    assert_eq!(manifest.app.title, "No-code CRM");
    assert!(manifest.app.requires_host.is_some());
    assert_eq!(manifest.app.depends.len(), 1, "the crm dependency travels");
    assert_eq!(manifest.assets.len(), 2, "both payload assets are declared");
    let paths: Vec<&str> = manifest
        .assets
        .iter()
        .map(|asset| asset.path.as_str())
        .collect();
    assert!(paths.contains(&spot_nocode_publish::CUSTOMISATION_ASSET));
    assert!(paths.contains(&spot_nocode_publish::DECLARATIVE_ASSET));
    assert!(
        manifest
            .capabilities
            .doctypes
            .read
            .contains(&"customer".to_owned()),
        "the declared DocTypes are in the manifest"
    );
    assert!(
        manifest
            .capabilities
            .doctypes
            .write
            .contains(&"asset".to_owned()),
        "a designed DocType is declared too"
    );
    assert!(
        source.bundle().component_imports().is_empty(),
        "the empty component imports nothing"
    );
    assert_eq!(
        source.bundle().wasm(),
        EMPTY_COMPONENT,
        "a no-code app carries the honest empty component"
    );

    let declarative = source.declarative();
    assert_eq!(declarative.app.publisher, PUBLISHER);
    assert_eq!(declarative.app.licence_ref.as_deref(), Some("SPDX:MIT"));
    assert_eq!(
        declarative.doctypes.len(),
        1,
        "the designed DocType travels"
    );
    assert_eq!(declarative.doctypes[0].name, "asset");
    assert_eq!(declarative.forms.len(), 1, "its designed form travels");
    assert_eq!(declarative.processes[0].name, "onboarding");
    assert_eq!(declarative.reports[0].name, "tiers");

    let customisation = open_customisation(source.customisation());
    for kind in ["meta", "views", "scripts", "rules", "fixtures"] {
        assert!(
            customisation.section(kind).is_some(),
            "the selected {kind} content is captured"
        );
    }
    let fixtures = customisation
        .section("fixtures")
        .expect("fixtures captured");
    assert!(
        fixtures.files.contains_key("customer.json"),
        "the declared fixture rows travel"
    );
    assert!(
        !fixtures.files.contains_key("sales_invoice.json"),
        "rows for an undeclared DocType never travel"
    );

    // The same inputs always give the same bytes: entry order, timestamps and compression are
    // fixed by the bundle builder, and the captured sections are canonical.
    let second = package(&draft, &site).expect("the draft packages again");
    assert_eq!(source.bytes(), second.bytes(), "packaging is deterministic");
    assert_eq!(source.digest(), second.digest());
}

#[test]
fn a_designed_draft_missing_an_item_is_refused_by_name() {
    let site = fixture_site();
    let mut draft = full_draft();
    draft.items.push(ItemRef::Script {
        name: "missing_script".to_owned(),
    });
    let error = package(&draft, &site).expect_err("the missing script is refused");
    assert_eq!(
        error,
        Error::MissingItem {
            item: "script:missing_script".to_owned()
        }
    );
    assert!(
        error.to_string().contains("script:missing_script"),
        "the message names the item: {error}"
    );
}

#[test]
fn a_malformed_process_definition_is_refused() {
    let site = fixture_site();
    let mut draft = full_draft();
    draft.items.push(ItemRef::Process {
        name: "broken".to_owned(),
        source: "this is not = [valid".to_owned(),
    });
    let error = package(&draft, &site).expect_err("the broken process is refused");
    assert_eq!(error.code(), "nocode_publish.bad_draft");
    assert!(error.to_string().contains("broken"), "{error}");
}

#[test]
fn an_invalid_app_name_is_refused() {
    let site = fixture_site();
    let draft =
        spot_nocode_publish::AppDraft::new("No Code CRM", semver::Version::new(1, 0, 0), PUBLISHER);
    let error = package(&draft, &site).expect_err("the name is refused");
    assert_eq!(error.code(), "nocode_publish.bad_draft");
}

#[test]
fn a_host_the_manifest_does_not_support_is_refused() {
    let site = fixture_site();
    let mut draft = full_draft();
    draft.requires_host = Some(semver::VersionReq::parse(">=9, <10").expect("a valid requirement"));
    let error = package(&draft, &site).expect_err("the host version is refused");
    assert_eq!(error.code(), "nocode_publish.bad_bundle");
    assert!(
        error.to_string().contains("bundle.host_required"),
        "the validator's code reaches the message: {error}"
    );
}

#[test]
fn an_over_privileged_bundle_is_refused_by_the_policy() {
    let site = fixture_site();
    let draft = full_draft();
    let policy = Policy {
        max_write_doctypes: Some(0),
        ..Policy::new()
    };
    let error =
        package_with_policy(&draft, &site, &policy).expect_err("the policy refuses the writes");
    assert_eq!(error.code(), "nocode_publish.over_privileged");
    assert!(
        error.to_string().contains("bundle.over_privileged"),
        "the message names the capability: {error}"
    );

    // The same package is acceptable to the default policy, so the refusal above is the
    // policy's, not a malformed bundle.
    let source = package(&draft, &site).expect("the default policy accepts it");
    Bundle::open(
        source.bytes(),
        &spot_bundle::Limits::default(),
        &bundle_host(),
    )
    .expect("the bundle validates");
}
