//! Signing and publishing acceptance: a signed package resolves byte for byte from the store,
//! bad versions and signatures are refused, and private releases stay with their organisation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use semver::Version;
use spot_app_review::{ReviewConfig, Verdict};
use spot_nocode_publish::{
    Audience, Error, PublishOptions, Registry, check_version_bump, package, publish, review_app,
    sign,
};

#[test]
fn a_signed_package_publishes_and_resolves_from_the_store() {
    let site = fixture_site();
    let source = package(&install_draft(), &site).expect("package");
    let signed = sign(source, &signer(), NOW).expect("sign");
    let setup = setup();

    let receipt = publish(
        &signed,
        &setup.registry,
        &PublishOptions::store(NOW, CREATED_AT),
    )
    .expect("publish");
    assert_eq!(receipt.release.app.as_str(), "no_code_crm");
    assert_eq!(receipt.release.version, Version::new(1, 0, 0));
    assert_eq!(receipt.release.publisher, PUBLISHER);
    assert_eq!(receipt.audience, Audience::Store);
    assert_ne!(
        receipt.report.verdict,
        Verdict::Rejected,
        "the review report travels with the listing"
    );
    assert_eq!(
        receipt.report.digest.as_deref(),
        Some(signed.digest().to_hex().as_str())
    );

    let fetched = setup
        .registry
        .fetch("no_code_crm", &Version::new(1, 0, 0), NOW)
        .expect("resolve");
    assert_eq!(
        fetched.bytes,
        signed.bytes(),
        "the store returns the exact bytes"
    );
    assert_eq!(fetched.release.digest, signed.digest());
    assert_eq!(fetched.verified.publisher_id, PUBLISHER);
}

#[test]
fn the_same_version_cannot_be_published_twice() {
    let site = fixture_site();
    let setup = setup();
    let first = sign(
        package(&install_draft(), &site).expect("package"),
        &signer(),
        NOW,
    )
    .expect("sign");
    publish(
        &first,
        &setup.registry,
        &PublishOptions::store(NOW, CREATED_AT),
    )
    .expect("the first publish");

    let again = sign(
        package(&install_draft(), &site).expect("package"),
        &signer(),
        NOW,
    )
    .expect("sign");
    let error = publish(
        &again,
        &setup.registry,
        &PublishOptions::store(NOW, CREATED_AT),
    )
    .expect_err("a republish is refused");
    assert_eq!(
        error,
        Error::VersionExists {
            app: "no_code_crm".to_owned(),
            version: "1.0.0".to_owned(),
        }
    );
}

#[test]
fn a_lower_version_is_refused_before_the_registry() {
    let site = fixture_site();
    let setup = setup();
    let first = sign(
        package(&install_draft(), &site).expect("package"),
        &signer(),
        NOW,
    )
    .expect("sign");
    publish(
        &first,
        &setup.registry,
        &PublishOptions::store(NOW, CREATED_AT),
    )
    .expect("the first publish");

    let lower = sign(
        package(&draft_at(Version::new(0, 9, 0)), &site).expect("package"),
        &signer(),
        NOW,
    )
    .expect("sign");
    let error = publish(
        &lower,
        &setup.registry,
        &PublishOptions::store(NOW, CREATED_AT),
    )
    .expect_err("a lower version is refused");
    assert_eq!(error.code(), "nocode_publish.version_not_increasing");
}

#[test]
fn a_higher_version_publishes_next_to_the_first() {
    let site = fixture_site();
    let setup = setup();
    for version in [Version::new(1, 0, 0), Version::new(1, 1, 0)] {
        let signed = sign(
            package(&draft_at(version.clone()), &site).expect("package"),
            &signer(),
            NOW,
        )
        .expect("sign");
        publish(
            &signed,
            &setup.registry,
            &PublishOptions::store(NOW, CREATED_AT),
        )
        .unwrap_or_else(|error| panic!("{version} publishes: {error}"));
    }
    let mut versions = setup
        .registry
        .versions("no_code_crm", NOW)
        .expect("versions");
    versions.sort();
    assert_eq!(versions, vec![Version::new(1, 0, 0), Version::new(1, 1, 0)]);
}

#[test]
fn a_private_release_is_limited_to_the_publishers_organisation() {
    let site = fixture_site();
    let mut draft = install_draft();
    draft.name = "private_crm".to_owned();
    let signed = sign(package(&draft, &site).expect("package"), &signer(), NOW).expect("sign");

    let other = setup();
    let error = publish(
        &signed,
        &other.registry,
        &PublishOptions::private("other-org", NOW, CREATED_AT),
    )
    .expect_err("another organisation is refused");
    assert_eq!(
        error,
        Error::PrivateOtherOrganisation {
            publisher: PUBLISHER.to_owned(),
            organisation: "other-org".to_owned(),
        }
    );

    let own = setup();
    let receipt = publish(
        &signed,
        &own.registry,
        &PublishOptions::private(PUBLISHER, NOW, CREATED_AT),
    )
    .expect("the owner publishes privately");
    assert_eq!(
        receipt.audience,
        Audience::Private {
            organisation: PUBLISHER.to_owned()
        }
    );
}

#[test]
fn an_expired_certificate_is_refused_at_signing_time() {
    let site = fixture_site();
    let source = package(&install_draft(), &site).expect("package");
    let error =
        sign(source, &expired_signer(), NOW).expect_err("the expired certificate is refused");
    assert_eq!(error.code(), "nocode_publish.signature");
}

#[test]
fn a_certificate_for_another_publisher_is_refused() {
    let site = fixture_site();
    let mut draft = install_draft();
    draft.publisher = "other-publisher".to_owned();
    let source = package(&draft, &site).expect("package");
    let error = sign(source, &signer(), NOW).expect_err("the wrong publisher is refused");
    assert_eq!(error.code(), "nocode_publish.signature");
    assert!(error.to_string().contains("other-publisher"), "{error}");
}

#[test]
fn the_review_gate_stops_a_malformed_bundle() {
    let error = review_app(b"this is not a zip", &ReviewConfig::default(), CREATED_AT)
        .expect_err("a malformed bundle is a blocker");
    assert_eq!(error.code(), "nocode_publish.review_rejected");
    assert!(error.to_string().contains("bundle.bad_archive"), "{error}");
}

#[test]
fn semver_rules_are_strict_and_ignore_build_metadata() {
    let existing = vec![Version::new(1, 0, 0), Version::new(1, 2, 0)];
    assert!(check_version_bump("app", &existing, &Version::new(1, 3, 0)).is_ok());
    let same = check_version_bump("app", &existing, &Version::new(1, 2, 0)).expect_err("same");
    assert_eq!(same.code(), "nocode_publish.version_exists");
    let metadata = Version::parse("1.2.0+build.7").expect("a version");
    let metadata = check_version_bump("app", &existing, &metadata).expect_err("build metadata");
    assert_eq!(metadata.code(), "nocode_publish.version_exists");
    let lower = check_version_bump("app", &existing, &Version::new(1, 1, 0)).expect_err("lower");
    assert_eq!(lower.code(), "nocode_publish.version_not_increasing");
}
