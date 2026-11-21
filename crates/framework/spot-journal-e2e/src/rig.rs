//! The commit path: a relay over the test store, shared so a scenario can seal it, behind the
//! commit driver and the durability gate, with a recording observer.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use spot_journal_commit::{
    ChunkRelay, CommitAnswer, CommitDriver, CommitRequest, CommitSettings, CommitTicket, Observer,
    Runtime, ShipEntry, ShipError, Shipped,
};
use spot_journal_durability::{AckGate, GateConfig, OnUnreachable, Policy, TenantDurability};
use spot_journal_relay::{Durability, Relay, RelayConfig, TailKeys};
use spot_journal_segment::SealLimits;

use crate::events::{SOURCE, TENANT};
use crate::keys::{Signer, manifest_keys};
use crate::store::{EventLog, TestStore};

/// A relay both the driver and the scenario can reach (never at the same time).
#[derive(Clone)]
pub struct SharedRelay(pub Arc<tokio::sync::Mutex<Relay<TestStore>>>);

impl ChunkRelay for SharedRelay {
    fn generation(&self) -> u64 {
        self.0.try_lock().map_or(1, |relay| relay.generation())
    }

    fn next_seq(&self) -> u64 {
        self.0
            .try_lock()
            .map_or(1, |relay| relay.next_position().seq)
    }

    async fn ship(
        &mut self,
        batch_id: u64,
        entries: &[ShipEntry],
        now_millis: u64,
    ) -> Result<Shipped, ShipError> {
        let mut relay = self.0.lock().await;
        ChunkRelay::ship(&mut *relay, batch_id, entries, now_millis).await
    }

    async fn resume(&mut self) -> Result<(), ShipError> {
        let mut relay = self.0.lock().await;
        ChunkRelay::resume(&mut *relay).await
    }
}

/// A virtual clock; `sleep_until` is never awaited because scenarios call `step` themselves.
#[derive(Clone, Default)]
pub struct Clock(pub Arc<AtomicU64>);

impl Clock {
    pub fn advance(&self, millis: u64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Runtime for Clock {
    fn now_millis(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    fn sleep_until(&self, _deadline_millis: u64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// Writes `ack <seq>` into the event log when a commit is acknowledged, and keeps every answer.
pub struct Recorder {
    pub log: EventLog,
    pub answers: Mutex<Vec<(u64, CommitAnswer)>>,
}

impl Observer for Recorder {
    fn resolved(&self, seq: u64, answer: &CommitAnswer) {
        if matches!(answer, CommitAnswer::Acked(_)) {
            self.log.lock().unwrap().push(format!("ack {seq}"));
        }
        self.answers.lock().unwrap().push((seq, *answer));
    }
}

pub type Driver = CommitDriver<SharedRelay, Clock>;

pub struct Rig {
    pub driver: Driver,
    pub relay: SharedRelay,
    pub clock: Clock,
    pub store: TestStore,
    pub log: EventLog,
    pub recorder: Arc<Recorder>,
}

/// The test tenant's journal key: a fixed KEK, so a reopened relay reads what the first wrote.
pub fn tail_keys() -> TailKeys {
    let provider =
        spot_secrets::EnvKey::from_base64("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
            .expect("a 32 byte test key");
    TailKeys::new(Arc::new(provider))
}

/// The relay configuration: remote-sync, sealing every ~40 KiB so a run makes several segments.
pub fn relay_config(tenant: &str, source: &str) -> RelayConfig {
    let mut config =
        RelayConfig::new(tenant, source, Durability::RemoteSync).with_tail_keys(tail_keys());
    config.limits = SealLimits {
        max_bytes: 40 * 1024,
        max_age_millis: u64::MAX,
    };
    config
}

impl Rig {
    /// Opens (or resumes) generation 1 of `TENANT` on `store` and builds a driver at the relay's
    /// next position.
    pub async fn open(store: TestStore, log: EventLog) -> Rig {
        let relay = Relay::open(
            store.clone(),
            Box::new(Signer),
            manifest_keys(),
            relay_config(TENANT, SOURCE),
            1_000,
        )
        .await
        .unwrap();
        let relay = SharedRelay(Arc::new(tokio::sync::Mutex::new(relay)));
        let clock = Clock::default();
        let recorder = Arc::new(Recorder {
            log: log.clone(),
            answers: Mutex::new(Vec::new()),
        });
        let gate = AckGate::new(
            TenantDurability {
                policy: Policy::RemoteSync {
                    on_unreachable: OnUnreachable::Halt,
                },
                journal_target_configured: true,
            },
            GateConfig::default(),
        );
        let driver = CommitDriver::new(
            gate,
            relay.clone(),
            clock.clone(),
            CommitSettings::default(),
        )
        .unwrap()
        .with_observer(recorder.clone());
        Rig {
            driver,
            relay,
            clock,
            store,
            log,
            recorder,
        }
    }

    /// The sequence number the next commit must take.
    pub async fn next_seq(&self) -> u64 {
        self.relay.0.lock().await.next_position().seq
    }

    /// Submits `line` as commit `seq` and confirms its local database commit.
    pub fn commit(&self, seq: u64, line: Vec<u8>) -> CommitTicket {
        let ticket = self
            .driver
            .submit(CommitRequest {
                command_id: format!("cmd-{seq}"),
                seq,
                entry: line,
            })
            .unwrap();
        self.driver.local_commit_durable(seq).unwrap();
        ticket
    }

    /// Closes the group window and runs the driver once.
    pub async fn flush(&self) {
        self.clock.advance(10);
        self.driver.step().await;
    }

    /// Seals the open segment (the relay is not otherwise reachable while the driver runs).
    pub async fn seal(&self) -> bool {
        self.relay.0.lock().await.seal_now(9_000).await.unwrap()
    }
}
