//! The shipper against a fake relay that remembers what it was given, so a test can kill the
//! shipper at every step boundary and read exactly what the "journal" holds afterwards.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_events_contract::envelope::Position;
use spot_journal_commit::{ChunkRelay, ShipEntry, ShipError, Shipped};
use spot_journal_manifest::Fork;
use spot_journal_ship::memory::{MemoryLease, MemoryOutbox, MemoryWatermarks, RecordingAlarms};
use spot_journal_ship::{
    AlarmKind, ChainChecker, ChainState, PassThrough, PortFuture, Ports, Protector, RelayFactory,
    ShipFault, ShipRelay, Shipper, ShipperConfig, Step,
};

const SOURCE: &str = "acme";

/// What the journal holds, shared by every relay a test opens (a restart reopens over it).
#[derive(Default)]
struct World {
    /// `(generation, seq, line)` made durable by an append (remote-sync tail chunks).
    held: Mutex<Vec<(u64, u64, Vec<u8>)>>,
    sealed: Mutex<u64>,
    opens: Mutex<Vec<(u64, Option<Fork>)>>,
    /// The next append fails before it writes.
    fail_before: AtomicBool,
    /// The next append writes and then reports a failure (a crash after the write).
    fail_after: AtomicBool,
    /// The next seal fails.
    fail_seal: AtomicBool,
    /// Seal everything held when asked.
    seal_all: AtomicBool,
}

impl World {
    fn seqs(&self, generation: u64) -> Vec<u64> {
        self.held
            .lock()
            .unwrap()
            .iter()
            .filter(|(g, _, _)| *g == generation)
            .map(|(_, s, _)| *s)
            .collect()
    }

    fn last(&self, generation: u64) -> u64 {
        self.seqs(generation).last().copied().unwrap_or(0)
    }
}

struct FakeRelay {
    world: Arc<World>,
    generation: u64,
    remote: bool,
}

impl ChunkRelay for FakeRelay {
    fn generation(&self) -> u64 {
        self.generation
    }

    fn next_seq(&self) -> u64 {
        self.world.last(self.generation) + 1
    }

    async fn ship(
        &mut self,
        _batch_id: u64,
        entries: &[ShipEntry],
        _now_millis: u64,
    ) -> Result<Shipped, ShipError> {
        if self.world.fail_before.swap(false, Ordering::SeqCst) {
            return Err(ShipError::StoreDown {
                code: "journal_relay.store_failed",
            });
        }
        for (expect, entry) in (self.next_seq()..).zip(entries) {
            if entry.seq != expect {
                return Err(ShipError::Stopped {
                    code: "journal_relay.batch_gap",
                });
            }
        }
        {
            let mut held = self.world.held.lock().unwrap();
            for entry in entries {
                held.push((self.generation, entry.seq, entry.line.clone()));
            }
        }
        if self.world.fail_after.swap(false, Ordering::SeqCst) {
            return Err(ShipError::StoreDown {
                code: "journal_relay.store_failed",
            });
        }
        let up_to_seq = entries.last().map_or(0, |e| e.seq);
        Ok(if self.remote {
            Shipped::Durable { up_to_seq }
        } else {
            Shipped::Buffered
        })
    }

    async fn resume(&mut self) -> Result<(), ShipError> {
        Ok(())
    }
}

impl ShipRelay for FakeRelay {
    async fn seal_if_due(&mut self, _now_millis: i64) -> Result<bool, ShipFault> {
        if self.world.fail_seal.swap(false, Ordering::SeqCst) {
            return Err(ShipFault::StoreDown {
                code: "journal_relay.store_failed",
            });
        }
        if self.world.seal_all.load(Ordering::SeqCst) {
            let last = self.world.last(self.generation);
            let mut sealed = self.world.sealed.lock().unwrap();
            if last > *sealed {
                *sealed = last;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn sealed_through(&self) -> u64 {
        *self.world.sealed.lock().unwrap()
    }
}

struct FakeFactory {
    world: Arc<World>,
    remote: bool,
}

impl RelayFactory for FakeFactory {
    type Relay = FakeRelay;

    fn open<'a>(
        &'a self,
        generation: u64,
        fork: Option<Fork>,
        _now_millis: i64,
    ) -> PortFuture<'a, Result<FakeRelay, ShipFault>> {
        Box::pin(async move {
            self.world.opens.lock().unwrap().push((generation, fork));
            if !self.remote {
                // A restarted local relay has lost its open segment.
                let sealed = *self.world.sealed.lock().unwrap();
                self.world
                    .held
                    .lock()
                    .unwrap()
                    .retain(|(_, s, _)| *s <= sealed);
            }
            Ok(FakeRelay {
                world: Arc::clone(&self.world),
                generation,
                remote: self.remote,
            })
        })
    }
}

struct Rig {
    world: Arc<World>,
    outbox: Arc<MemoryOutbox>,
    marks: Arc<MemoryWatermarks>,
    lease: Arc<MemoryLease>,
    alarms: Arc<RecordingAlarms>,
    now: i64,
}

impl Rig {
    fn new() -> Self {
        Self {
            world: Arc::new(World::default()),
            outbox: Arc::new(MemoryOutbox::new()),
            marks: Arc::new(MemoryWatermarks::new()),
            lease: Arc::new(MemoryLease::held()),
            alarms: Arc::new(RecordingAlarms::new()),
            now: 1_000_000,
        }
    }

    fn shipper_with(
        &self,
        remote: bool,
        batch: usize,
        protector: Arc<dyn Protector>,
        checker: Option<Arc<dyn ChainChecker>>,
    ) -> Shipper<FakeFactory> {
        let mut cfg =
            ShipperConfig::new(SOURCE, if remote { "remote-sync" } else { "local" }, remote);
        cfg.batch_max_entries = batch;
        cfg.rpo_grace_millis = 5_000;
        cfg.verify_every_millis = 1_000;
        let (shipper, _) = Shipper::new(
            cfg,
            Ports {
                outbox: self.outbox.clone(),
                marks: self.marks.clone(),
                lease: self.lease.clone(),
                protector,
                alarms: self.alarms.clone(),
                factory: FakeFactory {
                    world: Arc::clone(&self.world),
                    remote,
                },
                checker,
                backup: None,
            },
        )
        .unwrap();
        shipper
    }

    fn shipper(&self, remote: bool, batch: usize) -> Shipper<FakeFactory> {
        self.shipper_with(remote, batch, Arc::new(PassThrough), None)
    }

    async fn step(&mut self, shipper: &mut Shipper<FakeFactory>) -> Step {
        self.now += 1_000;
        shipper.step(self.now).await
    }

    /// Steps until the shipper is idle, allowing failures to be retried.
    async fn drain(&mut self, shipper: &mut Shipper<FakeFactory>) {
        for _ in 0..60 {
            if matches!(self.step(shipper).await, Step::Idle) {
                return;
            }
        }
        panic!("the shipper did not settle");
    }

    fn mark(&self) -> (u64, u64) {
        let mark = self.marks.get(SOURCE).unwrap();
        (mark.position.generation, mark.position.seq)
    }
}

#[tokio::test]
async fn it_ships_in_order_in_batches_and_advances_only_to_durable() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 5);
    let mut shipper = rig.shipper(true, 2);
    let first = rig.step(&mut shipper).await;
    assert_eq!(
        first,
        Step::Shipped {
            entries: 2,
            durable: Position {
                generation: 1,
                seq: 2
            }
        }
    );
    assert_eq!(rig.mark(), (1, 2));
    rig.drain(&mut shipper).await;
    assert_eq!(rig.world.seqs(1), vec![1, 2, 3, 4, 5]);
    assert_eq!(rig.mark(), (1, 5));
}

#[tokio::test]
async fn under_local_the_watermark_follows_the_sealed_position_not_the_buffered_one() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 3);
    let mut shipper = rig.shipper(false, 10);
    rig.step(&mut shipper).await;
    assert_eq!(rig.world.seqs(1), vec![1, 2, 3], "the relay holds them");
    assert_eq!(
        rig.mark().1,
        0,
        "but nothing is durable until a segment is sealed"
    );
    rig.world.seal_all.store(true, Ordering::SeqCst);
    rig.step(&mut shipper).await;
    assert_eq!(rig.mark(), (1, 3));
}

#[tokio::test]
async fn a_restart_under_local_re_reads_what_was_only_buffered() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 4);
    let mut shipper = rig.shipper(false, 2);
    rig.step(&mut shipper).await; // 1,2 buffered, not sealed
    rig.world.seal_all.store(true, Ordering::SeqCst);
    rig.step(&mut shipper).await; // 3,4 buffered; seal covers 1..=4
    assert_eq!(rig.mark(), (1, 4));
    rig.outbox.push_range(1, 5, 6);
    rig.world.seal_all.store(false, Ordering::SeqCst);
    rig.step(&mut shipper).await; // 5,6 buffered only
    drop(shipper);
    // The process restarts: the open segment is lost, the watermark says 4.
    let mut again = rig.shipper(false, 10);
    rig.world.seal_all.store(true, Ordering::SeqCst);
    rig.drain(&mut again).await;
    assert_eq!(rig.world.seqs(1), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(rig.mark(), (1, 6));
}

#[tokio::test]
async fn a_failure_at_every_step_boundary_converges_without_gap_or_duplicate() {
    for boundary in [
        "read",
        "protect",
        "append_before",
        "append_after",
        "watermark",
        "seal",
    ] {
        let mut rig = Rig::new();
        rig.world.seal_all.store(true, Ordering::SeqCst);
        let flaky = Arc::new(FlakyProtector::default());
        let mut shipper = rig.shipper_with(true, 3, flaky.clone(), None);
        rig.outbox.push_range(1, 1, 2);
        assert!(matches!(rig.step(&mut shipper).await, Step::Shipped { .. }));
        rig.outbox.push_range(1, 3, 8);
        match boundary {
            "read" => rig.outbox.set_down(true),
            "protect" => flaky.fail.store(true, Ordering::SeqCst),
            "append_before" => rig.world.fail_before.store(true, Ordering::SeqCst),
            "append_after" => rig.world.fail_after.store(true, Ordering::SeqCst),
            "watermark" => rig.marks.set_down(true),
            "seal" => rig.world.fail_seal.store(true, Ordering::SeqCst),
            _ => unreachable!(),
        }
        let failed = rig.step(&mut shipper).await;
        assert!(
            matches!(failed, Step::Degraded { .. }),
            "{boundary}: expected a degraded step, got {failed:?}"
        );
        rig.outbox.set_down(false);
        flaky.fail.store(false, Ordering::SeqCst);
        rig.marks.set_down(false);
        rig.drain(&mut shipper).await;
        assert_eq!(
            rig.world.seqs(1),
            (1..=8).collect::<Vec<u64>>(),
            "{boundary}"
        );
        assert_eq!(rig.mark(), (1, 8), "{boundary}");
    }
}

#[tokio::test]
async fn killing_the_shipper_after_the_append_and_restarting_ships_each_entry_once() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 3);
    let mut shipper = rig.shipper(true, 10);
    rig.step(&mut shipper).await;
    rig.outbox.push_range(1, 4, 6);
    // The append lands, the process dies before the watermark is written.
    rig.marks.set_down(true);
    rig.step(&mut shipper).await;
    drop(shipper);
    rig.marks.set_down(false);
    assert_eq!(rig.mark(), (1, 3), "the watermark never moved");
    let mut again = rig.shipper(true, 10);
    rig.drain(&mut again).await;
    assert_eq!(rig.world.seqs(1), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(rig.mark(), (1, 6));
}

#[tokio::test]
async fn only_the_lease_holder_ships_and_a_deposed_writer_stops() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 4);
    let mut shipper = rig.shipper(true, 2);
    rig.step(&mut shipper).await;
    rig.lease.set_held(false);
    assert_eq!(rig.step(&mut shipper).await, Step::NotWriter);
    assert_eq!(
        rig.world.seqs(1),
        vec![1, 2],
        "nothing written without the lease"
    );
    rig.lease.set_held(true);
    rig.drain(&mut shipper).await;
    assert_eq!(rig.world.seqs(1), vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn a_new_outbox_generation_opens_a_fork_and_the_old_one_stays() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 3);
    let mut shipper = rig.shipper(true, 10);
    rig.drain(&mut shipper).await;
    rig.outbox.start_generation(2);
    rig.outbox.push_range(2, 1, 2);
    rig.drain(&mut shipper).await;
    assert_eq!(
        rig.world.seqs(1),
        vec![1, 2, 3],
        "generation 1 is untouched"
    );
    assert_eq!(rig.world.seqs(2), vec![1, 2]);
    assert_eq!(rig.mark(), (2, 2));
    let opens = rig.world.opens.lock().unwrap().clone();
    assert_eq!(opens.len(), 2);
    let fork = opens[1].1.clone().unwrap();
    assert_eq!(
        (opens[1].0, fork.generation, fork.forked_from_position),
        (2, 2, 3)
    );
    // A restart reopens the very same fork record.
    drop(shipper);
    let mut again = rig.shipper(true, 10);
    rig.outbox.push_range(2, 3, 3);
    rig.drain(&mut again).await;
    let opens = rig.world.opens.lock().unwrap().clone();
    assert_eq!(opens[2].1, opens[1].1);
    assert_eq!(rig.world.seqs(2), vec![1, 2, 3]);
}

#[tokio::test]
async fn a_hole_in_the_outbox_stops_the_shipper_and_raises_journal_gap() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 3);
    let mut shipper = rig.shipper(true, 10);
    rig.drain(&mut shipper).await;
    // Entries 4..=6 were pruned before they were shipped (no barrier): 7 follows 3.
    rig.outbox.push_range(1, 7, 8);
    let step = rig.step(&mut shipper).await;
    assert_eq!(
        step,
        Step::Stopped {
            code: "journal_ship.outbox_gap"
        }
    );
    assert!(rig.alarms.is_open(AlarmKind::JournalGap));
    assert_eq!(
        rig.world.seqs(1),
        vec![1, 2, 3],
        "nothing is written on top of the hole"
    );
    assert_eq!(
        rig.step(&mut shipper).await,
        Step::Stopped {
            code: "journal_ship.outbox_gap"
        }
    );
}

#[derive(Default)]
struct FlakyProtector {
    fail: AtomicBool,
}

impl Protector for FlakyProtector {
    fn protect(&self, line: &[u8]) -> Result<Vec<u8>, ShipFault> {
        if self.fail.load(Ordering::SeqCst) {
            Err(ShipFault::Protect {
                code: "journal_protect.protect_failed",
            })
        } else {
            Ok(line.to_vec())
        }
    }
}

struct Marking;

impl Protector for Marking {
    fn protect(&self, line: &[u8]) -> Result<Vec<u8>, ShipFault> {
        let mut out = b"P:".to_vec();
        out.extend_from_slice(line);
        Ok(out)
    }
}

#[tokio::test]
async fn every_line_goes_through_the_protector_before_the_relay_sees_it() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 3);
    let mut shipper = rig.shipper_with(true, 10, Arc::new(Marking), None);
    rig.drain(&mut shipper).await;
    let held = rig.world.held.lock().unwrap().clone();
    assert_eq!(held.len(), 3);
    assert!(held.iter().all(|(_, _, line)| line.starts_with(b"P:")));
}

struct Verdict(Mutex<ChainState>);

impl ChainChecker for Verdict {
    fn check(&self, _generation: u64) -> PortFuture<'_, Result<ChainState, ShipFault>> {
        Box::pin(async move { Ok(*self.0.lock().unwrap()) })
    }
}

#[tokio::test]
async fn a_damaged_chain_stops_the_watermark_and_raises_journal_gap() {
    let mut rig = Rig::new();
    rig.world.seal_all.store(true, Ordering::SeqCst);
    let verdict = Arc::new(Verdict(Mutex::new(ChainState::Clean)));
    rig.outbox.push_range(1, 1, 3);
    let mut shipper = rig.shipper_with(true, 10, Arc::new(PassThrough), Some(verdict.clone()));
    rig.drain(&mut shipper).await;
    assert_eq!(rig.mark(), (1, 3));
    *verdict.0.lock().unwrap() = ChainState::Damaged {
        code: "journal_verify.hash_mismatch",
    };
    rig.outbox.push_range(1, 4, 6);
    // The next verification (due by time) fails before the batch is written.
    let step = rig.step(&mut shipper).await;
    assert_eq!(
        step,
        Step::Stopped {
            code: "journal_verify.hash_mismatch"
        }
    );
    assert!(rig.alarms.is_open(AlarmKind::JournalGap));
    assert_eq!(rig.world.seqs(1), vec![1, 2, 3]);
    assert_eq!(
        rig.mark(),
        (1, 3),
        "the watermark does not move past the damage"
    );
    assert!(matches!(rig.step(&mut shipper).await, Step::Stopped { .. }));
}

#[tokio::test]
async fn a_long_outage_raises_rpo_breach_once_and_recovery_clears_it_and_reships() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 2);
    let mut shipper = rig.shipper(true, 10);
    rig.drain(&mut shipper).await;
    rig.outbox.push_range(1, 3, 6);
    for _ in 0..8 {
        rig.world.fail_before.store(true, Ordering::SeqCst);
        assert!(matches!(
            rig.step(&mut shipper).await,
            Step::Degraded { .. }
        ));
    }
    let raises = rig
        .alarms
        .events()
        .iter()
        .filter(|(_, kind, raised)| *kind == AlarmKind::RpoBreach && *raised)
        .count();
    assert_eq!(raises, 1, "raised once, after the grace");
    assert!(rig.alarms.is_open(AlarmKind::RpoBreach));
    rig.drain(&mut shipper).await;
    assert!(
        !rig.alarms.is_open(AlarmKind::RpoBreach),
        "cleared on recovery"
    );
    assert_eq!(
        rig.world.seqs(1),
        vec![1, 2, 3, 4, 5, 6],
        "re-shipped, nothing dropped"
    );
}

#[tokio::test]
async fn the_progress_handle_reports_the_durable_position_and_can_be_waited_on() {
    let mut rig = Rig::new();
    rig.outbox.push_range(1, 1, 3);
    let mut cfg = ShipperConfig::new(SOURCE, "remote-sync", true);
    cfg.batch_max_entries = 10;
    let (mut shipper, handle) = Shipper::new(
        cfg,
        Ports {
            outbox: rig.outbox.clone(),
            marks: rig.marks.clone(),
            lease: rig.lease.clone(),
            protector: Arc::new(PassThrough),
            alarms: rig.alarms.clone(),
            factory: FakeFactory {
                world: Arc::clone(&rig.world),
                remote: true,
            },
            checker: None,
            backup: None,
        },
    )
    .unwrap();
    let target = Position {
        generation: 1,
        seq: 3,
    };
    assert!(!handle.wait_durable(target, Duration::from_millis(20)).await);
    rig.step(&mut shipper).await;
    assert_eq!(handle.durable(), target);
    assert!(handle.wait_durable(target, Duration::from_millis(20)).await);
    let text = handle.now().readiness("remote-sync");
    assert!(text.starts_with("journal: remote-sync ok"), "{text}");
    assert!(text.contains("g1:s3"), "{text}");
    rig.lease.set_held(false);
    rig.step(&mut shipper).await;
    assert!(handle.now().readiness("remote-sync").contains("standby"));
}

#[tokio::test]
async fn backoff_doubles_and_stops_at_thirty_seconds() {
    let base = Duration::from_millis(250);
    assert_eq!(Shipper::<FakeFactory>::backoff(base, 1), base);
    assert_eq!(Shipper::<FakeFactory>::backoff(base, 2), base * 2);
    assert_eq!(
        Shipper::<FakeFactory>::backoff(base, 40),
        Duration::from_secs(30)
    );
}
