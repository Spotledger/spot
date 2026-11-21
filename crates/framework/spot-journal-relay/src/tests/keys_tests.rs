//! The shared test keys (`TailKeys::for_tests`) and the size bound a batcher stops at.

use std::sync::Arc;

use spot_events_contract::envelope::Position;
use spot_secrets::MemoryKeyring;

use crate::tail::{
    self, CHUNK_FRAMING_BYTES, ENTRY_OVERHEAD_BOUND, MAX_CHUNK_BYTES, Rule, TailKeys,
    encoded_len_upper_bound, encoded_len_upper_bound_for, encoded_len_upper_bound_of,
};
use crate::{Durability, RelayConfig};

use super::support::{SOURCE, TENANT, input, tail_key};

/// A small deterministic generator (a seeded xorshift), so a failing batch can be replayed.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

fn random_batch(rng: &mut Seeded) -> Vec<(Position, Vec<u8>)> {
    let count = 1 + rng.below(12);
    (1..=count)
        .map(|seq| {
            let len = usize::try_from(rng.below(3_000)).unwrap();
            let text: String = (0..len)
                .map(|_| char::from(b'a' + u8::try_from(rng.below(26)).unwrap()))
                .collect();
            (Position { generation: 1, seq }, text.into_bytes())
        })
        .collect()
}

#[tokio::test]
async fn the_test_keys_are_deterministic() {
    let first = TailKeys::for_tests(TENANT);
    let second = TailKeys::for_tests(TENANT);
    assert_eq!(
        first.mac_current(&[b"same input"]).await.unwrap(),
        second.mac_current(&[b"same input"]).await.unwrap()
    );
    // A chunk sealed under one instance opens under another of the same tenant.
    let entries = input(1, 1, 3);
    let bytes = tail::encode_v2(&first, &tail_key(1, 1), 1, &entries)
        .await
        .unwrap();
    let chunk = tail::decode(TENANT, 1, Some(&second), &tail_key(1, 1), &bytes)
        .await
        .unwrap();
    assert_eq!(chunk.entries, entries);
}

#[tokio::test]
async fn the_test_keys_are_separated_by_tenant() {
    let a = TailKeys::for_tests("acme");
    let b = TailKeys::for_tests("beta");
    assert_ne!(
        a.mac_current(&[b"same input"]).await.unwrap(),
        b.mac_current(&[b"same input"]).await.unwrap()
    );
    let bytes = tail::encode_v2(&a, &tail_key(1, 1), 1, &input(1, 1, 3))
        .await
        .unwrap();
    let error = tail::decode(TENANT, 1, Some(&b), &tail_key(1, 1), &bytes)
        .await
        .expect_err("another tenant's keys do not verify the chunk");
    assert_eq!(error.rule(), Rule::B);
}

#[test]
fn a_production_config_carries_no_default_keys() {
    for durability in [
        Durability::Local,
        Durability::ReplicaSync,
        Durability::RemoteSync,
    ] {
        assert!(
            RelayConfig::new(TENANT, SOURCE, durability)
                .tail_keys
                .is_none(),
            "{durability:?}"
        );
    }
}

#[test]
fn the_upper_bound_counts_the_framing_and_each_entry() {
    assert_eq!(encoded_len_upper_bound(&[]), CHUNK_FRAMING_BYTES);
    assert_eq!(
        encoded_len_upper_bound_of([10, 20]),
        CHUNK_FRAMING_BYTES + 2 * ENTRY_OVERHEAD_BOUND + 30
    );
    assert_eq!(
        encoded_len_upper_bound(&input(1, 1, 3)),
        encoded_len_upper_bound_of(input(1, 1, 3).iter().map(|(_, line)| line.len()))
    );
    assert_eq!(
        encoded_len_upper_bound_for(2, 30),
        encoded_len_upper_bound_of([10, 20])
    );
    assert!(encoded_len_upper_bound_of([usize::MAX, usize::MAX]) >= MAX_CHUNK_BYTES);
}

#[tokio::test]
async fn the_upper_bound_is_never_below_the_real_length_over_200_random_batches() {
    let providers: [TailKeys; 2] = [
        TailKeys::for_tests(TENANT),
        TailKeys::new(Arc::new(MemoryKeyring::new("tenant-journal-key").unwrap())),
    ];
    let mut rng = Seeded(0x5eed_1234_abcd_0001);
    for round in 0..200_u64 {
        let entries = random_batch(&mut rng);
        let keys = &providers[usize::try_from(round % 2).unwrap()];
        let real = tail::encode_v2(keys, &tail_key(1, 1), 1, &entries)
            .await
            .unwrap();
        let bound = encoded_len_upper_bound(&entries);
        let real_len = u64::try_from(real.len()).unwrap();
        assert!(
            bound >= real_len,
            "round {round}: bound {bound} below real {real_len}"
        );
        // Not absurdly loose either: a batcher using it must not split needlessly.
        assert!(bound <= real_len + 520 * u64::try_from(entries.len()).unwrap() + 100);
    }
}
