//! Test doubles: a virtual clock, a relay that fails on demand and a recording observer.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use spot_journal_durability::{AckGate, GateConfig, OnUnreachable, Policy, TenantDurability};
use tokio::sync::Notify;

use crate::{
    ChunkRelay, CommitAnswer, CommitDriver, CommitRequest, CommitSettings, Observer, Runtime,
    ShipEntry, ShipError, Shipped,
};

#[derive(Default)]
pub(crate) struct Clock {
    now: AtomicU64,
    changed: Notify,
}

impl Clock {
    pub(crate) fn advance(&self, millis: u64) {
        self.now.fetch_add(millis, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}

#[derive(Clone, Default)]
pub(crate) struct VirtualClock(pub(crate) Arc<Clock>);

impl Runtime for VirtualClock {
    fn now_millis(&self) -> u64 {
        self.0.now.load(Ordering::SeqCst)
    }

    fn sleep_until(&self, deadline_millis: u64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            loop {
                let mut changed = std::pin::pin!(self.0.changed.notified());
                changed.as_mut().enable();
                if self.now_millis() >= deadline_millis {
                    return;
                }
                changed.await;
            }
        })
    }
}

#[derive(Default)]
pub(crate) struct RelayState {
    /// Every attempt: (batch id, first seq, last seq).
    pub(crate) attempts: Vec<(u64, u64, u64)>,
    /// Sequence numbers held durably.
    pub(crate) held: BTreeSet<u64>,
    pub(crate) next_seq: u64,
    /// The store is down: every call fails.
    pub(crate) down: bool,
    /// The next failing ship writes the chunk anyway (an ambiguous failure).
    pub(crate) write_despite_failure: bool,
    /// A failure no retry cures.
    pub(crate) fatal: bool,
    pub(crate) buffered_only: bool,
}

impl RelayState {
    pub(crate) fn appends(&self) -> usize {
        self.attempts.len()
    }
}

#[derive(Clone)]
pub(crate) struct FakeRelay(pub(crate) Arc<Mutex<RelayState>>);

impl FakeRelay {
    pub(crate) fn new(first_seq: u64) -> Self {
        Self(Arc::new(Mutex::new(RelayState {
            next_seq: first_seq,
            ..RelayState::default()
        })))
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, RelayState> {
        self.0.lock().unwrap()
    }
}

impl ChunkRelay for FakeRelay {
    fn generation(&self) -> u64 {
        1
    }

    fn next_seq(&self) -> u64 {
        self.state().next_seq
    }

    async fn ship(
        &mut self,
        batch_id: u64,
        entries: &[ShipEntry],
        _now_millis: u64,
    ) -> Result<Shipped, ShipError> {
        let mut s = self.state();
        let first = entries.first().map_or(0, |e| e.seq);
        let last = entries.last().map_or(0, |e| e.seq);
        s.attempts.push((batch_id, first, last));
        if s.fatal {
            return Err(ShipError::Stopped {
                code: "journal_relay.split_brain",
            });
        }
        if s.down {
            if s.write_despite_failure {
                s.write_despite_failure = false;
                for e in entries {
                    s.held.insert(e.seq);
                }
                s.next_seq = last + 1;
            }
            return Err(ShipError::StoreDown {
                code: "journal_relay.store_failed",
            });
        }
        assert_eq!(first, s.next_seq, "the driver shipped a gap or a repeat");
        for e in entries {
            s.held.insert(e.seq);
        }
        s.next_seq = last + 1;
        Ok(if s.buffered_only {
            Shipped::Buffered
        } else {
            Shipped::Durable { up_to_seq: last }
        })
    }

    async fn resume(&mut self) -> Result<(), ShipError> {
        if self.state().down {
            return Err(ShipError::StoreDown {
                code: "journal_relay.store_failed",
            });
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Recorder(pub(crate) Mutex<Vec<(u64, CommitAnswer)>>);

impl Observer for Recorder {
    fn resolved(&self, seq: u64, answer: &CommitAnswer) {
        self.0.lock().unwrap().push((seq, *answer));
    }
}

impl Recorder {
    pub(crate) fn acked(&self) -> Vec<u64> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, a)| match a {
                CommitAnswer::Acked(seq) => Some(*seq),
                _ => None,
            })
            .collect()
    }
}

pub(crate) type Driver = CommitDriver<FakeRelay, VirtualClock>;

pub(crate) struct Rig {
    pub(crate) driver: Arc<Driver>,
    pub(crate) relay: FakeRelay,
    pub(crate) clock: VirtualClock,
    pub(crate) recorder: Arc<Recorder>,
}

pub(crate) const REMOTE_DEGRADE: Policy = Policy::RemoteSync {
    on_unreachable: OnUnreachable::Degrade,
};
pub(crate) const REMOTE_HALT: Policy = Policy::RemoteSync {
    on_unreachable: OnUnreachable::Halt,
};

pub(crate) fn rig_with(policy: Policy, settings: CommitSettings, gate: GateConfig) -> Rig {
    let relay = FakeRelay::new(1);
    let clock = VirtualClock::default();
    let recorder = Arc::new(Recorder::default());
    let tenant = TenantDurability {
        policy,
        journal_target_configured: true,
    };
    let driver = CommitDriver::new(
        AckGate::new(tenant, gate),
        relay.clone(),
        clock.clone(),
        settings,
    )
    .with_observer(recorder.clone());
    Rig {
        driver: Arc::new(driver),
        relay,
        clock,
        recorder,
    }
}

pub(crate) fn rig(policy: Policy) -> Rig {
    rig_with(policy, CommitSettings::default(), GateConfig::default())
}

pub(crate) fn request(seq: u64) -> CommitRequest {
    CommitRequest {
        command_id: format!("cmd-{seq}"),
        seq,
        entry: format!("{{\"seq\":{seq}}}").into_bytes(),
    }
}

impl Rig {
    /// Submits `seq` and confirms its local commit.
    pub(crate) fn commit(&self, seq: u64) -> crate::CommitTicket {
        let ticket = self.driver.submit(request(seq)).unwrap();
        self.driver.local_commit_durable(seq).unwrap();
        ticket
    }

    /// Moves the clock past the group window and runs the driver once.
    pub(crate) async fn flush(&self) {
        self.clock.0.advance(10);
        self.driver.step().await;
    }
}
