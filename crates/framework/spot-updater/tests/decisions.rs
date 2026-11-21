//! The pure decision: apply, step via, skip or refuse, with the code that names why.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use semver::Version;
use spot_updater::{Channel, Decision, Digest, Installed, UpdatePolicy, UpdaterErrorCode, decide};

fn installed(version: &str) -> Installed {
    Installed {
        product: PRODUCT.to_owned(),
        version: Version::parse(version).unwrap(),
        channel: Channel::Stable,
        schema_epoch: 3,
        payload_sha256: None,
    }
}

#[test]
fn a_newer_version_applies() {
    assert_eq!(
        decide(
            &installed("1.5.0"),
            &manifest(),
            &UpdatePolicy::default(),
            NOW
        ),
        Decision::Apply
    );
}

#[test]
fn an_older_version_is_refused() {
    assert_eq!(
        decide(
            &installed("3.0.0"),
            &manifest(),
            &UpdatePolicy::default(),
            NOW
        ),
        Decision::Refuse(UpdaterErrorCode::Downgrade)
    );
}

#[test]
fn the_same_version_is_skipped() {
    assert_eq!(
        decide(
            &installed("2.0.0"),
            &manifest(),
            &UpdatePolicy::default(),
            NOW
        ),
        Decision::Skip("this version is already installed".to_owned())
    );
}

#[test]
fn the_same_version_with_a_different_digest_is_refused() {
    let mut state = installed("2.0.0");
    state.payload_sha256 = Some(Digest::of(b"another payload"));
    assert_eq!(
        decide(&state, &manifest(), &UpdatePolicy::default(), NOW),
        Decision::Refuse(UpdaterErrorCode::DigestChanged)
    );
}

#[test]
fn another_product_is_refused() {
    let mut state = installed("1.5.0");
    state.product = "other".to_owned();
    assert_eq!(
        decide(&state, &manifest(), &UpdatePolicy::default(), NOW),
        Decision::Refuse(UpdaterErrorCode::ProductMismatch)
    );
}

#[test]
fn a_disallowed_channel_is_refused() {
    let mut offered = manifest();
    offered.channel = Channel::Beta;
    let policy = UpdatePolicy {
        allowed_channels: vec![Channel::Stable],
        ..UpdatePolicy::default()
    };
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &policy, NOW),
        Decision::Refuse(UpdaterErrorCode::ChannelNotAllowed)
    );
}

#[test]
fn a_newer_schema_epoch_without_a_listed_migration_is_refused() {
    let mut offered = manifest();
    offered.schema_epoch = 4;
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &UpdatePolicy::default(), NOW),
        Decision::Refuse(UpdaterErrorCode::SchemaEpoch)
    );
}

#[test]
fn a_newer_schema_epoch_with_a_listed_migration_applies() {
    let mut offered = manifest();
    offered.schema_epoch = 4;
    offered.migrations = vec![3];
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &UpdatePolicy::default(), NOW),
        Decision::Apply
    );
}

#[test]
fn a_database_newer_than_the_update_understands_is_refused() {
    let mut offered = manifest();
    offered.schema_epoch = 2;
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &UpdatePolicy::default(), NOW),
        Decision::Refuse(UpdaterErrorCode::SchemaEpoch)
    );
}

#[test]
fn a_min_from_violation_steps_through_the_listed_via() {
    let mut offered = manifest();
    offered.min_from = Version::parse("1.8.0").unwrap();
    offered.via = vec![Version::parse("1.8.0").unwrap()];
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &UpdatePolicy::default(), NOW),
        Decision::StepVia(Version::parse("1.8.0").unwrap())
    );
}

#[test]
fn a_min_from_violation_without_a_via_is_refused() {
    let mut offered = manifest();
    offered.min_from = Version::parse("1.8.0").unwrap();
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &UpdatePolicy::default(), NOW),
        Decision::Refuse(UpdaterErrorCode::MinFrom)
    );
}

#[test]
fn a_manifest_dated_in_the_future_is_refused() {
    let mut offered = manifest();
    offered.released_at = NOW + spot_updater::decide::RELEASE_TOLERANCE_SECS + 1;
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &UpdatePolicy::default(), NOW),
        Decision::Refuse(UpdaterErrorCode::NotReleased)
    );
}

#[test]
fn a_release_inside_the_tolerance_applies() {
    let mut offered = manifest();
    offered.released_at = NOW + spot_updater::decide::RELEASE_TOLERANCE_SECS;
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &UpdatePolicy::default(), NOW),
        Decision::Apply
    );
}

#[test]
fn the_offered_type_is_the_manifest() {
    let offered: spot_updater::Offered = manifest();
    assert_eq!(
        decide(&installed("1.5.0"), &offered, &UpdatePolicy::default(), NOW),
        Decision::Apply
    );
}
