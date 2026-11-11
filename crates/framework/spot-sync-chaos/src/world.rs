//! The World: one hub relay, N nodes, a virtual clock and a wire that loses, duplicates and
//! reorders.

use std::collections::BTreeMap;

use spot_sync_closure::RecordKey;
use spot_sync_hub::{HubRelay, MemoryQueue, MemoryStream, RelayConfig, SendQueue};
use spot_sync_node::{
    DEFAULT_MAX_TENANTS, Delivery, MemoryParking, NodeConfig, NodeIntake, ParkingLimits, Receipt,
};

use crate::catalog::{EntityScope, WorldCatalog, hub_source, references, report};
use crate::clock::Clock;
use crate::model::{
    AckRecord, ChaosError, CutHit, Edge, Frame, NackRecord, RoundReport, Step, WorldConfig,
};
use crate::rng::Rng;
use crate::store::{MemImages, MemStore};

mod query;
mod stream;

/// The hub stream's source name.
pub const HUB_SOURCE: &str = "acme-hub";
/// The one event topic the world's records carry.
pub const TOPIC: &str = concat!("erp", ".", "document", ".", "saved");
/// The schema major of that topic.
pub const MAJOR: u32 = 1;
/// The entities the world's records belong to.
pub const ENTITIES: [&str; 3] = ["SalesInvoice", "Customer", "Item"];

/// One entry of the hub's stream, as the world remembers it.
#[derive(Clone, Debug)]
struct Entry {
    position: u64,
    tenant: String,
    key: RecordKey,
    version: u64,
}

/// One node: its intake, its durable store and its link.
struct Node {
    id: String,
    intake: NodeIntake<MemStore, MemoryParking, WorldCatalog>,
    config: NodeConfig,
    scope: EntityScope,
    generation: u64,
    reported_position: Option<u64>,
    hub_acked: u64,
    suppress_ack: bool,
    pending_nack: Option<(u64, String)>,
    /// Frames of responses still arriving, by offer (volatile session state).
    inbox: BTreeMap<u64, BTreeMap<u32, Frame>>,
    /// The newest response this node applied completely.
    last_offer: u64,
}

impl Node {
    /// The process died and came back: the durable store survives, the volatile checkpoint, the
    /// parking queue and the in-flight acknowledgement do not.
    fn restart(&mut self) -> Result<(), ChaosError> {
        let durable = self.intake.applier().clone();
        self.intake = NodeIntake::new(
            self.config.clone(),
            durable,
            MemoryParking::new(),
            WorldCatalog,
            report(),
        )
        .map_err(|error| ChaosError::new(0, format!("node restart: {error}")))?;
        self.suppress_ack = false;
        self.inbox.clear();
        self.last_offer = 0;
        Ok(())
    }
}

/// What delivering one frame did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivered {
    /// The node applied the bundle (or recognised it as a replay).
    Applied,
    /// The node refused the bundle.
    Refused,
    /// The frame was dropped by a cut before it reached the node.
    Skipped,
}

/// One world: a hub, `nodes` nodes, virtual time and the record stream between them.
pub struct World {
    cfg: WorldConfig,
    clock: Clock,
    rng: Rng,
    hub: HubRelay<MemoryQueue, MemoryStream, MemImages>,
    images: MemImages,
    nodes: Vec<Node>,
    source: spot_events_contract::SourceId,
    /// The records to write, one wave per version.
    plan: Vec<Vec<(RecordKey, u64)>>,
    /// How many waves have been written.
    appended: usize,
    /// Every entry written, in position order.
    entries: Vec<Entry>,
    next_position: u64,
    next_txn: u64,
    next_offer: u64,
    cuts: BTreeMap<(usize, Step), Edge>,
    hits: Vec<CutHit>,
    acks: Vec<AckRecord>,
    nacks: Vec<NackRecord>,
    transcript: Vec<String>,
    round: usize,
}

impl World {
    /// Build a world from its settings.
    ///
    /// # Errors
    /// An unusable setting, an unusable source id, or a relay or intake refusal.
    pub fn build(cfg: WorldConfig) -> Result<Self, ChaosError> {
        if cfg.nodes == 0 {
            return Err(ChaosError::new(cfg.seed, "a world needs at least one node"));
        }
        if cfg.records == 0 || cfg.versions == 0 {
            return Err(ChaosError::new(
                cfg.seed,
                "a world needs at least one record and one version",
            ));
        }
        if cfg.reorder_window == 0 {
            return Err(ChaosError::new(
                cfg.seed,
                "the reorder window is at least one",
            ));
        }
        let seed = cfg.seed;
        let source = hub_source()?;
        let references = Box::new(references());
        let images = MemImages::new();
        let mut relay = HubRelay::new(
            RelayConfig {
                tenant: cfg.tenant.clone(),
                source: HUB_SOURCE.to_owned(),
                max_bundles_per_tick_per_dest: 16,
                read_batch: 256,
                ..RelayConfig::new(cfg.tenant.clone(), HUB_SOURCE)
            },
            MemoryStream::new(),
            MemoryQueue::new(),
            images.clone(),
            references,
        )
        .map_err(|error| ChaosError::new(seed, format!("relay: {error}")))?;

        let node_config = NodeConfig {
            hub_source: source.clone(),
            limits: ParkingLimits::default(),
            max_tenants: DEFAULT_MAX_TENANTS,
        };

        let mut nodes = Vec::with_capacity(cfg.nodes);
        for index in 0..cfg.nodes {
            let scope = match cfg.narrow_node {
                Some(narrow) if narrow == index => {
                    let narrowed: Vec<&str> = cfg
                        .narrow_entities
                        .iter()
                        .map(String::as_str)
                        .filter(|entity| ENTITIES.contains(entity))
                        .collect();
                    EntityScope::new(&narrowed)
                }
                _ => EntityScope::everything(&ENTITIES),
            };
            // Destinations are registered before the stream grows: the relay starts reading at
            // its cursor, so a node registered late would only ever see newer positions.
            relay
                .add_destination(spot_sync_hub::Destination::new(
                    format!("node-{index}"),
                    Box::new(scope.clone()),
                    Box::new(spot_sync_closure::NoRedaction),
                    report(),
                ))
                .map_err(|error| ChaosError::new(seed, format!("destination: {error}")))?;
            let intake = NodeIntake::new(
                node_config.clone(),
                MemStore::new(),
                MemoryParking::new(),
                WorldCatalog,
                report(),
            )
            .map_err(|error| ChaosError::new(seed, format!("intake: {error}")))?;
            nodes.push(Node {
                id: format!("node-{index}"),
                intake,
                config: node_config.clone(),
                scope,
                generation: cfg.generation,
                reported_position: None,
                hub_acked: 0,
                suppress_ack: false,
                pending_nack: None,
                inbox: BTreeMap::new(),
                last_offer: 0,
            });
        }

        let mut world = Self {
            clock: Clock::new(cfg.epoch_ms, cfg.tick_ms),
            rng: Rng::new(cfg.seed),
            hub: relay,
            images,
            nodes,
            source,
            plan: Vec::new(),
            appended: 0,
            entries: Vec::new(),
            next_position: 0,
            next_txn: 0,
            next_offer: 0,
            cuts: BTreeMap::new(),
            hits: Vec::new(),
            acks: Vec::new(),
            nacks: Vec::new(),
            transcript: Vec::new(),
            round: 0,
            cfg,
        };
        world.plan_stream();
        world.append_wave()?;
        if world.cfg.images_run_ahead {
            while world.appended < world.plan.len() {
                world.append_wave()?;
            }
        }
        Ok(world)
    }

    /// A world from a bare seed, with the default settings.
    ///
    /// # Errors
    /// As [`World::build`].
    pub fn from_seed(seed: u64) -> Result<Self, ChaosError> {
        Self::build(WorldConfig::seeded(seed))
    }

    /// Arm a cut: the next `step` on `node` fails on the given edge.
    pub fn cut(&mut self, node: usize, step: Step, edge: Edge) {
        self.cuts.insert((node, step), edge);
    }

    /// Reconnect a node: every armed cut on it is dropped.
    pub fn heal(&mut self, node: usize) {
        self.cuts.retain(|(armed, _), _| *armed != node);
        self.transcript
            .push(format!("{:04} node-{node} reconnect", self.round));
    }

    /// Move a node's clock against the hub's.
    pub fn skew(&mut self, node: usize, skew_ms: i64) {
        self.clock.set_skew(node, skew_ms);
    }

    /// Move a node's clock by `delta_ms` without the hub moving.
    pub fn jump(&mut self, node: usize, delta_ms: i64) {
        self.clock.jump(node, delta_ms);
    }

    /// Make a node's sink refuse the next bundle carrying `key` once.
    ///
    /// # Errors
    /// `UnknownNode` for an index the world does not have.
    pub fn refuse_once(&mut self, node: usize, key: &RecordKey) -> Result<(), ChaosError> {
        let seed = self.cfg.seed;
        let Some(state) = self.nodes.get_mut(node) else {
            return Err(ChaosError::new(seed, format!("no node {node}")));
        };
        state.intake.applier_mut().refuse_next(key.clone());
        Ok(())
    }

    /// The stream generation a node last applied from.
    #[must_use]
    pub fn node_generation(&self, node: usize) -> u64 {
        self.nodes.get(node).map_or(0, |n| n.generation)
    }

    /// The applied position a node reports at the handshake: after a restore, the position of
    /// the backup it was restored from; otherwise its acknowledged prefix.
    #[must_use]
    pub fn node_reported_position(&self, node: usize) -> Option<u64> {
        let state = self.nodes.get(node)?;
        state
            .reported_position
            .or_else(|| state.intake.applied_prefix(&self.cfg.tenant))
    }

    /// A draw from the world's own generator, so seeded behaviour outside a round replays too.
    pub fn draw_below(&mut self, n: u64) -> u64 {
        self.rng.below(n)
    }

    /// Replace a node's durable state: wipe it, or keep what is at or below `keep`.
    pub(crate) fn reset_node(
        &mut self,
        node: usize,
        generation: u64,
        reported: Option<u64>,
        keep: Option<u64>,
    ) -> Result<(), ChaosError> {
        let seed = self.cfg.seed;
        let Some(state) = self.nodes.get_mut(node) else {
            return Err(ChaosError::new(seed, format!("no node {node}")));
        };
        match keep {
            Some(position) => state.intake.applier_mut().truncate_after(position),
            None => state.intake.applier_mut().wipe(),
        }
        state.generation = generation;
        state.reported_position = reported;
        state.hub_acked = 0;
        state.restart()?;
        self.transcript.push(format!(
            "{:04} node-{node} reset generation {generation} reported {reported:?}",
            self.round
        ));
        Ok(())
    }

    /// Install one chunk of a bootstrap snapshot into a node, checking its scope. Returns how
    /// many records were stored.
    pub(crate) fn install_chunk(
        &mut self,
        node: usize,
        chunk: &spot_sync_bootstrap::Chunk,
    ) -> Result<usize, ChaosError> {
        let seed = self.cfg.seed;
        let tenant = self.cfg.tenant.clone();
        let Some(state) = self.nodes.get_mut(node) else {
            return Err(ChaosError::new(seed, format!("no node {node}")));
        };
        let mut stored = 0;
        for bundle in &chunk.bundles {
            for record in &bundle.records {
                if !state.scope.covers_entity(&record.key.entity) {
                    return Err(ChaosError::new(
                        seed,
                        format!("snapshot served an out-of-scope record to node-{node}"),
                    ));
                }
                if state
                    .intake
                    .applier_mut()
                    .install(&tenant, &record.key, record.version)
                {
                    stored += 1;
                }
            }
        }
        Ok(stored)
    }

    /// A node finished a bootstrap: it is on the hub's generation at the hub's head.
    pub(crate) fn note_bootstrapped(&mut self, node: usize) -> Result<(), ChaosError> {
        let head = self.head();
        let generation = self.cfg.generation;
        // Finishing the snapshot is the node's acknowledgement of its position: the hub trims
        // what the queue still holds below it, which the node no longer needs.
        let through = head.min(self.hub.cursor());
        let id = self.node_id(node).to_owned();
        let now = self.clock.hub_wall_ms();
        let seed = self.cfg.seed;
        let already = self.nodes.get(node).map_or(0, |n| n.hub_acked);
        if through > already {
            self.hub
                .on_ack_at(&id, through, now)
                .map_err(|error| ChaosError::new(seed, format!("bootstrap ack {id}: {error}")))?;
        }
        if let Some(state) = self.nodes.get_mut(node) {
            state.generation = generation;
            state.reported_position = Some(head);
            state.hub_acked = state.hub_acked.max(through);
        }
        self.transcript.push(format!(
            "{:04} node-{node} bootstrapped to position {head}",
            self.round
        ));
        Ok(())
    }

    /// Drop a node's records above `position` and mark it as restored from an older backup.
    pub fn restore_node(&mut self, node: usize, position: u64) -> Result<(), ChaosError> {
        let generation = self.cfg.restored_generation;
        self.reset_node(node, generation, Some(position), Some(position))?;
        self.transcript.push(format!(
            "{:04} node-{node} restored to position {position} generation {generation}",
            self.round
        ));
        Ok(())
    }

    /// One round: the stream grows, the hub reads it, every edge takes its turn and the hub
    /// answers. The wire may lose, duplicate or reorder every frame.
    ///
    /// # Errors
    /// A hub or node refusal the world cannot explain, which is a defect the test must see.
    pub fn round(&mut self) -> Result<RoundReport, ChaosError> {
        self.round += 1;
        self.clock.advance();
        // A wave is written once the hub has read the previous one, unless the scenario lets the
        // images run ahead of the stream.
        if self.appended < self.plan.len()
            && (self.cfg.images_run_ahead || self.hub.cursor() == self.head())
        {
            self.append_wave()?;
        }
        let now = self.clock.hub_wall_ms();
        let seed = self.cfg.seed;
        let tick = self
            .hub
            .tick(now)
            .map_err(|error| ChaosError::new(seed, format!("relay tick: {error}")))?;
        self.transcript.push(format!(
            "{:04} hub read {} cursor {}",
            self.round, tick.read, tick.cursor
        ));
        if let Some(code) = tick.failed {
            return Err(ChaosError::new(
                seed,
                format!("the relay refused the stream: {code}"),
            ));
        }

        let mut up: Vec<Frame> = Vec::new();
        for index in 0..self.nodes.len() {
            up.extend(self.edge_frames(index));
        }
        let up_count = up.len();
        let (up, lost_up, dup_up) = self.chaos(up);
        let mut down: Vec<Frame> = Vec::new();
        for frame in up {
            down.extend(self.hub_receives(frame, now)?);
        }
        let down_count = down.len();
        let (down, lost_down, dup_down) = self.chaos(down);
        let mut applied = 0;
        let mut refused = 0;
        for frame in down {
            match self.deliver(frame)? {
                Delivered::Applied => applied += 1,
                Delivered::Refused => refused += 1,
                Delivered::Skipped => {}
            }
        }
        Ok(RoundReport {
            round: self.round,
            up: up_count,
            down: down_count,
            dropped: usize::try_from(lost_up + lost_down).unwrap_or(usize::MAX),
            duplicated: usize::try_from(dup_up + dup_down).unwrap_or(usize::MAX),
            applied,
            refused,
            cursor: self.hub.cursor(),
        })
    }

    /// Run rounds until every node's state equals the hub's, the hub's queue is empty and the
    /// cursor has reached the head. Returns the rounds it took.
    ///
    /// # Errors
    /// A failure from a round, or the world not converging within `max_rounds`.
    pub fn settle(&mut self, max_rounds: usize) -> Result<usize, ChaosError> {
        for round in 0..max_rounds {
            if self.converged() {
                return Ok(round);
            }
            self.round()?;
        }
        if self.converged() {
            return Ok(max_rounds);
        }
        Err(ChaosError::new(
            self.cfg.seed,
            format!(
                "did not converge in {max_rounds} rounds: cursor {} head {}, applied {:?}, queued {:?}",
                self.cursor(),
                self.head(),
                (0..self.nodes.len())
                    .map(|i| self.applied(i).len())
                    .collect::<Vec<_>>(),
                (0..self.nodes.len())
                    .map(|i| self.queued(i))
                    .collect::<Vec<_>>()
            ),
        ))
    }

    /// Whether every node's state equals the hub's for its scope and nothing is in flight.
    #[must_use]
    pub fn converged(&self) -> bool {
        (0..self.nodes.len()).all(|node| self.state_matches(node) && self.queued(node) == 0)
            && self.cursor() == self.head()
    }

    /// Whether one node's state equals the hub's, restricted to its scope.
    #[must_use]
    pub fn state_matches(&self, node: usize) -> bool {
        let Some(state) = self.nodes.get(node) else {
            return false;
        };
        let expected: BTreeMap<RecordKey, u64> = self
            .expected()
            .into_iter()
            .filter(|(key, _)| state.scope.covers_entity(&key.entity))
            .collect();
        state.intake.applier().records_of(&self.cfg.tenant) == expected
    }

    /// The records of another tenant that leaked into a node's store. Empty for a correct world.
    #[must_use]
    pub fn foreign_records(&self, node: usize) -> Vec<(String, RecordKey)> {
        self.nodes.get(node).map_or_else(Vec::new, |state| {
            state
                .intake
                .applier()
                .all_records()
                .keys()
                .filter(|(tenant, _)| tenant != &self.cfg.tenant)
                .cloned()
                .collect()
        })
    }

    /// The positions at or below `prefix`, in the node's tenant and scope, that it does not hold.
    /// This is the evidence behind an acknowledgement: it must be empty.
    #[must_use]
    pub fn missing_through(&self, node: usize, prefix: u64) -> Vec<u64> {
        let Some(state) = self.nodes.get(node) else {
            return Vec::new();
        };
        let store = state.intake.applier();
        let mut missing = Vec::new();
        for entry in &self.entries {
            if entry.position > prefix || entry.tenant != self.cfg.tenant {
                continue;
            }
            if !state.scope.covers_entity(&entry.key.entity) {
                continue;
            }
            if store
                .version_of(&entry.tenant, &entry.key)
                .is_none_or(|version| version < entry.version)
            {
                missing.push(entry.position);
            }
        }
        missing
    }

    /// One edge's turn: acknowledge what it applied, report a refusal, ask for more.
    fn edge_frames(&mut self, node: usize) -> Vec<Frame> {
        let mut out = Vec::new();
        let tenant = self.cfg.tenant.clone();
        let prefix = self.nodes[node].intake.applied_prefix(&tenant);
        let ack_wanted = prefix
            .filter(|prefix| *prefix > self.nodes[node].hub_acked)
            .filter(|_| !self.nodes[node].suppress_ack);
        self.nodes[node].suppress_ack = false;
        if let Some(prefix) = ack_wanted {
            match self.take_cut(node, Step::Ack) {
                Some(edge) => self.note_hit(node, Step::Ack, edge),
                None => out.push(Frame::Ack { node, prefix }),
            }
        }
        if let Some((position, code)) = self.nodes[node].pending_nack.take() {
            match self.take_cut(node, Step::Nack) {
                Some(edge) => self.note_hit(node, Step::Nack, edge),
                None => out.push(Frame::Nack {
                    node,
                    position,
                    code,
                }),
            }
        }
        match self.take_cut(node, Step::Offer) {
            Some(edge) => self.note_hit(node, Step::Offer, edge),
            None => out.push(Frame::Offer { node }),
        }
        out
    }

    /// The hub's turn: answer offers, record acknowledgements and refusals.
    fn hub_receives(&mut self, frame: Frame, now: i64) -> Result<Vec<Frame>, ChaosError> {
        let seed = self.cfg.seed;
        let mut down = Vec::new();
        match frame {
            Frame::Offer { node } => {
                let id = self.node_id(node).to_owned();
                let items = self
                    .hub
                    .queue()
                    .peek(&id, self.cfg.batch)
                    .map_err(|error| ChaosError::new(seed, format!("queue: {error}")))?;
                self.transcript.push(format!(
                    "{:04} hub serves {id} {} bundles",
                    self.round,
                    items.len()
                ));
                self.next_offer += 1;
                let offer = self.next_offer;
                let of = u32::try_from(items.len()).unwrap_or(u32::MAX);
                for (ordinal, item) in items.into_iter().enumerate() {
                    down.push(Frame::Batch {
                        node,
                        offer,
                        ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                        of,
                        position: item.position,
                        index: item.item.index,
                        bundle: Box::new(item.item.bundle),
                    });
                }
            }
            Frame::Ack { node, prefix } => {
                let id = self.node_id(node).to_owned();
                let missing = self.missing_through(node, prefix);
                self.hub
                    .on_ack_at(&id, prefix, now)
                    .map_err(|error| ChaosError::new(seed, format!("ack {id}: {error}")))?;
                self.nodes[node].hub_acked = prefix;
                self.transcript.push(format!(
                    "{:04} hub acked {id} prefix {prefix} ({} missing)",
                    self.round,
                    missing.len()
                ));
                self.acks.push(AckRecord {
                    node,
                    prefix,
                    round: self.round,
                    missing,
                });
            }
            Frame::Nack {
                node,
                position,
                code,
            } => {
                self.transcript.push(format!(
                    "{:04} hub heard {} nack pos {position} code {code}",
                    self.round,
                    self.node_id(node)
                ));
                self.nacks.push(NackRecord {
                    node,
                    position,
                    code,
                    round: self.round,
                });
            }
            Frame::Batch { .. } => {}
        }
        Ok(down)
    }

    /// Hand one frame to its node's session.
    ///
    /// A response to one offer is one message of the session: its frames may arrive duplicated,
    /// late or out of order, and the node applies the response only when every frame of it has
    /// arrived, in the order the hub served them. A response that never completes is dropped when
    /// a newer one starts, and the hub serves the same unacknowledged bundles again. The node
    /// intake's applied prefix is the highest position it has settled and cannot see a bundle it
    /// never received, so without this ordered session an acknowledgement would leap over a lost
    /// bundle (`tests/findings.rs` shows it).
    fn deliver(&mut self, frame: Frame) -> Result<Delivered, ChaosError> {
        let Frame::Batch {
            node,
            offer,
            of,
            ordinal,
            ..
        } = &frame
        else {
            return Ok(Delivered::Skipped);
        };
        let (node, offer, of, ordinal) = (*node, *offer, *of, *ordinal);
        let batch_cut = self.take_cut(node, Step::Batch);
        if let Some(edge) = batch_cut {
            self.note_hit(node, Step::Batch, edge);
            if edge == Edge::Before {
                return Ok(Delivered::Skipped);
            }
            // The frame landed, but the link died before the acknowledgement could leave.
            self.nodes[node].suppress_ack = true;
        }
        if offer <= self.nodes[node].last_offer {
            self.transcript.push(format!(
                "{:04} {} ignores a repeat of offer {offer}",
                self.round,
                self.node_id(node)
            ));
            return Ok(Delivered::Skipped);
        }
        let state = &mut self.nodes[node];
        // A newer response abandons the incomplete older ones.
        state.inbox.retain(|held, _| *held >= offer);
        state.inbox.entry(offer).or_default().insert(ordinal, frame);
        let complete = state
            .inbox
            .get(&offer)
            .is_some_and(|frames| u32::try_from(frames.len()).is_ok_and(|n| n == of));
        if !complete {
            return Ok(Delivered::Skipped);
        }
        let frames = state.inbox.remove(&offer).unwrap_or_default();
        state.last_offer = offer;
        let mut outcome = Delivered::Skipped;
        for (_, frame) in frames {
            match self.apply(frame)? {
                Delivered::Applied => outcome = Delivered::Applied,
                Delivered::Refused => {
                    outcome = Delivered::Refused;
                    // The rest of the response is not applied behind a refusal: the prefix must
                    // not move past it. The hub serves it again.
                    break;
                }
                Delivered::Skipped => break,
            }
        }
        Ok(outcome)
    }

    /// Apply one frame of a complete response.
    fn apply(&mut self, frame: Frame) -> Result<Delivered, ChaosError> {
        let Frame::Batch {
            node,
            position,
            index,
            bundle,
            ..
        } = frame
        else {
            return Ok(Delivered::Skipped);
        };
        let checkpoint_cut = self.take_cut(node, Step::Checkpoint);
        if checkpoint_cut == Some(Edge::Before) {
            // The process died inside the apply: an atomic apply is all or nothing, so the
            // bundle is rolled back and the hub re-sends it after the reconnect.
            self.note_hit(node, Step::Checkpoint, Edge::Before);
            return Ok(Delivered::Skipped);
        }
        let seed = self.cfg.seed;
        let delivery = Delivery {
            source: self.source.clone(),
            tenant: self.cfg.tenant.clone(),
            bundle: (*bundle).clone(),
        };
        let receipt = self.nodes[node]
            .intake
            .receive(delivery, self.clock.elapsed_ms())
            .map_err(|error| {
                ChaosError::new(
                    seed,
                    format!("node {} receive: {error}", self.node_id(node)),
                )
            })?;
        let outcome = match &receipt {
            Receipt::Applied(applied) => {
                self.transcript.push(format!(
                    "{:04} {} applied pos {position}/{} records {} duplicates {} older {}",
                    self.round,
                    self.node_id(node),
                    index,
                    applied.applied.records.len(),
                    applied.duplicates,
                    applied.older_ignored
                ));
                Delivered::Applied
            }
            Receipt::Refused(refusal) => {
                let node_id = self.node_id(node).to_owned();
                self.transcript.push(format!(
                    "{:04} {node_id} refused pos {position}: {}",
                    self.round, refusal.reason
                ));
                self.nodes[node].pending_nack =
                    Some((position, concat!("sync_node", ".refused").to_owned()));
                Delivered::Refused
            }
            Receipt::Parked { missing } | Receipt::Overflow { missing } => {
                self.transcript.push(format!(
                    "{:04} {} parked pos {position} missing {}",
                    self.round,
                    self.node_id(node),
                    missing.len()
                ));
                Delivered::Applied
            }
            Receipt::Held(reason) => {
                self.transcript.push(format!(
                    "{:04} {} held pos {position}: {reason:?}",
                    self.round,
                    self.node_id(node)
                ));
                Delivered::Applied
            }
        };
        if checkpoint_cut == Some(Edge::After) {
            // The apply and its checkpoint are durable; the process then died, so the volatile
            // acknowledgement is gone and the node re-derives it from its durable state.
            self.note_hit(node, Step::Checkpoint, Edge::After);
            self.nodes[node].restart()?;
        }
        Ok(outcome)
    }

    /// Misbehave the wire: reorder windows, duplicate every frame, lose some.
    fn chaos(&mut self, frames: Vec<Frame>) -> (Vec<Frame>, u64, u64) {
        let mut dropped = 0;
        let mut duplicated = 0;
        let mut frames = frames;
        let window = self.cfg.reorder_window.max(1);
        if window > 1 {
            for chunk in frames.chunks_mut(window) {
                self.rng.shuffle(chunk);
            }
        }
        let mut out: Vec<Frame> = Vec::with_capacity(frames.len());
        for frame in frames {
            if self.cfg.loss_permille > 0 && self.rng.chance(self.cfg.loss_permille) {
                dropped += 1;
                continue;
            }
            if self.cfg.duplicate {
                out.push(frame.clone());
                duplicated += 1;
            }
            out.push(frame);
        }
        (out, dropped, duplicated)
    }

    /// Take an armed cut for this step, if there is one.
    fn take_cut(&mut self, node: usize, step: Step) -> Option<Edge> {
        self.cuts.remove(&(node, step))
    }

    fn note_hit(&mut self, node: usize, step: Step, edge: Edge) {
        self.hits.push(CutHit {
            node,
            step,
            edge,
            round: self.round,
        });
        self.transcript.push(format!(
            "{:04} cut {} {} {}",
            self.round,
            self.node_id(node),
            step.name(),
            edge.name()
        ));
    }
}
