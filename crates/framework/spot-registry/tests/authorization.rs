#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use spot_bundle_signing::*;
use spot_registry::*;

#[test]
fn private_authorization_expires_for_every_protected_operation() {
    let s = Setup::new();
    let r = s.publish("private_app", "1.0.0", "", 12);
    let caller = s.caller(12);
    s.registry
        .yank(&r.app, &r.version, "retired".into(), &caller, NOW)
        .unwrap();
    let expires = NOW + AUTHORIZATION_LIFETIME_SECONDS;
    let visibility = Visibility {
        include_pre: true,
        include_inactive: true,
    };
    assert_eq!(
        s.registry
            .releases(&r.app, &visibility, Some(&caller), expires - 1)
            .unwrap()
            .len(),
        1
    );
    assert_code(
        s.registry
            .releases(&r.app, &visibility, Some(&caller), expires),
        "registry.unauthorized",
    );
    assert_code(
        s.registry.search(
            &SearchQuery {
                visibility,
                caller: Some(caller.clone()),
                ..Default::default()
            },
            expires,
        ),
        "registry.unauthorized",
    );
    assert_code(
        s.registry
            .describe(&r.app, "forged".into(), "bad".into(), &caller, expires),
        "registry.unauthorized",
    );
    assert_code(
        s.registry
            .yank(&r.app, &r.version, "changed".into(), &caller, expires),
        "registry.unauthorized",
    );
    assert_code(
        s.registry
            .withdraw(&r.app, &r.version, "forged".into(), &caller, expires),
        "registry.unauthorized",
    );
    assert!(
        s.index
            .release(&r.app, &r.version)
            .unwrap()
            .unwrap()
            .withdrawn
            .is_none()
    );
    assert_eq!(s.audit.records().unwrap().len(), 2);
}

#[test]
fn authorization_rejects_expired_future_and_modified_evidence() {
    let s = Setup::new();
    let b = bytes("identity", "1.0.0", "");
    let expired = authorization(&b, 12, NOW - AUTHORIZATION_LIFETIME_SECONDS);
    assert_code(
        s.registry
            .authenticate(&b, expired.signature, expired.chain, NOW),
        "registry.unauthorized",
    );
    let future = authorization(&b, 12, NOW + 1);
    assert_code(
        s.registry
            .authenticate(&b, future.signature, future.chain, NOW),
        "registry.signature_in_future",
    );
    let mut changed = authorization(&b, 12, NOW);
    changed.signature.signed_at -= 1;
    assert_code(
        s.registry
            .authenticate(&b, changed.signature, changed.chain, NOW),
        "signing.signature_invalid",
    );
    let good = authorization(&b, 12, NOW);
    assert_code(
        s.registry.authenticate(
            &bytes("different", "1.0.0", ""),
            good.signature,
            good.chain,
            NOW,
        ),
        "signing.signature_invalid",
    );
}

#[test]
fn authorization_is_domain_separated_from_publish_and_transfer() {
    assert!(AppName::parse(AUTHORIZATION_APP_ID).is_err());
    let s = Setup::new();
    let b = bytes("identity", "1.0.0", "");
    let auth = authorization(&b, 12, NOW);
    assert_code(
        run(s
            .registry
            .publish(b.clone(), auth.signature, auth.chain, NOW)),
        "signing.app_mismatch",
    );
    let signature = sign_bundle(
        &open(&b).digest(),
        TRANSFER_APP_ID,
        TRANSFER_VERSION,
        &cert(12),
        &key(12),
        NOW,
    )
    .unwrap();
    assert_code(
        s.registry
            .authenticate(&b, signature, CertificateChain::new(cert(12)), NOW),
        "signing.app_mismatch",
    );
    assert!(s.index.ownership(&app("identity")).unwrap().is_none());
    assert!(s.audit.records().unwrap().is_empty());
}

#[test]
fn private_authorization_still_rechecks_current_revocation_and_status() {
    let s = Setup::new();
    let r = s.publish("private_app", "1.0.0", "", 12);
    let caller = s.caller(12);
    s.index
        .set_publisher_status("publisher12", PublisherStatus::Suspended)
        .unwrap();
    assert_code(
        s.registry
            .withdraw(&r.app, &r.version, "bad".into(), &caller, NOW),
        "registry.publisher_inactive",
    );
    s.index
        .set_publisher_status("publisher12", PublisherStatus::Active)
        .unwrap();
    s.trust
        .update_revocations(revoke_key(12, 2, NOW, false), NOW)
        .unwrap();
    assert_code(
        s.registry
            .withdraw(&r.app, &r.version, "bad".into(), &caller, NOW),
        "signing.key_revoked",
    );
    assert!(
        s.index
            .release(&r.app, &r.version)
            .unwrap()
            .unwrap()
            .active()
    );
}

#[test]
fn public_release_evidence_is_not_publisher_authorization() {
    let s = Setup::new();
    let release = s.publish("private_app", "1.0.0", "", 12);
    let fetched = run(s.registry.fetch(&release.app, &release.version, NOW)).unwrap();
    // An arbitrary installer knows every input here, without the publisher's key.
    assert_code(
        s.registry.authenticate(
            &fetched.bytes,
            fetched.release.signature_ref.signature,
            fetched.release.signature_ref.chain,
            NOW,
        ),
        "signing.app_mismatch",
    );
    assert!(
        s.index
            .release(&release.app, &release.version)
            .unwrap()
            .unwrap()
            .active()
    );
    assert_eq!(s.audit.records().unwrap().len(), 1);
}
