//! For any batch sizes and seal points, what the segments hold equals what went in.

use proptest::prelude::*;
use spot_journal_segment::verify_chain;

use super::support::*;
use crate::{Durability, RelayError};

async fn run(durability: Durability, sizes: Vec<(u64, bool)>) -> Result<(), RelayError> {
    let store = FaultStore::new();
    let mut relay = open(&store, never_due(durability)).await?;
    let mut next = 1u64;
    for (index, (size, seal_after)) in sizes.iter().enumerate() {
        let batch_id = u64::try_from(index).unwrap_or(0) + 1;
        relay
            .append_batch(
                batch(batch_id, next, next + size - 1),
                2_000 + batch_id as i64,
            )
            .await?;
        next += size;
        if *seal_after {
            relay.seal_now(3_000).await?;
        }
    }
    relay.seal_now(4_000).await?;
    let total = next - 1;

    let segments = stored_segments(&store, 1).await;
    assert_eq!(entries_of(&segments), input(1, 1, total));
    if !segments.is_empty() {
        verify_chain(&segments).expect("chain verifies");
    }
    let manifest = stored_manifest(&store, 1).await;
    assert_eq!(manifest.last_seq(), total);
    assert_eq!(manifest.segments().len(), segments.len());
    assert!(store.keys_under(&tail_prefix(1)).await.is_empty());
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn segments_read_back_equal_the_input(
        sizes in proptest::collection::vec((1u64..6, any::<bool>()), 1..12),
        remote in any::<bool>(),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let durability = if remote { Durability::RemoteSync } else { Durability::Local };
        runtime.block_on(run(durability, sizes)).unwrap();
    }
}
