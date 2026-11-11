//! The wire model and state machines: stable names, integrity, gaps, duplicates, the two-phase
//! checkpoint and version negotiation, exercised through the public API.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(missing_docs)]

use spot_events_contract::SourceId;
use spot_sync_protocol::{
    Ack, Action, Batch, Checkpoint, CheckpointStore, Error, Hello, MemoryCheckpoints, MemorySink,
    Nack, NodeId, Offer, Outcome, ProtocolRange, ProtocolVersion, Receiver, ReceiverConfig, Record,
    RecordName, Result, Sender, SenderConfig,
};

fn source(partition: &str) -> SourceId {
    SourceId::new(&format!("localhost#{partition}")).unwrap()
}

fn records(from: u64, count: u64) -> Vec<Record> {
    (0..count)
        .map(|index| {
            let seq = from + index;
            Record::new(
                RecordName::new(&format!("doc_{seq}")).unwrap(),
                1,
                seq,
                format!("v{seq}"),
            )
            .unwrap()
        })
        .collect()
}

fn offer(source: SourceId, from: u64, count: u64) -> Offer {
    Offer::new(source, records(from, count))
}

#[test]
fn wire_names_are_stable() {
    let batch = Batch::new(source("hub"), records(1, 1)).unwrap();
    let json = serde_json::to_string(&batch).unwrap();
    for key in ["source", "from_seq", "to_seq", "records", "checksum"] {
        assert!(json.contains(&format!("\"{key}\"")), "{json} lacks {key}");
    }
    let back: Batch = serde_json::from_str(&json).unwrap();
    assert_eq!(back, batch);
    back.verify().unwrap();

    let ack = serde_json::to_string(&Ack::new(source("hub"), 4)).unwrap();
    assert!(ack.contains("\"through_seq\":4"));

    let gap = serde_json::to_string(&Nack::Gap {
        source: source("hub"),
        expected: 9,
    })
    .unwrap();
    assert!(gap.contains("\"reason\":\"gap\""), "{gap}");

    let hello = serde_json::to_string(&Hello::new(
        NodeId::new("edge-1").unwrap(),
        ProtocolRange::only(ProtocolVersion::new(2)),
        5,
    ))
    .unwrap();
    for key in ["node", "protocol_range", "metadata_epoch"] {
        assert!(hello.contains(&format!("\"{key}\"")), "{hello} lacks {key}");
    }
}

#[test]
fn a_checksum_mismatch_is_refused_and_re_requested() {
    let mut receiver = Receiver::new(
        ReceiverConfig::default(),
        MemorySink::new(),
        MemoryCheckpoints::new(),
    )
    .unwrap();
    let mut batch = Batch::new(source("hub"), records(1, 3)).unwrap();
    batch.records[1].payload = "tampered".to_owned();
    match receiver.on_batch(&batch) {
        Outcome::Nack(Nack::Corrupt { from_seq, .. }) => assert_eq!(from_seq, 1),
        other => panic!("expected corrupt, got {other:?}"),
    }
    assert!(receiver.sink().is_empty());

    let mut sender = Sender::new(SenderConfig::default()).unwrap();
    // Prime a cursor, then a corrupt nack rewinds it to the refused batch's first sequence.
    assert!(matches!(
        sender.next(&offer(source("hub"), 1, 3)),
        Action::Send(_)
    ));
    sender.on_nack(&Nack::Corrupt {
        source: source("hub"),
        from_seq: 1,
    });
    assert_eq!(sender.next_seq(&source("hub")), 1);
}

#[test]
fn a_gap_nacks_and_rewinds_to_resume_without_a_hole() {
    let mut receiver = Receiver::new(
        ReceiverConfig::default(),
        MemorySink::new(),
        MemoryCheckpoints::new(),
    )
    .unwrap();
    // The batch for sequences 5..=6 arrives first.
    match receiver.on_batch(&Batch::new(source("hub"), records(5, 2)).unwrap()) {
        Outcome::Nack(Nack::Gap { expected, .. }) => assert_eq!(expected, 1),
        other => panic!("expected gap, got {other:?}"),
    }
    assert!(receiver.sink().is_empty());
    // Re-send from the checkpoint: 1..=6, then 7..=8.
    assert!(
        receiver
            .on_batch(&Batch::new(source("hub"), records(1, 6)).unwrap())
            .is_ack()
    );
    assert!(
        receiver
            .on_batch(&Batch::new(source("hub"), records(7, 2)).unwrap())
            .is_ack()
    );
    let seqs: Vec<u64> = receiver
        .sink()
        .order()
        .iter()
        .map(|applied| applied.seq)
        .collect();
    assert_eq!(seqs, (1..=8).collect::<Vec<_>>());
    assert_eq!(
        receiver
            .stored_checkpoint(&source("hub"))
            .unwrap()
            .last_applied_seq,
        8
    );
}

#[test]
fn a_duplicate_batch_is_acknowledged_but_not_reapplied() {
    let mut receiver = Receiver::new(
        ReceiverConfig::default(),
        MemorySink::new(),
        MemoryCheckpoints::new(),
    )
    .unwrap();
    let batch = Batch::new(source("hub"), records(1, 3)).unwrap();
    assert!(receiver.on_batch(&batch).is_ack());
    assert!(receiver.on_batch(&batch).is_ack());
    assert_eq!(receiver.sink().len(), 3);
    assert_eq!(receiver.sink().order().len(), 3);
}

#[test]
fn a_crash_between_apply_and_checkpoint_is_absorbed() {
    /// Fails the first checkpoint write, modelling a crash after the durable apply.
    struct FailOnce {
        inner: MemoryCheckpoints,
        fail_next: bool,
    }
    impl CheckpointStore for FailOnce {
        fn load(&self, source: &SourceId) -> Option<Checkpoint> {
            self.inner.load(source)
        }
        fn store(&mut self, checkpoint: &Checkpoint) -> Result<()> {
            if self.fail_next {
                self.fail_next = false;
                return Err(Error::store_failed("injected"));
            }
            self.inner.store(checkpoint)
        }
    }

    let mut receiver = Receiver::new(
        ReceiverConfig::default(),
        MemorySink::new(),
        FailOnce {
            inner: MemoryCheckpoints::new(),
            fail_next: true,
        },
    )
    .unwrap();
    let batch = Batch::new(source("hub"), records(1, 3)).unwrap();
    assert!(matches!(receiver.on_batch(&batch), Outcome::Failed(_)));
    // Durable apply landed; the checkpoint did not.
    assert_eq!(receiver.sink().len(), 3);
    assert!(receiver.stored_checkpoint(&source("hub")).is_none());
    // A restart discards the volatile cache; the redelivery re-applies (a no-op) and checkpoints.
    receiver.crash();
    assert!(receiver.on_batch(&batch).is_ack());
    assert_eq!(receiver.sink().len(), 3);
    assert_eq!(receiver.sink().order().len(), 3, "no record applied twice");
    assert_eq!(
        receiver
            .stored_checkpoint(&source("hub"))
            .unwrap()
            .last_applied_seq,
        3
    );
}

#[test]
fn an_oversize_batch_is_refused() {
    let mut receiver = Receiver::new(
        ReceiverConfig::new(2).unwrap(),
        MemorySink::new(),
        MemoryCheckpoints::new(),
    )
    .unwrap();
    match receiver.on_batch(&Batch::new(source("hub"), records(1, 5)).unwrap()) {
        Outcome::Nack(Nack::BatchTooLarge { limit, got, .. }) => {
            assert_eq!((limit, got), (2, 5));
        }
        other => panic!("expected batch_too_large, got {other:?}"),
    }
    // The sender halves its cap and retries from the last acknowledged sequence.
    let mut sender = Sender::new(SenderConfig::new(8, 8).unwrap()).unwrap();
    sender.on_nack(&Nack::BatchTooLarge {
        source: source("hub"),
        limit: 2,
        got: 5,
    });
    let Action::Send(batch) = sender.next(&offer(source("hub"), 1, 8)) else {
        panic!("expected a smaller batch");
    };
    assert!(batch.len() <= 4);
}

#[test]
fn the_window_backpressures_the_sender() {
    let mut sender = Sender::new(SenderConfig::new(2, 3).unwrap()).unwrap();
    assert!(matches!(
        sender.next(&offer(source("hub"), 1, 10)),
        Action::Send(_)
    ));
    assert!(matches!(
        sender.next(&offer(source("hub"), 1, 10)),
        Action::Send(_)
    ));
    assert!(sender.next(&offer(source("hub"), 1, 10)).is_wait());
    sender.on_ack(&Ack::new(source("hub"), 4));
    assert!(matches!(
        sender.next(&offer(source("hub"), 1, 10)),
        Action::Send(_)
    ));
}

#[test]
fn version_negotiation_picks_the_highest_shared() {
    let node = NodeId::new("edge-1").unwrap();
    let peer = Hello::new(
        node.clone(),
        ProtocolRange::new(ProtocolVersion::new(1), ProtocolVersion::new(3)).unwrap(),
        1,
    );
    let ours = Hello::new(
        node.clone(),
        ProtocolRange::new(ProtocolVersion::new(2), ProtocolVersion::new(4)).unwrap(),
        1,
    );
    assert_eq!(ours.negotiate(&peer), Ok(ProtocolVersion::new(3)));
    let disjoint = Hello::new(node, ProtocolRange::only(ProtocolVersion::new(9)), 1);
    assert!(matches!(
        ours.negotiate(&disjoint),
        Err(Nack::VersionMismatch { .. })
    ));
}

#[test]
fn a_sender_then_receiver_round_trip_applies_once_in_order() {
    let src = source("hub");
    let mut sender = Sender::new(SenderConfig::new(4, 16).unwrap()).unwrap();
    let mut receiver = Receiver::new(
        ReceiverConfig::default(),
        MemorySink::new(),
        MemoryCheckpoints::new(),
    )
    .unwrap();
    let mut ack = 0;
    loop {
        match sender.next(&offer(src.clone(), 1, 10)) {
            Action::Send(batch) => {
                let outcome = receiver.on_batch(&batch);
                let Ack { through_seq, .. } = outcome.into_ack().unwrap();
                ack = through_seq;
                sender.on_ack(&Ack::new(src.clone(), through_seq));
            }
            Action::Wait => break,
            // `Action` is non-exhaustive; a future variant would also mean "stop here".
            _ => break,
        }
    }
    assert_eq!(ack, 10);
    assert_eq!(receiver.sink().len(), 10);
}
