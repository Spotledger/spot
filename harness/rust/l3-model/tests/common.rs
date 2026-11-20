//! Shared by the backend suites.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, dead_code)]

use spot_l3_model::runner::run_sequence;
use spot_l3_model::shrink::{Check, shrink, to_rust};
use spot_l3_model::{Call, Divergence, GenConfig, Model, generate};
use spot_storage_contract::StorageBackend;

/// The default base seed: fixed, so CI is deterministic.
pub const DEFAULT_SEED: u64 = 20_261_113;

pub fn base_seed() -> u64 {
    std::env::var("SPOT_L3_MODEL_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SEED)
}

pub fn sequences(default: u64) -> u64 {
    std::env::var("SPOT_L3_MODEL_SEQUENCES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

pub fn calls_of(seed: u64) -> Vec<Call> {
    generate(seed, GenConfig::default())
}

/// Runs `count` seeded sequences with a fresh backend each; on a divergence shrinks it and fails
/// with the minimal sequence and its seed.
pub async fn run_many<B, F>(count: u64, make: F)
where
    B: StorageBackend + 'static,
    F: Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = B> + Send>>
        + Send
        + Sync
        + 'static,
{
    let base = base_seed();
    let make = std::sync::Arc::new(make);
    for i in 0..count {
        let seed = base + i;
        let calls = calls_of(seed);
        let result = run_sequence(make().await, &calls, Model::new()).await;
        if let Err(first) = result {
            let make2 = make.clone();
            let check: Check<'_> = Box::new(move |c: Vec<Call>| {
                let make3 = make2.clone();
                Box::pin(
                    async move { run_sequence(make3().await, &c, Model::new()).await.is_err() },
                )
            });
            let small = shrink(calls, check).await;
            let again: Result<_, Divergence> = run_sequence(make().await, &small, Model::new())
                .await
                .map(|_| ());
            panic!(
                "seed {seed} diverged: {first}\nminimal ({} calls): {again:?}\n{}",
                small.len(),
                to_rust(seed, &small)
            );
        }
    }
}
