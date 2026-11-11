//! Plain data: the steps a cut can interrupt, the frames on the wire and the settings.

use std::fmt;

use spot_sync_closure::Bundle;

/// One step of the sync protocol, as a cut can interrupt it.
///
/// The five steps mirror ADR-067 section 7's "connection cuts at every protocol step" and the
/// protocol's own vocabulary (`spot-sync-protocol`, `message`):
///
/// * `Offer` — the edge asks for work (a pull request; a batch is only ever an answer to one).
/// * `Batch` — a bundle travels from the hub to the node.
/// * `Ack` — the node reports the prefix it applied.
/// * `Checkpoint` — the node's durable checkpoint for the bundle it just applied.
/// * `Nack` — the node refuses a bundle and says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// The edge's request for work.
    Offer,
    /// One bundle on the wire.
    Batch,
    /// The node's applied-prefix acknowledgement.
    Ack,
    /// The node's durable checkpoint of an applied bundle.
    Checkpoint,
    /// The node's refusal of a bundle.
    Nack,
}

impl Step {
    /// Every step, in protocol order: what `tests/cuts.rs` iterates.
    #[must_use]
    pub fn all() -> [Self; 5] {
        [
            Self::Offer,
            Self::Batch,
            Self::Ack,
            Self::Checkpoint,
            Self::Nack,
        ]
    }

    /// The step's name, as the replay line and the transcript print it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Offer => "offer",
            Self::Batch => "batch",
            Self::Ack => "ack",
            Self::Checkpoint => "checkpoint",
            Self::Nack => "nack",
        }
    }
}

/// Which side of a step a cut falls on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// The step never runs: nothing is produced and nothing changes.
    Before,
    /// The step runs on its own side and the frame it produces is lost in flight, so the peer
    /// never learns it happened and asks again after the reconnect.
    After,
}

impl Edge {
    /// The edge's name for the transcript.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// One armed cut that fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutHit {
    /// The node whose link was cut.
    pub node: usize,
    /// The step it fell on.
    pub step: Step,
    /// Which side of it.
    pub edge: Edge,
    /// The round it fired in.
    pub round: usize,
}

/// One frame on the wire. Every frame is one message; the wire may drop, duplicate or reorder it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// The edge asks the hub for the next bundles.
    Offer {
        /// The asking node.
        node: usize,
    },
    /// One bundle, addressed to one node.
    Batch {
        /// The receiving node.
        node: usize,
        /// The offer this frame answers: one response is one message of the session.
        offer: u64,
        /// This frame's place in the response, in the order the hub serves it.
        ordinal: u32,
        /// How many frames the response has.
        of: u32,
        /// The stream position the bundle ends at (the queue's identity for it).
        position: u64,
        /// The bundle's index within its position.
        index: u32,
        /// The bundle itself.
        bundle: Box<Bundle>,
    },
    /// The node's applied prefix.
    Ack {
        /// The acknowledging node.
        node: usize,
        /// The prefix it applied.
        prefix: u64,
    },
    /// The node's refusal of a bundle.
    Nack {
        /// The refusing node.
        node: usize,
        /// The position of the refused bundle.
        position: u64,
        /// The catalogue code of the refusal.
        code: String,
    },
}

impl Frame {
    /// The node the frame belongs to.
    #[must_use]
    pub fn node(&self) -> usize {
        match self {
            Self::Offer { node }
            | Self::Batch { node, .. }
            | Self::Ack { node, .. }
            | Self::Nack { node, .. } => *node,
        }
    }

    /// How the transcript prints it.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Offer { node } => format!("node-{node} offer"),
            Self::Batch {
                node,
                offer,
                ordinal,
                of,
                position,
                index,
                bundle,
            } => format!(
                "node-{node} batch offer={offer} {ordinal}/{of} pos={position} index={index} records={}",
                bundle.records.len()
            ),
            Self::Ack { node, prefix } => format!("node-{node} ack prefix={prefix}"),
            Self::Nack {
                node,
                position,
                code,
            } => format!("node-{node} nack pos={position} code={code}"),
        }
    }
}

/// What one round did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoundReport {
    /// The round number.
    pub round: usize,
    /// Frames the edge sent.
    pub up: usize,
    /// Frames the hub sent.
    pub down: usize,
    /// Frames the wire dropped (a cut, or a lossy wire).
    pub dropped: usize,
    /// Frames the wire duplicated.
    pub duplicated: usize,
    /// Bundles the nodes applied.
    pub applied: usize,
    /// Bundles a node refused.
    pub refused: usize,
    /// The hub's read cursor after the round.
    pub cursor: u64,
}

/// One acknowledgement, with the evidence the checker re-reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AckRecord {
    /// The node.
    pub node: usize,
    /// The prefix it claimed.
    pub prefix: u64,
    /// The round it was sent in.
    pub round: usize,
    /// The positions at or below `prefix`, in the node's scope, that it did **not** hold.
    pub missing: Vec<u64>,
}

/// One refusal the node reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NackRecord {
    /// The node.
    pub node: usize,
    /// The refused bundle's position.
    pub position: u64,
    /// The catalogue code.
    pub code: String,
    /// The round.
    pub round: usize,
}

/// Why a world could not be built or could not converge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChaosError {
    /// The seed, so the failure replays.
    pub seed: u64,
    /// A plain-language reason.
    pub what: String,
}

impl ChaosError {
    /// A failure for `seed`.
    #[must_use]
    pub fn new(seed: u64, what: impl Into<String>) -> Self {
        Self {
            seed,
            what: what.into(),
        }
    }
}

impl fmt::Display for ChaosError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SEED={} {}", self.seed, self.what)
    }
}

impl std::error::Error for ChaosError {}

impl From<spot_sync_hub::HubError> for ChaosError {
    fn from(error: spot_sync_hub::HubError) -> Self {
        Self::new(0, format!("hub: {error}"))
    }
}

impl From<spot_sync_node::NodeError> for ChaosError {
    fn from(error: spot_sync_node::NodeError) -> Self {
        Self::new(0, format!("node: {error}"))
    }
}

impl From<spot_sync_bootstrap::BootstrapError> for ChaosError {
    fn from(error: spot_sync_bootstrap::BootstrapError) -> Self {
        Self::new(0, format!("bootstrap: {error}"))
    }
}

/// The World's settings. Values are settings, never hidden constants; a run is a pure function of
/// them and of `seed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldConfig {
    /// The seed every draw comes from.
    pub seed: u64,
    /// How many nodes pull.
    pub nodes: usize,
    /// How many records the hub's stream holds.
    pub records: usize,
    /// How many versions are written per record, one wave at a time.
    pub versions: u64,
    /// The tenant the hub serves.
    pub tenant: String,
    /// A second tenant whose records are written to the same stream and must never be delivered.
    pub foreign_tenant: String,
    /// Whether to write the foreign tenant's records at all.
    pub write_foreign: bool,
    /// Most records one source transaction groups, so a batch can carry more than one.
    pub group_max: usize,
    /// Bundles a node asks for in one round.
    pub batch: usize,
    /// The wire's reorder window: frames are shuffled in windows of this size. `1` keeps order.
    pub reorder_window: usize,
    /// Whether the wire duplicates every frame.
    pub duplicate: bool,
    /// Chance in parts per thousand that the wire drops a frame on its own.
    pub loss_permille: u64,
    /// Virtual milliseconds one round advances.
    pub tick_ms: u64,
    /// The hub wall clock at the start, milliseconds since the epoch.
    pub epoch_ms: i64,
    /// The stream generation the hub is on.
    pub generation: u64,
    /// The generation a restored node comes back with: older than the hub's.
    pub restored_generation: u64,
    /// Whether the `Customer` entity travels in the mandatory lane (served before the bulk, out
    /// of position order). Off by default: see `tests/findings.rs` for why.
    pub mandatory_customers: bool,
    /// Write every wave before the hub reads any (the hub's images then run ahead of the
    /// entries it bundles). Off by default: see `tests/findings.rs` for why.
    pub images_run_ahead: bool,
    /// A node whose scope holds only these entities (`None`: every node is fully scoped).
    pub narrow_node: Option<usize>,
    /// The entities the narrow node may receive.
    pub narrow_entities: Vec<String>,
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            nodes: 3,
            records: 6,
            versions: 2,
            tenant: "acme".to_owned(),
            foreign_tenant: "beta".to_owned(),
            write_foreign: true,
            group_max: 2,
            batch: 2,
            reorder_window: 1,
            duplicate: false,
            loss_permille: 0,
            tick_ms: 1,
            epoch_ms: 1_760_000_000_000,
            generation: 1,
            restored_generation: 0,
            mandatory_customers: false,
            images_run_ahead: false,
            narrow_node: None,
            narrow_entities: Vec::new(),
        }
    }
}

impl WorldConfig {
    /// A configuration for `seed` with the defaults.
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        Self {
            seed,
            ..Self::default()
        }
    }

    /// A smallest-useful world: one node, a handful of records, no faults.
    #[must_use]
    pub fn tiny(seed: u64) -> Self {
        Self {
            seed,
            nodes: 1,
            records: 4,
            versions: 1,
            batch: 4,
            ..Self::default()
        }
    }
}
