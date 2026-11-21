//! The full adapter flow of `ProviderSubjectKeys` with this store behind two nodes: the ttl makes
//! node B see node A's destroy, and a restart keeps the keys.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{fixture, node};
use spot_journal_pii::SubjectKeys;
use spot_journal_pii_secrets::testing::{CounterRng, ManualClock, MemoryAudit};
use spot_journal_pii_secrets::{AdapterConfig, ProviderSubjectKeys};
use spot_journal_pii_secrets_pg::PgWrappedKeyStore;
use spot_secrets::MemoryKeyring;

type Keys = ProviderSubjectKeys<Arc<MemoryKeyring>, PgWrappedKeyStore, Arc<MemoryAudit>>;

struct Node {
    keys: Keys,
    clock: ManualClock,
    rng: Arc<CounterRng>,
    audit: Arc<MemoryAudit>,
}

fn adapter(provider: &Arc<MemoryKeyring>, store: PgWrappedKeyStore, seed: u64) -> Node {
    let clock = ManualClock::new(1_000_000);
    let rng = Arc::new(CounterRng::new(seed));
    let audit = Arc::new(MemoryAudit::new());
    let keys = ProviderSubjectKeys::new(
        AdapterConfig::new("acme", b"tenant-salt".to_vec()),
        provider.clone(),
        store,
        audit.clone(),
        Box::new(rng.clone()),
        Box::new(clock.clone()),
    )
    .expect("adapter");
    Node {
        keys,
        clock,
        rng,
        audit,
    }
}

fn key(n: &Node, subject: &str) -> Option<[u8; 32]> {
    n.keys.key_for(subject).unwrap().map(|k| *k.expose())
}

#[test]
fn a_destroy_on_node_a_reaches_node_b_after_the_ttl_and_no_second_key_is_made() {
    let Some(fx) = fixture() else { return };
    let provider = Arc::new(MemoryKeyring::new("acme-kek").expect("keyring"));
    let a = adapter(&provider, node(&fx, "pii_node_a"), 1);
    let b = adapter(&provider, node(&fx, "pii_node_b"), 1_000);

    let original = key(&a, "alice").expect("created on A");
    assert_eq!(
        key(&b, "alice"),
        Some(original),
        "B reads A's key from the table"
    );
    assert_eq!(b.rng.calls(1_000), 0);

    a.keys.destroy("alice").unwrap();
    assert_eq!(a.audit.events().len(), 1);
    assert_eq!(key(&a, "alice"), None);

    // Inside the ttl B still serves from memory; past it B reads the tombstone.
    b.clock.advance(59_999);
    assert_eq!(key(&b, "alice"), Some(original));
    b.clock.advance(1);
    assert_eq!(key(&b, "alice"), None);
    assert_eq!(key(&b, "alice"), None);
    assert_eq!(b.rng.calls(1_000), 0, "B never made a second key");
    assert!(b.audit.events().is_empty(), "B audits nothing");
}

#[test]
fn a_restart_keeps_every_key_and_every_erasure() {
    let Some(fx) = fixture() else { return };
    let provider = Arc::new(MemoryKeyring::new("acme-kek").expect("keyring"));
    let (alice, bob) = {
        let first = adapter(&provider, node(&fx, "pii_restart_1"), 1);
        let alice = key(&first, "alice").unwrap();
        let bob = key(&first, "bob").unwrap();
        first.keys.destroy("bob").unwrap();
        (alice, bob)
    };
    let second = adapter(&provider, node(&fx, "pii_restart_2"), 5_000);
    assert_eq!(
        key(&second, "alice"),
        Some(alice),
        "alice is still readable"
    );
    assert_ne!(Some(bob), key(&second, "alice"));
    assert_eq!(
        key(&second, "bob"),
        None,
        "bob stays erased across a restart"
    );
    assert_eq!(
        second.rng.calls(5_000),
        0,
        "no key was made on the second node"
    );
}
