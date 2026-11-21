//! The two store-backed inputs, against an empty in-memory store.

use futures::executor::block_on;
use spot_journal_audit::{OldestSnapshot, Position};
use spot_journal_manifest::VerifyingKeys;
use spot_journal_snapshot::{SnapshotInfo, SnapshotKeys};
use spot_journal_store::{MemoryStore, TenantId};
use spot_journal_verify::{Expected, Mode};

use super::{Fixture, NOW, defaults};
use crate::{BlockerKind, ChainState, SnapshotFacts, Verdict, assess, check_chain, read_snapshots};

struct NoKeys;
impl SnapshotKeys for NoKeys {
    fn verify(&self, _: &str, _: u64, _: &[u8], _: &[u8]) -> bool {
        false
    }
}

#[test]
fn a_store_with_no_manifest_is_a_tampering_finding_not_a_clean_chain() {
    let store = MemoryStore::new();
    let expected = Expected {
        tenant: TenantId::new("acme").expect("a valid tenant"),
        source: "main".to_owned(),
        generation: None,
    };
    let chain = block_on(check_chain(
        &store,
        &VerifyingKeys::new(),
        &expected,
        Mode::Light,
    ));
    assert_eq!(chain.tenant, "acme");
    assert_eq!(chain.state, ChainState::Complete);
    assert!(chain.findings.iter().any(|f| f.tampering));

    let mut fx = Fixture::healthy();
    fx.chain = chain;
    let v = assess(&fx.inputs(), NOW, &defaults()).verdict;
    assert!(matches!(v, Verdict::NotRecoverable { .. }));
    assert!(
        v.blockers()
            .iter()
            .any(|b| b.key == concat!("recovery", ".blocker.chain_tampering")
                && b.kind == BlockerKind::Definite)
    );
}

#[test]
fn a_question_that_cannot_be_asked_is_an_unreadable_chain() {
    let store = MemoryStore::new();
    let expected = Expected {
        tenant: TenantId::new("acme").expect("a valid tenant"),
        source: String::new(),
        generation: None,
    };
    let chain = block_on(check_chain(
        &store,
        &VerifyingKeys::new(),
        &expected,
        Mode::Light,
    ));
    assert_eq!(chain.state, ChainState::Unreadable);
    assert!(chain.findings.is_empty());
}

#[test]
fn an_empty_snapshot_listing_is_no_snapshot() {
    let store = MemoryStore::new();
    let facts = block_on(read_snapshots(&store, "acme", &NoKeys));
    assert!(facts.readable);
    assert_eq!(facts.count, 0);
    assert_eq!(facts.oldest, None);
    let mut fx = Fixture::healthy();
    fx.snapshots = facts;
    let v = assess(&fx.inputs(), NOW, &defaults()).verdict;
    assert_eq!(
        v.blockers()[0].key,
        concat!("recovery", ".blocker.no_snapshot")
    );
}

#[test]
fn an_invalid_tenant_is_an_unreadable_snapshot_listing() {
    let store = MemoryStore::new();
    let facts = block_on(read_snapshots(&store, "../escape", &NoKeys));
    assert_eq!(facts, SnapshotFacts::unreadable());
}

#[test]
fn the_oldest_snapshot_is_the_one_taken_first() {
    let info = |generation, position, verified, taken_at_millis| SnapshotInfo {
        position,
        generation,
        verified,
        taken_at_millis,
    };
    let facts = SnapshotFacts::from_infos(&[
        info(1, 300, true, 3_000),
        info(1, 100, false, 1_000),
        info(1, 200, true, 2_000),
    ]);
    assert_eq!(facts.count, 3);
    assert_eq!(facts.verified, 2);
    assert_eq!(
        facts.oldest,
        Some(OldestSnapshot {
            position: Position::new(1, 100),
            taken_at_millis: 1_000,
        })
    );
}
