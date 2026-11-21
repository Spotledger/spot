#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use spot_registry::*;
#[test]
fn dual_signature_transfer_preserves_history_and_previous_release_publisher() {
    let s = Setup::new();
    let old = s.publish("demo", "1.0.0", "", 12);
    let t = transfer("demo", 12, 13, 0);
    s.registry.transfer(t.clone(), NOW).unwrap();
    let owner = s.index.ownership(&old.app).unwrap().unwrap();
    assert_eq!(owner.publisher, "publisher13");
    assert_eq!(owner.generation, 1);
    assert_eq!(s.index.transfers(&old.app).unwrap(), vec![t]);
    assert_code(
        publish(&s.registry, bytes("demo", "2.0.0", ""), 12),
        "registry.not_owner",
    );
    let new = s.publish("demo", "2.0.0", "", 13);
    assert_eq!(new.publisher, "publisher13");
    assert_eq!(
        run(s.registry.fetch_digest(old.digest, NOW))
            .unwrap()
            .release
            .publisher,
        "publisher12"
    );
    let audit = s.audit.records().unwrap();
    assert_eq!(audit[1].action, AuditAction::Transfer);
    assert_eq!(audit[1].who, "publisher12");
    assert!(audit[1].reason.contains("publisher13"));
}
#[test]
fn replay_refused_even_after_ownership_returns_to_original_owner() {
    let s = Setup::new();
    s.publish("demo", "1.0.0", "", 12);
    let original = transfer("demo", 12, 13, 0);
    s.registry.transfer(original.clone(), NOW).unwrap();
    assert_code(
        s.registry.transfer(original.clone(), NOW),
        "registry.not_owner",
    );
    s.registry
        .transfer(transfer("demo", 13, 12, 1), NOW)
        .unwrap();
    assert_code(
        s.registry.transfer(original, NOW),
        "registry.transfer_replay",
    );
    assert_eq!(
        s.index.ownership(&app("demo")).unwrap().unwrap().generation,
        2
    );
    assert_eq!(s.audit.records().unwrap().len(), 3);
}
#[test]
fn changing_any_transfer_commitment_field_invalidates_both_signatures() {
    for field in 0..8 {
        let s = Setup::new();
        s.publish("demo", "1.0.0", "", 12);
        let mut t = transfer("demo", 12, 13, 0);
        match field {
            0 => t.request.app = app("other"),
            1 => t.request.from = "publisher14".into(),
            2 => t.request.to = "publisher14".into(),
            3 => t.request.generation = 1,
            4 => t.request.at -= 1,
            5 => t.request.expires_at += 1,
            6 => t.request.reason = "changed".into(),
            _ => t.to = t.from.clone(),
        }
        let error = s.registry.transfer(t, NOW).unwrap_err();
        assert!(
            matches!(
                error.code,
                "signing.signature_invalid" | "registry.transfer_invalid"
            ),
            "{error:?}"
        );
        assert_eq!(
            s.index.ownership(&app("demo")).unwrap().unwrap().publisher,
            "publisher12"
        );
        assert_eq!(s.audit.records().unwrap().len(), 1);
    }
}
#[test]
fn expired_future_self_and_unknown_transfers_refused() {
    let s = Setup::new();
    s.publish("demo", "1.0.0", "", 12);
    assert_code(
        s.registry.transfer(transfer("demo", 12, 13, 0), 1000),
        "registry.transfer_invalid",
    );
    assert_code(
        s.registry.transfer(transfer("demo", 12, 13, 0), NOW - 1),
        "registry.transfer_invalid",
    );
    assert_code(
        s.registry.transfer(transfer("demo", 12, 12, 0), NOW),
        "registry.transfer_replay",
    );
    assert_code(
        s.registry.transfer(transfer("free", 12, 13, 0), NOW),
        "registry.not_owner",
    );
}
#[test]
fn revocation_and_suspension_block_transfers_without_mutation() {
    let s = Setup::new();
    s.publish("demo", "1.0.0", "", 12);
    s.trust
        .update_revocations(revoke_key(13, 2, 400, false), NOW)
        .unwrap();
    assert_code(
        s.registry.transfer(transfer("demo", 12, 13, 0), NOW),
        "signing.key_revoked",
    );
    assert_eq!(
        s.index.ownership(&app("demo")).unwrap().unwrap().generation,
        0
    );
    let s = Setup::new();
    s.publish("demo", "1.0.0", "", 12);
    s.publish("other", "1.0.0", "", 13);
    s.index
        .set_publisher_status("publisher13", PublisherStatus::Suspended)
        .unwrap();
    assert_code(
        s.registry.transfer(transfer("demo", 12, 13, 0), NOW),
        "registry.publisher_inactive",
    );
}
#[test]
fn concurrent_transfers_have_one_generation_winner() {
    let s = Setup::new();
    s.publish("demo", "1.0.0", "", 12);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = [13, 14]
        .into_iter()
        .map(|to| {
            let registry = s.registry.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let t = transfer("demo", 12, to, 0);
                barrier.wait();
                registry.transfer(t, NOW)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        s.index.ownership(&app("demo")).unwrap().unwrap().generation,
        1
    );
    assert_eq!(s.audit.records().unwrap().len(), 2);
}
