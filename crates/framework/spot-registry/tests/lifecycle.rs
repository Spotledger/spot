#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use spot_bundle::{BundleBuilder, Limits};
use spot_bundle_signing::*;
use spot_registry::*;

#[test]
fn real_pack_empty_registry_roundtrip_through_client() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("doctypes")).unwrap();
    std::fs::write(
        dir.path().join("manifest.toml"),
        manifest(
            "clean_app",
            "1.2.3",
            "requires_host = \"^0.1\"\ndepends = [{ app = \"base\", version = \"^2\" }]\n",
        ),
    )
    .unwrap();
    std::fs::write(dir.path().join("app.wasm"), COMPONENT).unwrap();
    let (report, packed) =
        spot_pack::pack_app(dir.path(), &spot_pack::PackOptions::default()).unwrap();
    assert!(report.is_ok());
    let s = Setup::new();
    assert!(
        s.registry
            .latest(&app("clean_app"), false)
            .unwrap()
            .is_none()
    );
    let bundle = packed.open(&spot_pack::PackOptions::default()).unwrap();
    let signature =
        spot_pack::sign_packed(&bundle, "clean_app", "1.2.3", &cert(12), &key(12), 150).unwrap();
    let r = run(s.registry.publish(
        packed.bytes.clone(),
        signature,
        CertificateChain::new(cert(12)),
        NOW,
    ))
    .unwrap();
    let client: Box<dyn RegistryClient> = Box::new(LocalRegistryClient::new(s.registry.clone()));
    let page = run(client.search(
        &SearchQuery {
            text: "clean_app".into(),
            ..Default::default()
        },
        NOW,
    ))
    .unwrap();
    assert_eq!(page.releases, vec![r.clone()]);
    assert_eq!(run(client.latest(&r.app, false)).unwrap(), Some(r.clone()));
    assert_eq!(
        run(client.releases(&r.app, &Visibility::default(), None, NOW)).unwrap(),
        vec![r.clone()]
    );
    let verified = run(client.fetch(&r.app, &r.version, NOW)).unwrap();
    assert_eq!(verified.bytes, packed.bytes);
    assert_eq!(open(&verified.bytes).digest(), packed.digest);
    assert_eq!(verified.verified.publisher_id, "publisher12");
    assert_eq!(verified.verified.revocation_generation, 1);
    assert_eq!(
        verified.release.depends[0].version,
        VersionReq::parse("^2").unwrap()
    );
    assert_eq!(s.audit.records().unwrap().len(), 1);
    assert!(run(s.registry.repair_orphans()).unwrap().is_empty());
}

#[test]
fn size_refusal_precedes_archive_and_signature() {
    let s = Setup::policy(RegistryPolicy {
        limits: Limits {
            max_compressed_bytes: 2,
            ..Limits::default()
        },
        ..Default::default()
    });
    let p = proof(&bytes("demo", "1.0.0", ""), 12);
    assert_code(
        run(s.registry.publish(vec![0; 3], p.signature, p.chain, NOW)),
        "registry.too_large",
    );
    assert!(s.index.digests().unwrap().is_empty());
    assert!(run(s.blobs.inventory()).unwrap().is_empty());
}
#[test]
fn bundle_refusal_precedes_signature_and_does_not_claim_name() {
    let s = Setup::new();
    let p = proof(&bytes("demo", "1.0.0", ""), 12);
    assert_code(
        run(s.registry.publish(vec![1; 10], p.signature, p.chain, NOW)),
        "bundle.bad_archive",
    );
    assert!(s.index.ownership(&app("demo")).unwrap().is_none());
}
#[test]
fn invalid_requirements_rejected_by_authoritative_manifest_parser() {
    let s = Setup::new();
    for extra in [
        "requires_host = \"not semver\"",
        "depends = [{ app = \"base\", version = \"invalid\" }]",
    ] {
        let b = bytes("demo", "1.0.0", extra);
        let p = proof(&bytes("demo", "1.0.0", ""), 12);
        assert_code(
            run(s.registry.publish(b, p.signature, p.chain, NOW)),
            "bundle.bad_manifest",
        );
    }
    assert!(s.index.digests().unwrap().is_empty());
}
#[test]
fn incompatible_host_is_bundle_refusal() {
    let s = Setup::new();
    let p = proof(&bytes("demo", "1.0.0", ""), 12);
    assert_code(
        run(s.registry.publish(
            bytes("demo", "1.0.0", "requires_host = \"^9\""),
            p.signature,
            p.chain,
            NOW,
        )),
        "bundle.host_required",
    );
}
#[test]
fn signature_precedes_owner_version_and_privileges() {
    let s = Setup::new();
    s.publish("demo", "2.0.0", "", 12);
    let b = bytes("demo", "1.0.0", "[capabilities]\nemail = true");
    let mut p = proof(&b, 13);
    p.signature = BundleSignature::from_parts(
        p.signature.app_id,
        p.signature.app_version,
        p.signature.publisher_id,
        p.signature.key_id,
        p.signature.signed_at,
        [0; 64],
    );
    assert_code(
        run(s.registry.publish(b, p.signature, p.chain, NOW)),
        "signing.signature_invalid",
    );
    assert_eq!(
        s.index
            .releases(&app("demo"), &Visibility::default(), None)
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn unknown_root_and_certificate_signature_checked_before_ownership() {
    let s = Setup::new();
    s.publish("demo", "2.0.0", "", 12);
    let b = bytes("demo", "1.0.0", "");
    let mut p = proof(&b, 13);
    p.chain.leaf.body.issued_by = key(99).public().key_id();
    assert_code(
        run(s.registry.publish(b.clone(), p.signature, p.chain, NOW)),
        "signing.unknown_root",
    );
    let mut p = proof(&b, 13);
    p.chain.leaf.body.display_name = "forged".into();
    assert_code(
        run(s.registry.publish(b, p.signature, p.chain, NOW)),
        "signing.certificate_signature_invalid",
    );
}
#[test]
fn ownership_precedes_version_and_capability_refusal() {
    let s = Setup::new();
    s.publish("demo", "2.0.0", "", 12);
    assert_code(
        publish(
            &s.registry,
            bytes("demo", "1.0.0", "[capabilities]\nemail=true"),
            13,
        ),
        "registry.not_owner",
    );
}
#[test]
fn version_precedes_capability_refusal() {
    let s = Setup::new();
    s.publish("demo", "2.0.0", "", 12);
    assert_code(
        publish(
            &s.registry,
            bytes("demo", "1.0.0", "[capabilities]\nemail=true"),
            12,
        ),
        "registry.version_not_increasing",
    );
}
#[test]
fn overprivileged_refusal_leaves_no_claim_blob_or_audit() {
    let s = Setup::new();
    assert_code(
        publish(
            &s.registry,
            bytes("demo", "1.0.0", "[capabilities]\nemail=true"),
            12,
        ),
        "bundle.over_privileged",
    );
    assert!(s.index.ownership(&app("demo")).unwrap().is_none());
    assert!(run(s.blobs.inventory()).unwrap().is_empty());
    assert!(s.audit.records().unwrap().is_empty());
    s.publish("demo", "1.0.0", "", 13);
}
#[test]
fn idempotent_publish_preserves_original_release_and_one_audit() {
    let s = Setup::new();
    let r = s.publish("demo", "1.0.0", "", 12);
    let p = proof(&bytes("demo", "1.0.0", ""), 12);
    let again = run(s
        .registry
        .publish(bytes("demo", "1.0.0", ""), p.signature, p.chain, NOW + 1))
    .unwrap();
    assert_eq!(r, again);
    assert_eq!(s.audit.records().unwrap().len(), 1);
}
#[test]
fn idempotence_never_bypasses_trust_or_current_revocation() {
    let s = Setup::new();
    s.publish("demo", "1.0.0", "", 12);
    let b = bytes("demo", "1.0.0", "");
    let mut p = proof(&b, 12);
    p.chain.leaf.body.valid_to = 200;
    assert_code(
        run(s.registry.publish(b.clone(), p.signature, p.chain, NOW)),
        "signing.certificate_signature_invalid",
    );
    s.trust
        .update_revocations(revoke_key(12, 2, 400, false), NOW)
        .unwrap();
    assert_code(publish(&s.registry, b, 12), "signing.key_revoked");
    assert_eq!(s.audit.records().unwrap().len(), 1);
}
#[test]
fn versions_conflict_downgrade_yank_and_build_metadata() {
    let s = Setup::new();
    let r = s.publish("demo", "2.0.0", "", 12);
    assert_code(
        publish(&s.registry, bytes("demo", "2.0.0", "# changed"), 12),
        "registry.version_conflict",
    );
    assert_code(
        publish(&s.registry, bytes("demo", "1.9.9", ""), 12),
        "registry.version_not_increasing",
    );
    assert_code(
        publish(&s.registry, bytes("demo", "2.0.0+build", ""), 12),
        "registry.version_conflict",
    );
    let caller = s.caller(12);
    s.registry
        .yank(&r.app, &r.version, "bad release".into(), &caller, NOW)
        .unwrap();
    assert_code(
        publish(&s.registry, bytes("demo", "2.0.0", "# changed"), 12),
        "registry.version_conflict",
    );
    assert_eq!(
        s.publish("demo", "2.0.0", "", 12).yanked,
        Some(Removal {
            at: NOW,
            reason: "bad release".into()
        })
    );
    assert_code(
        publish(&s.registry, bytes("demo", "1.0.0", ""), 12),
        "registry.version_not_increasing",
    );
    s.publish("demo", "2.0.1", "", 12);
}
#[test]
fn prerelease_hidden_and_version_precedence_still_monotonic() {
    let s = Setup::new();
    let stable = s.publish("demo", "1.0.0", "", 12);
    let pre = s.publish("demo", "2.0.0-alpha.1", "", 12);
    assert_eq!(
        s.registry.latest(&app("demo"), false).unwrap(),
        Some(stable)
    );
    assert_eq!(s.registry.latest(&app("demo"), true).unwrap(), Some(pre));
    assert_eq!(
        s.registry
            .search(&SearchQuery::default(), NOW)
            .unwrap()
            .releases
            .len(),
        1
    );
    assert_code(
        publish(&s.registry, bytes("demo", "1.5.0", ""), 12),
        "registry.version_not_increasing",
    );
    s.publish("demo", "2.0.0", "", 12);
}
#[test]
fn yank_digest_fetch_withdraw_and_audit_do_not_delete_bytes() {
    let s = Setup::new();
    let r = s.publish("demo", "1.0.0", "", 12);
    let caller = s.caller(12);
    s.registry
        .yank(&r.app, &r.version, "regression".into(), &caller, NOW)
        .unwrap();
    s.registry
        .yank(&r.app, &r.version, "ignored repeat".into(), &caller, NOW)
        .unwrap();
    assert!(s.registry.latest(&r.app, false).unwrap().is_none());
    assert_code(
        run(s.registry.fetch(&r.app, &r.version, NOW)),
        "registry.yanked",
    );
    assert!(run(s.registry.fetch_digest(r.digest, NOW)).is_ok());
    s.registry
        .withdraw(&r.app, &r.version, "security".into(), &caller, NOW)
        .unwrap();
    assert_code(
        run(s.registry.fetch_digest(r.digest, NOW)),
        "registry.withdrawn",
    );
    assert!(run(s.blobs.exists(r.digest)).unwrap());
    let audits = s.audit.records().unwrap();
    assert_eq!(
        audits.iter().map(|a| a.action.clone()).collect::<Vec<_>>(),
        vec![
            AuditAction::Publish,
            AuditAction::Yank,
            AuditAction::Withdraw
        ]
    );
    assert_eq!(audits[2].reason, "security");
    assert_eq!(audits[2].who, "publisher12");
}
#[test]
fn current_revocation_blocks_previously_published_client_fetch_and_stale_updates() {
    let s = Setup::new();
    let r = s.publish("demo", "1.0.0", "", 12);
    let client = LocalRegistryClient::new(s.registry.clone());
    assert!(run(client.fetch(&r.app, &r.version, NOW)).is_ok());
    s.trust
        .update_revocations(revoke_key(12, 2, 400, false), NOW)
        .unwrap();
    assert_code(
        run(client.fetch(&r.app, &r.version, NOW)),
        "signing.key_revoked",
    );
    assert_code(
        run(s.registry.fetch_digest(r.digest, NOW)),
        "signing.key_revoked",
    );
    assert_code(
        s.trust.update_revocations(list(1), NOW),
        "signing.stale_revocation_list",
    );
    assert_code(
        s.trust.update_revocations(list(2), NOW),
        "signing.stale_revocation_list",
    );
}
#[test]
fn compromised_publisher_and_digest_revocations_are_enforced() {
    for kind in 0..3 {
        let s = Setup::new();
        let r = s.publish("demo", "1.0.0", "", 12);
        let mut body = list(2).body;
        let expected = match kind {
            0 => {
                body.keys = revoke_key(12, 2, 900, true).body.keys;
                "signing.key_compromised"
            }
            1 => {
                body.publishers.push(RevokedPublisher {
                    publisher_id: "publisher12".into(),
                    effective_from: 400,
                    reason: "revoked".into(),
                });
                "signing.publisher_revoked"
            }
            _ => {
                body.bundles.push(RevokedBundle {
                    digest_hex: r.digest.to_hex(),
                    reason: "malware".into(),
                });
                "signing.bundle_revoked"
            }
        };
        s.trust
            .update_revocations(issue_revocation_list(body, &root()), NOW)
            .unwrap();
        assert_code(run(s.registry.fetch_digest(r.digest, NOW)), expected);
    }
}
#[test]
fn forged_list_cannot_replace_current_trust() {
    let s = Setup::new();
    let mut bad = list(2);
    bad.body.generation = 3;
    assert_code(
        s.trust.update_revocations(bad, NOW),
        "signing.revocation_signature_invalid",
    );
    let r = s.publish("demo", "1.0.0", "", 12);
    assert_eq!(
        run(s.registry.fetch_digest(r.digest, NOW))
            .unwrap()
            .verified
            .revocation_generation,
        1
    );
}
#[test]
fn certificate_expiry_and_future_signing_time_refused() {
    let s = Setup::new();
    let b = bytes("demo", "1.0.0", "");
    let p = proof(&b, 12);
    assert_code(
        run(s.registry.publish(b.clone(), p.signature, p.chain, 2100)),
        "signing.certificate_expired",
    );
    let p = proof(&b, 12);
    assert_code(
        run(s.registry.publish(b, p.signature, p.chain, 120)),
        "registry.signature_in_future",
    );
}
#[test]
fn app_version_and_publisher_signature_bindings_refused() {
    let s = Setup::new();
    let b = bytes("demo", "1.0.0", "");
    let digest = open(&b).digest();
    let sig = sign_bundle(&digest, "other", "1.0.0", &cert(12), &key(12), 150).unwrap();
    assert_code(
        run(s
            .registry
            .publish(b.clone(), sig, CertificateChain::new(cert(12)), NOW)),
        "signing.app_mismatch",
    );
    let sig = sign_bundle(&digest, "demo", "2.0.0", &cert(12), &key(12), 150).unwrap();
    assert_code(
        run(s
            .registry
            .publish(b.clone(), sig, CertificateChain::new(cert(12)), NOW)),
        "signing.version_mismatch",
    );
    let mut p = proof(&b, 12);
    p.signature.publisher_id = "publisher13".into();
    assert_code(
        run(s.registry.publish(b, p.signature, p.chain, NOW)),
        "signing.publisher_mismatch",
    );
}
#[test]
fn publisher_metadata_authenticated_and_suspension_not_bypassed() {
    let s = Setup::new();
    s.publish("demo", "1.0.0", "", 12);
    let p = s.index.publisher("publisher12").unwrap().unwrap();
    assert_eq!(p.display_name, "Publisher 12");
    assert_eq!(p.certificate_chain_root, root().public().key_id());
    s.index
        .set_publisher_status(&p.id, PublisherStatus::Suspended)
        .unwrap();
    assert_code(
        publish(&s.registry, bytes("demo", "1.0.0", ""), 12),
        "registry.publisher_inactive",
    );
    assert_code(
        publish(&s.registry, bytes("demo", "2.0.0", ""), 12),
        "registry.publisher_inactive",
    );
}
#[test]
fn inactive_visibility_requires_current_authenticated_publisher() {
    let s = Setup::new();
    let r = s.publish("demo", "1.0.0", "", 12);
    let caller = s.caller(12);
    s.registry
        .yank(&r.app, &r.version, "regression".into(), &caller, NOW)
        .unwrap();
    let visibility = Visibility {
        include_pre: true,
        include_inactive: true,
    };
    assert_code(
        s.registry.releases(&r.app, &visibility, None, NOW),
        "registry.unauthorized",
    );
    assert_eq!(
        s.registry
            .releases(&r.app, &visibility, Some(&caller), NOW)
            .unwrap()
            .len(),
        1
    );
    assert!(
        s.registry
            .releases(&r.app, &visibility, Some(&s.caller(13)), NOW)
            .unwrap()
            .is_empty()
    );
    s.trust
        .update_revocations(revoke_key(12, 2, 400, false), NOW)
        .unwrap();
    assert_code(
        s.registry.releases(&r.app, &visibility, Some(&caller), NOW),
        "signing.key_revoked",
    );
}
#[test]
fn removal_and_catalogue_updates_require_owner_and_nonempty_reason() {
    let s = Setup::new();
    let r = s.publish("demo", "1.0.0", "", 12);
    assert_code(
        s.registry
            .yank(&r.app, &r.version, "reason".into(), &s.caller(13), NOW),
        "registry.not_owner",
    );
    assert_code(
        s.registry
            .withdraw(&r.app, &r.version, "reason".into(), &s.caller(13), NOW),
        "registry.not_owner",
    );
    assert_code(
        s.registry
            .yank(&r.app, &r.version, " ".into(), &s.caller(12), NOW),
        "registry.reason_required",
    );
    assert_code(
        s.registry
            .describe(&r.app, "desc".into(), "cat".into(), &s.caller(13), NOW),
        "registry.not_owner",
    );
    assert_eq!(s.audit.records().unwrap().len(), 1);
}
#[test]
fn invalid_component_refused_before_trust() {
    let s = Setup::new();
    let p = proof(&bytes("demo", "1.0.0", ""), 12);
    let b = BundleBuilder::new(manifest("demo", "1.0.0", ""), b"\0asm\x01\0\0\0")
        .build()
        .unwrap();
    assert_code(
        run(s.registry.publish(b, p.signature, p.chain, NOW)),
        "bundle.bad_component",
    );
}

#[test]
fn idempotent_resigning_cannot_return_revoked_original_evidence() {
    let s = Setup::new();
    let b = bytes("demo", "1.0.0", "");
    s.publish("demo", "1.0.0", "", 12);
    s.trust
        .update_revocations(revoke_key(12, 2, 400, false), NOW)
        .unwrap();
    let mut body = cert(12).body;
    body.key = key(14).public();
    body.serial = 100;
    let certificate = issue_certificate(body, &root());
    let signature = sign_bundle(
        &open(&b).digest(),
        "demo",
        "1.0.0",
        &certificate,
        &key(14),
        450,
    )
    .unwrap();
    assert_code(
        run(s
            .registry
            .publish(b, signature, CertificateChain::new(certificate), NOW)),
        "signing.key_revoked",
    );
    assert_eq!(s.audit.records().unwrap().len(), 1);
}
#[test]
fn inflated_file_and_entry_limits_precede_trust() {
    for limits in [
        Limits {
            max_file_bytes: 1,
            ..Default::default()
        },
        Limits {
            max_entries: 1,
            ..Default::default()
        },
        Limits {
            max_total_bytes: 1,
            ..Default::default()
        },
    ] {
        let s = Setup::policy(RegistryPolicy {
            limits,
            ..Default::default()
        });
        let b = bytes("demo", "1.0.0", "");
        let p = proof(&b, 12);
        let error = run(s.registry.publish(b, p.signature, p.chain, NOW)).unwrap_err();
        assert!(matches!(
            error.code,
            "bundle.too_large" | "bundle.too_many_entries"
        ));
        assert!(run(s.blobs.inventory()).unwrap().is_empty());
        assert!(s.index.digests().unwrap().is_empty());
    }
}
#[test]
fn new_release_preserves_owner_curated_search_metadata() {
    let s = Setup::new();
    let r = s.publish("demo", "1.0.0", "", 12);
    s.registry
        .describe(&r.app, "text".into(), "category".into(), &s.caller(12), NOW)
        .unwrap();
    let latest = s.publish("demo", "2.0.0", "", 12);
    assert_eq!(latest.description, "text");
    assert_eq!(latest.category, "category");
}

#[test]
fn app_names_match_manifest_storage_identifier_bounds() {
    assert!(AppName::parse(&"a".repeat(63)).is_ok());
    for bad in [
        "".to_owned(),
        "a".repeat(64),
        "Upper".into(),
        "1name".into(),
        "_name".into(),
        "has-dash".into(),
        "é".into(),
        "../app".into(),
    ] {
        assert_code(AppName::parse(&bad), "registry.invalid_name");
    }
    assert_eq!(AppName::parse("app_42").unwrap().as_str(), "app_42");
}
