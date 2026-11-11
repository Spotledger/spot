//! The property: however pull, ack and restart interleave, a checkpoint never moves backwards
//! except through an explicit restart, which sets it to exactly the stated position.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use proptest::prelude::*;
use spot_event_stream::{ConsumerSpec, PayloadMode, Start, TopicSelector, position_to_feed};
use spot_events_contract::envelope::Position;
use spot_permissions::Principal;
use support::{doc_record, fixture_full, rid};

fn spec() -> ConsumerSpec {
    ConsumerSpec {
        name: "prop".to_owned(),
        owner: "owner".to_owned(),
        topics: vec![TopicSelector::Prefix("doc".to_owned())],
        filter: None,
        partitions: 1,
        payload_mode: PayloadMode::IdsOnly,
        start: Start::FromBeginning,
    }
}

async fn run(ops: Vec<u8>, restarts: Vec<u64>) -> bool {
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        spot_event_stream::BatchPolicy::default(),
        spot_event_stream::ParkPolicy::default(),
    )
    .await;
    for seq in 1..=6u64 {
        f.feed.push(doc_record(0, seq, rid(seq)));
    }
    f.engine
        .register(spec(), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("prop").await.unwrap();
    let mut last = f.engine.checkpoint("prop", 0).await.unwrap();
    let mut restart_index = 0usize;

    for op in ops {
        match op % 3 {
            0 => {
                let _ = f.engine.pull(&lease, 0, None, false).await;
            }
            1 => {
                if let Ok(batch) = f.engine.pull(&lease, 0, Some(1), false).await
                    && let Some(position) = batch.next_position
                {
                    let _ = f.engine.ack(&lease, 0, position).await;
                }
            }
            _ => {
                if !restarts.is_empty() {
                    let seq = restarts[restart_index % restarts.len()] % 8;
                    restart_index += 1;
                    let from = Position { generation: 0, seq };
                    if f.engine.restart(&lease, 0, from).await.is_ok() {
                        // A restart is the one operation allowed to move backwards; it sets the
                        // checkpoint to exactly `from`.
                        let now = f.engine.checkpoint("prop", 0).await.unwrap();
                        if position_to_feed(now) != position_to_feed(from) {
                            return false;
                        }
                        last = now;
                        continue;
                    }
                }
            }
        }
        let now = f.engine.checkpoint("prop", 0).await.unwrap();
        if position_to_feed(now) < position_to_feed(last) {
            return false;
        }
        last = now;
    }
    true
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn a_checkpoint_never_moves_backwards(
        ops in prop::collection::vec(0u8..3, 0..40),
        restarts in prop::collection::vec(0u64..8, 0..40),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let held = runtime.block_on(run(ops, restarts));
        prop_assert!(held, "a checkpoint moved backwards without a restart");
    }
}

/// The alphabet used to generate names and topics: lowercase letters, digits and `_`, plus the
/// three reserved prefixes spliced in so a generated topic is a reserved one about half the
/// time (a purely random topic almost never starts with a reserved prefix, so without this
/// splicing the property below would not exercise the reserved case at all).
fn name_strategy() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,12}"
}

fn topic_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => name_strategy(),
        1 => name_strategy().prop_map(|rest| format!("doc_{rest}")),
        1 => name_strategy().prop_map(|rest| format!("sys_{rest}")),
        1 => name_strategy().prop_map(|rest| format!("identity_{rest}")),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The property that `d.*` violated: a selector that matches a reserved topic is never
    /// unreserved. `p` is chosen half the time as a random non-empty prefix of the generated
    /// topic `t` (a wholly random `p` is almost never a prefix of a reserved topic, so that
    /// case alone would not have caught the bug).
    #[test]
    fn a_selector_that_matches_a_reserved_topic_is_never_unreserved(
        t in topic_strategy(),
        from_topic in any::<bool>(),
        random_p in name_strategy(),
        cut in 1usize..=13,
        as_exact in any::<bool>(),
    ) {
        let p = if from_topic {
            let end = cut.min(t.len()).max(1);
            t[..end].to_owned()
        } else {
            random_p
        };
        if p.is_empty() {
            return Ok(());
        }
        let reserved_topic = spot_events_contract::Topic::RESERVED_PREFIXES
            .iter()
            .any(|class| t.starts_with(class));

        let prefix = TopicSelector::Prefix(p.clone());
        if prefix.matches(&t) && reserved_topic {
            prop_assert!(
                prefix.reserved_class().is_some(),
                "Prefix({p:?}) matches reserved topic {t:?} but reserved_class() is None"
            );
        }

        if as_exact {
            let exact = TopicSelector::Exact(t.clone());
            if exact.matches(&t) && reserved_topic {
                prop_assert!(
                    exact.reserved_class().is_some(),
                    "Exact({t:?}) matches reserved topic {t:?} but reserved_class() is None"
                );
            }
        }
    }
}
