//! The simulator's discrete-event engine.
//!
//! One [`Sim`] owns the seeded generator, a priority queue of events keyed by `(tick, order)` so
//! equal ticks resolve in scheduling order, the producers, the hub and the edges. Every protocol
//! step (a handshake, a session step, a delivery, a crash, a produce tick, a chaos tick) is one
//! event, so a run is a pure function of its [`SimConfig`] and its seed.
//!
//! Fault injection has three faces, each at the point real life would break:
//!
//! * the **network** drops, duplicates, reorders (jittered latency) and partitions;
//! * a **crash** of either role resets only volatile state (cursors, sessions, the checkpoint
//!   cache); the durable apply sink and checkpoint store survive, exactly as a restarted process
//!   reloads them;
//! * a **checkpoint store failure** falls between a durable apply and its checkpoint, the two-phase
//!   window the idempotency key exists to absorb.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::time::Duration;

use spot_events_contract::SourceId;

mod steps;

use super::{Direction, Rng, SimConfig, SimError, SimReport};
use crate::error::{Error, Result as SyncResult};
use crate::ids::{NodeId, ProtocolRange, ProtocolVersion};
use crate::message::{Ack, Batch, Checkpoint, Hello, Nack};
use crate::receiver::{Receiver, ReceiverConfig};
use crate::record::{Record, RecordName};
use crate::resilience::{Deadline, RetryPolicy};
use crate::sender::{Sender, SenderConfig};
use crate::store::{CheckpointStore, MemoryCheckpoints, MemorySink};

/// Runs `config` plus a range of seeds, stopping at the first failure.
pub fn run_seeds(config: &SimConfig, seeds: impl IntoIterator<Item = u64>) -> Result<(), SimError> {
    for seed in seeds {
        let mut config = config.clone();
        config.seed = seed;
        run(&config)?;
    }
    Ok(())
}

/// One endpoint in the simulated network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Endpoint {
    /// The hub.
    Hub,
    /// An edge, by index.
    Edge(usize),
}

/// A message on the wire.
#[derive(Clone, Debug)]
enum Msg {
    /// A handshake from an edge.
    Hello {
        /// The sender.
        from: Endpoint,
        /// The handshake.
        hello: Hello,
    },
    /// The negotiated protocol version.
    HelloReply {
        /// The version both peers share.
        version: ProtocolVersion,
    },
    /// No common version.
    HelloNack,
    /// A pull request (edge to hub).
    Pull {
        /// The requester.
        from: Endpoint,
        /// The source index.
        source: usize,
        /// Where the edge will resume.
        from_seq: u64,
    },
    /// A batch (hub to edge).
    Batch {
        /// The source index.
        source: usize,
        /// The batch.
        batch: Batch,
    },
    /// No data now (hub to edge), with the long-poll hint implied by the edge's next step.
    Empty {
        /// The source index.
        source: usize,
    },
    /// A pull acknowledgement (edge to hub).
    Ack {
        /// The acknowledging edge.
        edge: usize,
        /// The acknowledgement.
        ack: Ack,
    },
    /// A pull refusal (edge to hub).
    Nack {
        /// The refusing edge.
        edge: usize,
        /// The refusal.
        nack: Nack,
    },
    /// A push (edge to hub).
    Push {
        /// The sending edge.
        edge: usize,
        /// The source index.
        source: usize,
        /// The batch.
        batch: Batch,
    },
    /// A push acknowledgement (hub to edge).
    PushAck {
        /// The acknowledgement.
        ack: Ack,
    },
    /// A push refusal (hub to edge).
    PushNack {
        /// The refusal.
        nack: Nack,
    },
}

/// What an event does.
#[derive(Debug)]
enum EvKind {
    /// Produce the next record.
    Produce,
    /// Draw a network chaos decision.
    Chaos,
    /// One handshake attempt for an edge.
    Handshake { edge: usize, generation: u64 },
    /// One pull session step.
    PullStep {
        edge: usize,
        source: usize,
        generation: u64,
    },
    /// One push session step.
    PushStep {
        edge: usize,
        source: usize,
        generation: u64,
    },
    /// Deliver a message.
    Deliver { to: Endpoint, msg: Msg },
}

/// One scheduled event, ordered by `(tick, order)`.
#[derive(Debug)]
struct Ev {
    at: u64,
    order: u64,
    kind: EvKind,
}

impl PartialEq for Ev {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.order == other.order
    }
}

impl Eq for Ev {}

impl PartialOrd for Ev {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ev {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.at, self.order).cmp(&(other.at, other.order))
    }
}

/// A checkpoint store that fails one write in every `fail_every`, modelling a crash between a
/// durable apply and its checkpoint. `load` always works, so the receiver reloads the true
/// checkpoint and re-applies (idempotently) before trying again.
#[derive(Debug)]
struct FlakyStore {
    inner: MemoryCheckpoints,
    fail_every: u64,
    calls: u64,
}

impl FlakyStore {
    fn new(fail_every: u64) -> Self {
        Self {
            inner: MemoryCheckpoints::new(),
            fail_every,
            calls: 0,
        }
    }
}

impl CheckpointStore for FlakyStore {
    fn load(&self, source: &SourceId) -> Option<Checkpoint> {
        self.inner.load(source)
    }

    fn store(&mut self, checkpoint: &Checkpoint) -> SyncResult<()> {
        self.calls = self.calls.wrapping_add(1);
        if self.fail_every > 1 && self.calls.is_multiple_of(self.fail_every) {
            return Err(Error::store_failed("injected checkpoint store failure"));
        }
        self.inner.store(checkpoint)
    }
}

/// One edge's state.
#[derive(Debug)]
struct EdgeState {
    node: NodeId,
    /// The global index of this edge's first source (push).
    base: usize,
    negotiated: bool,
    handshake_gen: u64,
    handshake_awaiting: Option<Deadline>,
    receiver: Receiver<MemorySink, FlakyStore>,
    pull_gen: Vec<u64>,
    pull_awaiting: Vec<Option<Deadline>>,
    sender: Sender,
    push_gen: Vec<u64>,
    push_awaiting: Vec<Option<(Batch, Deadline)>>,
    outbox: Vec<Vec<Record>>,
}

/// The whole simulation.
struct Sim {
    config: SimConfig,
    rng: Rng,
    clock: u64,
    order: u64,
    queue: BinaryHeap<Reverse<Ev>>,
    all_sources: Vec<SourceId>,
    index_of: BTreeMap<SourceId, usize>,
    per_source_len: Vec<u64>,
    plan: Vec<(usize, Record)>,
    produced: usize,
    total: usize,
    hub_streams: Vec<Vec<Record>>,
    hub_senders: Vec<Sender>,
    hub_receiver: Option<Receiver<MemorySink, FlakyStore>>,
    hub_hello: Hello,
    edges: Vec<EdgeState>,
    partitioned_until: Vec<u64>,
    sent: u64,
    dropped: u64,
    retry: RetryPolicy,
}

impl Sim {
    fn new(config: &SimConfig) -> Result<Self, SimError> {
        let fail = |reason: String| SimError {
            seed: config.seed,
            reason,
        };
        if config.sources == 0 {
            return Err(fail("sources must be at least 1".to_owned()));
        }
        if config.edges == 0 {
            return Err(fail("edges must be at least 1".to_owned()));
        }
        if config.records == 0 {
            return Err(fail("records must be at least 1".to_owned()));
        }
        if config.versions_per_name == 0 {
            return Err(fail("versions_per_name must be at least 1".to_owned()));
        }
        if config.max_ticks == 0 {
            return Err(fail("max_ticks must be at least 1".to_owned()));
        }
        let sender_config = SenderConfig::new(config.max_batch, config.window)
            .map_err(|error| fail(error.to_string()))?;
        let receiver_config = ReceiverConfig::new(config.max_batch as u64)
            .map_err(|error| fail(error.to_string()))?;
        let retry_ms = config.retry_every.max(1);
        let retry = RetryPolicy::new(
            2,
            Duration::from_millis(retry_ms),
            1,
            Duration::from_millis(retry_ms),
        )
        .map_err(|error| fail(error.to_string()))?;

        let mut all_sources = Vec::new();
        match config.direction {
            Direction::Pull => {
                for i in 0..config.sources {
                    all_sources.push(
                        SourceId::new(&format!("{}#src_{i}", config.site))
                            .map_err(|error| fail(error.to_string()))?,
                    );
                }
            }
            Direction::Push => {
                for e in 0..config.edges {
                    for i in 0..config.sources {
                        all_sources.push(
                            SourceId::new(&format!("{}#edge_{e}_{i}", config.site))
                                .map_err(|error| fail(error.to_string()))?,
                        );
                    }
                }
            }
        }
        let mut index_of = BTreeMap::new();
        for (index, source) in all_sources.iter().enumerate() {
            index_of.insert(source.clone(), index);
        }
        let source_count = all_sources.len();
        let n = all_sources.len();
        let base = config.records / n;
        let remainder = config.records % n;
        let per_source_len: Vec<u64> = (0..n)
            .map(|index| if index < remainder { base + 1 } else { base } as u64)
            .collect();

        let longest = per_source_len.iter().copied().max().unwrap_or(0);
        let mut plan: Vec<(usize, Record)> = Vec::with_capacity(config.records);
        for seq in 1..=longest {
            for (index, &len) in per_source_len.iter().enumerate() {
                if seq > len {
                    continue;
                }
                let versions = config.versions_per_name;
                let name_index = (seq - 1) / versions;
                let version = (seq - 1) % versions + 1;
                let name = RecordName::new(&format!("src{index}_doc{name_index}"))
                    .map_err(|error| fail(error.to_string()))?;
                let record = Record::new(name, version, seq, format!("v{seq}"))
                    .map_err(|error| fail(error.to_string()))?;
                plan.push((index, record));
            }
        }
        let total = plan.len();

        let (hub_streams, hub_senders, hub_receiver) = match config.direction {
            Direction::Pull => {
                let streams = vec![Vec::new(); n];
                let mut senders = Vec::with_capacity(config.edges);
                for _ in 0..config.edges {
                    senders.push(Sender::new(sender_config).map_err(|e| fail(e.to_string()))?);
                }
                (streams, senders, None)
            }
            Direction::Push => {
                let receiver = Receiver::new(
                    receiver_config,
                    MemorySink::new(),
                    FlakyStore::new(config.store_fail_every),
                )
                .map_err(|e| fail(e.to_string()))?;
                (Vec::new(), Vec::new(), Some(receiver))
            }
        };

        let mut edges = Vec::with_capacity(config.edges);
        for e in 0..config.edges {
            let node =
                NodeId::new(&format!("edge-{e}")).map_err(|error| fail(error.to_string()))?;
            let receiver = Receiver::new(
                receiver_config,
                MemorySink::new(),
                FlakyStore::new(config.store_fail_every),
            )
            .map_err(|error| fail(error.to_string()))?;
            let sender = Sender::new(sender_config).map_err(|error| fail(error.to_string()))?;
            let outbox = match config.direction {
                Direction::Push => vec![Vec::new(); config.sources],
                Direction::Pull => Vec::new(),
            };
            edges.push(EdgeState {
                node,
                base: e * config.sources,
                negotiated: false,
                handshake_gen: 0,
                handshake_awaiting: None,
                receiver,
                pull_gen: vec![0; source_count],
                pull_awaiting: vec![None; source_count],
                sender,
                push_gen: vec![0; source_count],
                push_awaiting: vec![None; source_count],
                outbox,
            });
        }

        let hub_hello = Hello::new(
            NodeId::new(&config.site).map_err(|error| fail(error.to_string()))?,
            ProtocolRange::current(),
            0,
        );

        let mut sim = Self {
            config: config.clone(),
            rng: Rng::new(config.seed),
            clock: 0,
            order: 0,
            queue: BinaryHeap::new(),
            all_sources,
            index_of,
            per_source_len,
            plan,
            produced: 0,
            total,
            hub_streams,
            hub_senders,
            hub_receiver,
            hub_hello,
            edges,
            partitioned_until: vec![0; config.edges],
            sent: 0,
            dropped: 0,
            retry,
        };
        sim.schedule(0, EvKind::Produce);
        for e in 0..sim.config.edges {
            sim.schedule(
                1 + e as u64,
                EvKind::Handshake {
                    edge: e,
                    generation: 0,
                },
            );
        }
        if sim.config.network.chaos_every > 0 {
            let at = sim.config.network.chaos_every;
            sim.schedule(at, EvKind::Chaos);
        }
        Ok(sim)
    }

    fn now(&self) -> Duration {
        Duration::from_millis(self.clock)
    }

    fn ticks_until(at: Duration) -> u64 {
        u64::try_from(at.as_millis()).unwrap_or(u64::MAX)
    }

    fn schedule(&mut self, at: u64, kind: EvKind) {
        let order = self.order;
        self.order = self.order.wrapping_add(1);
        self.queue.push(Reverse(Ev { at, order, kind }));
    }

    fn blocked(&self, from: Endpoint, to: Endpoint) -> bool {
        match (from, to) {
            (Endpoint::Edge(e), Endpoint::Hub) | (Endpoint::Hub, Endpoint::Edge(e)) => {
                self.partitioned_until.get(e).copied().unwrap_or(0) > self.clock
            }
            _ => false,
        }
    }

    fn transmit(&mut self, from: Endpoint, to: Endpoint, msg: Msg) {
        if self.blocked(from, to) {
            self.dropped += 1;
            return;
        }
        if self.rng.chance(self.config.network.drop_permille) {
            self.dropped += 1;
            return;
        }
        self.sent += 1;
        let jitter = self.config.network.jitter.max(1);
        let delay = self
            .config
            .network
            .latency
            .saturating_add(self.rng.below(jitter));
        // Draw the duplication decision before moving `msg`, so the common (not duplicated) path
        // costs no clone.
        let duplicate = self.rng.chance(self.config.network.duplicate_permille);
        let copy = duplicate.then(|| msg.clone());
        self.schedule(self.clock + delay, EvKind::Deliver { to, msg });
        if let Some(copy) = copy {
            let extra = delay.saturating_add(self.rng.below(jitter.saturating_add(1)));
            self.schedule(self.clock + extra, EvKind::Deliver { to, msg: copy });
        }
    }

    /// A deadline one jittered retry interval from now.
    fn retry_deadline(&mut self) -> Deadline {
        let jitter = 500 + self.rng.below(501);
        let delay = self.retry.delay(1, jitter);
        Deadline::new(self.now(), delay)
    }

    fn crash(&mut self, role: Endpoint) -> bool {
        if !self.rng.chance(self.config.crash_permille) {
            return false;
        }
        match role {
            Endpoint::Edge(e) => self.crash_edge(e),
            Endpoint::Hub => self.crash_hub(),
        }
        true
    }

    fn crash_edge(&mut self, e: usize) {
        let generation = {
            let edge = &mut self.edges[e];
            edge.receiver.crash();
            edge.sender.crash();
            edge.negotiated = false;
            edge.handshake_awaiting = None;
            edge.pull_awaiting.fill(None);
            edge.push_awaiting.fill(None);
            edge.handshake_gen = edge.handshake_gen.wrapping_add(1);
            edge.handshake_gen
        };
        self.schedule(
            self.clock + 1,
            EvKind::Handshake {
                edge: e,
                generation,
            },
        );
    }

    fn crash_hub(&mut self) {
        for sender in &mut self.hub_senders {
            sender.crash();
        }
        if let Some(receiver) = &mut self.hub_receiver {
            receiver.crash();
        }
    }

    fn schedule_pull(&mut self, edge: usize, source: usize, at: u64) {
        let generation = {
            let state = &mut self.edges[edge];
            state.pull_gen[source] = state.pull_gen[source].wrapping_add(1);
            state.pull_gen[source]
        };
        self.schedule(
            at,
            EvKind::PullStep {
                edge,
                source,
                generation,
            },
        );
    }

    fn schedule_push(&mut self, edge: usize, source: usize, at: u64) {
        let generation = {
            let state = &mut self.edges[edge];
            state.push_gen[source] = state.push_gen[source].wrapping_add(1);
            state.push_gen[source]
        };
        self.schedule(
            at,
            EvKind::PushStep {
                edge,
                source,
                generation,
            },
        );
    }

    fn schedule_steps(&mut self, edge: usize) {
        let at = self.clock + 1;
        match self.config.direction {
            Direction::Pull => {
                for source in 0..self.config.sources {
                    self.schedule_pull(edge, source, at);
                }
            }
            Direction::Push => {
                // Push steps are indexed by the global source, so an edge's local streams offset
                // by its base.
                let base = self.edges[edge].base;
                for local in 0..self.config.sources {
                    self.schedule_push(edge, base + local, at);
                }
            }
        }
    }
}
/// Runs one simulation and checks its invariant.
pub fn run(config: &SimConfig) -> Result<SimReport, SimError> {
    let mut sim = Sim::new(config)?;
    while let Some(event) = sim.queue.pop() {
        sim.clock = event.0.at;
        if sim.clock > sim.config.max_ticks {
            break;
        }
        sim.dispatch(event.0.kind);
        if sim.done() {
            break;
        }
    }
    if !sim.done() {
        return Err(SimError {
            seed: config.seed,
            reason: format!(
                "did not converge within {} ticks: produced {}/{} records, applied {:?}",
                config.max_ticks,
                sim.produced,
                sim.total,
                sim.applied()
            ),
        });
    }
    sim.verify().map_err(|reason| SimError {
        seed: config.seed,
        reason,
    })?;
    Ok(SimReport {
        seed: config.seed,
        direction: config.direction,
        produced: sim.produced,
        applied: sim.applied(),
        ticks: sim.clock,
        sent: sim.sent,
        dropped: sim.dropped,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::sim::{Direction, NetworkConfig, SimConfig};

    #[test]
    fn a_faulty_run_still_converges_once() {
        let config = SimConfig {
            seed: 42,
            records: 200,
            ..SimConfig::default()
        };
        let report = run(&config).unwrap();
        assert_eq!(report.produced, 200);
        assert!(report.applied.iter().all(|&n| n == 200));
        assert!(
            report.dropped > 0,
            "the network should have dropped something"
        );
    }

    #[test]
    fn push_converges_too() {
        let config = SimConfig {
            direction: Direction::Push,
            seed: 7,
            records: 120,
            network: NetworkConfig::default(),
            ..SimConfig::default()
        };
        let report = run(&config).unwrap();
        assert_eq!(report.applied, vec![120]);
    }
}
