//! The acceptance tests: a bundle promoted across three environments with identical results,
//! and every refusal the transport must make.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_custom_bundle::{CaptureOptions, Limits, SectionRegistry, SiteReader, capture};
use std::sync::LazyLock;

static REGISTRY: LazyLock<SectionRegistry> = LazyLock::new(SectionRegistry::new);
static LIMITS: LazyLock<Limits> = LazyLock::new(Limits::default);
use spot_environments::{
    Context, Policy, PromotionChain, Role, TransportPackage, Trust, import, transport, verify_chain,
};

fn context<'a>(
    set: &'a spot_environments::EnvironmentSet,
    target: Role,
    variables: &'a spot_environments::Variables,
    anchor: &'a spot_bundle_signing::TrustAnchor,
    revocations: &'a spot_bundle_signing::RevocationList,
) -> Context<'a> {
    Context {
        set,
        target: member(set, target),
        registry: &REGISTRY,
        limits: &LIMITS,
        trust: Trust {
            anchor,
            revocations,
            now: 200,
        },
        variables,
        actor: "glm",
        at: 1_760_000_000_000,
    }
}

#[test]
fn one_bundle_traverses_three_environments_with_one_digest() {
    let set = set(Policy::default());
    let anchor = anchor();
    let revocations = revocations();
    let mut chain = PromotionChain::new();

    let mut test_site = target_site();
    let test_variables = variables("test");
    let to_test = package(&set, Role::Dev, Role::Test);
    let test_import = import(
        &context(&set, Role::Test, &test_variables, &anchor, &revocations),
        &to_test,
        &mut chain,
        &mut test_site,
    )
    .unwrap();

    let source_digest = source_bundle().digest().to_hex();
    assert_eq!(test_import.record.bundle_digest, source_digest);
    assert_eq!(test_import.record.result_digest, source_digest);
    assert_eq!(test_import.record.source.role, Role::Dev);
    assert_eq!(test_import.record.target.role, Role::Test);
    assert_eq!(test_import.applied, 2);

    let mut prod_site = target_site();
    let prod_variables = variables("prod");
    let to_prod = package(&set, Role::Test, Role::Prod);
    let prod_import = import(
        &context(&set, Role::Prod, &prod_variables, &anchor, &revocations),
        &to_prod,
        &mut chain,
        &mut prod_site,
    )
    .unwrap();

    assert_eq!(prod_import.record.bundle_digest, source_digest);
    assert_eq!(prod_import.record.result_digest, source_digest);
    assert_eq!(prod_import.record.source.role, Role::Test);
    assert_eq!(prod_import.record.target.role, Role::Prod);
    assert_eq!(prod_import.record.seq, 2);
    assert_eq!(prod_import.record.prev, test_import.record.hash);

    // The proof: one digest traversed dev -> test -> prod, with an identical result each time.
    let proof = chain.verify().unwrap();
    assert_eq!(proof.digest, source_digest);
    assert_eq!(
        proof
            .environments
            .iter()
            .map(|env| env.role)
            .collect::<Vec<_>>(),
        vec![Role::Dev, Role::Test, Role::Prod]
    );

    // The values really did differ per environment, and the captures really were different.
    let options = CaptureOptions::new(
        "acme-customisations",
        "1.0.0",
        "glm",
        "2026-10-19T08:00:00Z",
    );
    let test_capture = capture(&test_site, &options).unwrap();
    let prod_capture = capture(&prod_site, &options).unwrap();
    assert_ne!(test_capture.digest(), prod_capture.digest());
    assert_ne!(test_capture.digest().to_hex(), source_digest);
    let endpoint = |site: &dyn SiteReader| -> String {
        let fixtures = site.fixtures();
        let document = &fixtures[0].documents[0];
        document["endpoint"].as_str().unwrap_or_default().to_owned()
    };
    assert_eq!(endpoint(&test_site), "https://test.acme.test");
    assert_eq!(endpoint(&prod_site), "https://prod.acme.test");
}

#[test]
fn a_tampered_package_is_refused() {
    let set = set(Policy::default());
    let anchor = anchor();
    let revocations = revocations();
    let registry = SectionRegistry::new();
    let limits = Limits::default();
    let trust = Trust {
        anchor: &anchor,
        revocations: &revocations,
        now: 200,
    };
    let mut package = package(&set, Role::Dev, Role::Test);

    // The envelope's digest no longer names the bundle.
    let mut wrong_digest = package.clone();
    wrong_digest.envelope.digest = "0".repeat(64);
    let error = wrong_digest
        .verify(&set, member(&set, Role::Test), &trust, &registry, &limits)
        .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::DIGEST_MISMATCH);

    // One changed byte in the archive is refused before anything is planned.
    let last = package.bundle.len() - 1;
    package.bundle[last] ^= 0xff;
    let error = package
        .verify(&set, member(&set, Role::Test), &trust, &registry, &limits)
        .unwrap_err();
    assert!(
        matches!(
            error.code(),
            spot_environments::codes::INVALID_BUNDLE | spot_environments::codes::DIGEST_MISMATCH
        ),
        "unexpected refusal: {error}"
    );
}

#[test]
fn an_unknown_signer_is_refused() {
    let set = set(Policy::default());
    let anchor = anchor();
    let revocations = revocations();
    let registry = SectionRegistry::new();
    let limits = Limits::default();
    let package = package(&set, Role::Dev, Role::Test);

    let other = spot_bundle_signing::generate_keypair(&[9_u8; 32]);
    let untrusted =
        spot_bundle_signing::TrustAnchor::new(vec![spot_bundle_signing::TrustedRoot::new(
            other.public(),
            0,
            None,
        )]);
    let trust = Trust {
        anchor: &untrusted,
        revocations: &revocations,
        now: 200,
    };
    let error = package
        .verify(&set, member(&set, Role::Test), &trust, &registry, &limits)
        .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::UNKNOWN_SIGNER);

    // Keep the trusted anchor referenced so the borrow checker sees the same shape as production.
    let _ = anchor;
}

#[test]
fn an_unresolved_placeholder_is_refused_before_planning() {
    let set = set(Policy::default());
    let anchor = anchor();
    let revocations = revocations();
    let mut site = target_site();
    let mut chain = PromotionChain::new();
    let package = package(&set, Role::Dev, Role::Test);

    let mut partial = spot_environments::Variables::new();
    partial
        .set_text("API_URL", "https://test.acme.test")
        .unwrap();
    let error = import(
        &context(&set, Role::Test, &partial, &anchor, &revocations),
        &package,
        &mut chain,
        &mut site,
    )
    .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::UNRESOLVED_VARIABLE);
    assert_eq!(error.item(), "TOKEN");
    assert!(chain.records().is_empty());
    assert!(site.fixtures().is_empty());
}

#[test]
fn skipping_an_environment_is_refused_by_default() {
    let default_set = set(Policy::default());
    let permissive = set(Policy { allow_skip: true });

    let bundle = source_bundle();
    let certificate = certificate();
    let key = publisher();
    let error = transport(
        &bundle,
        &default_set,
        member(&default_set, Role::Dev),
        member(&default_set, Role::Prod),
        "glm",
        &spot_environments::Signer {
            certificate: &certificate,
            key: &key,
        },
        150,
    )
    .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::SKIP_REFUSED);

    // A package made under a permissive policy is still refused by a receiver whose set does
    // not allow the skip.
    let package = package(&permissive, Role::Dev, Role::Prod);
    let anchor = anchor();
    let revocations = revocations();
    let registry = SectionRegistry::new();
    let limits = Limits::default();
    let trust = Trust {
        anchor: &anchor,
        revocations: &revocations,
        now: 200,
    };
    let error = package
        .verify(
            &default_set,
            member(&default_set, Role::Prod),
            &trust,
            &registry,
            &limits,
        )
        .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::SKIP_REFUSED);
}

#[test]
fn a_broken_chain_is_detected() {
    let set = set(Policy::default());
    let anchor = anchor();
    let revocations = revocations();
    let mut chain = PromotionChain::new();
    let mut test_site = target_site();
    let test_variables = variables("test");
    import(
        &context(&set, Role::Test, &test_variables, &anchor, &revocations),
        &package(&set, Role::Dev, Role::Test),
        &mut chain,
        &mut test_site,
    )
    .unwrap();
    let mut prod_site = target_site();
    let prod_variables = variables("prod");
    import(
        &context(&set, Role::Prod, &prod_variables, &anchor, &revocations),
        &package(&set, Role::Test, Role::Prod),
        &mut chain,
        &mut prod_site,
    )
    .unwrap();
    assert!(verify_chain(chain.records()).is_ok());

    let mut edited = chain.records().to_vec();
    edited[1].actor = "someone-else".to_owned();
    let error = verify_chain(&edited).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::CHAIN_BROKEN);

    let truncated = &chain.records()[1..];
    let error = verify_chain(truncated).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::CHAIN_BROKEN);

    let mut mixed = chain.records().to_vec();
    mixed[1].bundle_digest = "1".repeat(64);
    mixed[1].hash = mixed[1].calculate_hash(mixed[1].seq, &mixed[1].prev);
    let error = verify_chain(&mixed).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::CHAIN_BROKEN);
}

#[test]
fn a_package_for_another_target_is_refused() {
    let set = set(Policy::default());
    let anchor = anchor();
    let revocations = revocations();
    let registry = SectionRegistry::new();
    let limits = Limits::default();
    let trust = Trust {
        anchor: &anchor,
        revocations: &revocations,
        now: 200,
    };
    let package = package(&set, Role::Dev, Role::Test);
    let error = package
        .verify(&set, member(&set, Role::Prod), &trust, &registry, &limits)
        .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::WRONG_TARGET);
}

#[test]
fn transport_survives_a_round_trip_through_bytes() {
    let set = set(Policy::default());
    let anchor = anchor();
    let revocations = revocations();
    let registry = SectionRegistry::new();
    let limits = Limits::default();
    let trust = Trust {
        anchor: &anchor,
        revocations: &revocations,
        now: 200,
    };
    let package: TransportPackage = package(&set, Role::Dev, Role::Test);
    let verified = package
        .verify(&set, member(&set, Role::Test), &trust, &registry, &limits)
        .unwrap();
    assert_eq!(verified.publisher(), "acme-software");
    assert_eq!(verified.signed_at(), 150);
    assert_eq!(verified.source().role, Role::Dev);
    assert_eq!(verified.target().role, Role::Test);
    assert_eq!(verified.bundle().digest(), source_bundle().digest());
}

#[test]
fn transport_refuses_an_unknown_set() {
    let set = set(Policy::default());
    let bundle = source_bundle();
    let certificate = certificate();
    let key = publisher();
    let other = spot_environments::EnvironmentSet::new(
        "other",
        spot_tenancy::Region::new("eu-west").unwrap(),
        apps(),
        set.members().to_vec(),
        Policy::default(),
    )
    .unwrap();
    let package = transport(
        &bundle,
        &other,
        member(&other, Role::Dev),
        member(&other, Role::Test),
        "glm",
        &spot_environments::Signer {
            certificate: &certificate,
            key: &key,
        },
        150,
    )
    .unwrap();
    let anchor = anchor();
    let revocations = revocations();
    let trust = Trust {
        anchor: &anchor,
        revocations: &revocations,
        now: 200,
    };
    let error = package
        .verify(
            &set,
            member(&set, Role::Test),
            &trust,
            &SectionRegistry::new(),
            &Limits::default(),
        )
        .unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::WRONG_TARGET);
}
