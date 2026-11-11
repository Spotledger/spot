//! Property tests: `decide` never applies an older version than the installed one, and never
//! panics on arbitrary input.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use proptest::prelude::*;
use semver::Version;
use spot_updater::{
    Channel, Decision, Digest, Installed, UpdateManifest, UpdatePayload, UpdatePolicy,
    UpdaterErrorCode, decide,
};

/// An arbitrary semver version.
fn version() -> impl Strategy<Value = Version> {
    (0u64..1_000, 0u64..1_000, 0u64..1_000)
        .prop_map(|(major, minor, patch)| Version::new(major, minor, patch))
}

/// An arbitrary digest.
fn digest() -> impl Strategy<Value = Digest> {
    proptest::array::uniform32(any::<u8>()).prop_map(Digest::from_bytes)
}

/// An arbitrary install.
fn installed() -> impl Strategy<Value = Installed> {
    (version(), 0u64..1_000, proptest::option::of(digest())).prop_map(
        |(version, schema_epoch, payload_sha256)| Installed {
            product: PRODUCT.to_owned(),
            version,
            channel: Channel::Stable,
            schema_epoch,
            payload_sha256,
        },
    )
}

/// An arbitrary offered manifest.
fn offered() -> impl Strategy<Value = UpdateManifest> {
    (
        version(),
        version(),
        proptest::collection::vec(version(), 0..3),
        any::<i64>(),
        0u64..1_000,
        proptest::collection::vec(any::<u64>(), 0..3),
        digest(),
    )
        .prop_map(
            |(version, min_from, via, released_at, schema_epoch, migrations, sha256)| {
                UpdateManifest {
                    product: PRODUCT.to_owned(),
                    channel: Channel::Stable,
                    version,
                    min_from,
                    via,
                    released_at,
                    payload: UpdatePayload {
                        file: "update.zip".to_owned(),
                        size: 1,
                        sha256,
                    },
                    files: Vec::new(),
                    migrations,
                    requires_os: None,
                    schema_epoch,
                    notes: None,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// An older version is never applied.
    #[test]
    fn decide_never_applies_an_older_version(
        installed_version in version(),
        offered_version in version(),
        now in any::<i64>(),
        schema_epoch in 0u64..1_000,
    ) {
        prop_assume!(offered_version < installed_version);
        let state = Installed {
            product: PRODUCT.to_owned(),
            version: installed_version,
            channel: Channel::Stable,
            schema_epoch,
            payload_sha256: None,
        };
        let mut update = manifest();
        update.version = offered_version;
        let decision = decide(&state, &update, &UpdatePolicy::default(), now);
        prop_assert!(
            matches!(decision, Decision::Refuse(UpdaterErrorCode::Downgrade)),
            "an older version was not refused with the downgrade code: {decision:?}"
        );
    }

    /// Arbitrary input never panics.
    #[test]
    fn decide_never_panics(
        state in installed(),
        update in offered(),
        now in any::<i64>(),
        allowed in proptest::collection::vec(
            prop_oneof![Just(Channel::Stable), Just(Channel::Beta)],
            0..3,
        ),
    ) {
        let policy = UpdatePolicy {
            allowed_channels: allowed,
            ..UpdatePolicy::default()
        };
        let _ = decide(&state, &update, &policy, now);
    }
}
