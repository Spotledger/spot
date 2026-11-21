//! Seeded-corruption proofs on PostgreSQL: for each corruption, a clean workload passes every
//! invariant, then exactly the expected invariants fail and name the entity and an id.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_l7_invariants::corrupt::{pg_corruptors, prove};

const WORKERS: u32 = 4;
const OPERATIONS: u64 = 600;
const SEED: u64 = 7_000_007;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_corruption_is_caught_by_exactly_the_expected_invariants() {
    let mut proven = 0;
    for corruptor in pg_corruptors() {
        let Some(result) = prove(corruptor.as_ref(), WORKERS, OPERATIONS, SEED).await else {
            return;
        };
        let proof = result.unwrap_or_else(|e| panic!("{}: {e}", corruptor.name()));
        println!(
            "{}: before={:?} after={:?} ({} violations of the expected invariants)",
            proof.corruption,
            proof.failed_before,
            proof.failed_after,
            proof.violations.len()
        );
        assert!(
            proof.holds(corruptor.as_ref()),
            "corruption `{}` (seed {SEED}) was not caught as expected: {proof:#?}",
            corruptor.name()
        );
        proven += 1;
    }
    assert_eq!(proven, pg_corruptors().len());
}
