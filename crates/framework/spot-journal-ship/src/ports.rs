//! The seams of the shipper: everything it reads or writes besides the relay is a trait, so the
//! same loop runs against PostgreSQL in the host and against memory in a test that kills it at
//! every step boundary.

use std::future::Future;
use std::pin::Pin;

use spot_events_contract::envelope::Position;
use spot_journal_commit::ChunkRelay;
use spot_journal_manifest::Fork;

use crate::error::ShipFault;

/// The boxed future of a port call.
pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One outbox entry as the shipper sees it: its position and its envelope as one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxEntry {
    /// The position `(generation, seq)` in the site's outbox.
    pub position: Position,
    /// The envelope as one line of UTF-8 JSON.
    pub line: Vec<u8>,
}

/// One read of the outbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutboxRead {
    /// Up to the asked number of entries after the bookmark, in position order. Empty at the head.
    Entries(Vec<OutboxEntry>),
    /// The bookmark is too old, from another generation, or beyond the head: continue from the
    /// start of the current generation (position `0, 0`).
    Resync,
}

/// The site's outbox, read in position order.
pub trait OutboxSource: Send + Sync {
    /// Up to `limit` entries strictly after `after`. `Position { generation: 0, seq: 0 }` is the
    /// start of the current generation.
    fn read(&self, after: Position, limit: usize) -> PortFuture<'_, Result<OutboxRead, ShipFault>>;
}

/// How a generation other than the first was opened, kept so a restart reopens the very same
/// fork record (the relay compares it with the manifest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkMark {
    /// The last sequence number of the abandoned generation that is still valid.
    pub forked_from: u64,
    /// Unix milliseconds the fork was recorded.
    pub at_millis: i64,
}

/// The persisted watermark of one source: the last position known durable in the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    /// `(generation, seq)` last made durable; `(0, 0)` before anything was shipped.
    pub position: Position,
    /// The fork record that opened `position.generation`, when it is not generation 1.
    pub fork: Option<ForkMark>,
}

impl Mark {
    /// Nothing shipped yet.
    #[must_use]
    pub fn start() -> Self {
        Self {
            position: Position {
                generation: 0,
                seq: 0,
            },
            fork: None,
        }
    }
}

/// Where the watermark lives (`journal_ship_state`, one row per site and source).
pub trait WatermarkStore: Send + Sync {
    /// The stored mark of `source`, if there is one.
    fn load<'a>(&'a self, source: &'a str) -> PortFuture<'a, Result<Option<Mark>, ShipFault>>;
    /// Stores `mark` for `source`.
    fn save<'a>(
        &'a self,
        source: &'a str,
        mark: &'a Mark,
        now_millis: i64,
    ) -> PortFuture<'a, Result<(), ShipFault>>;
}

/// The one-writer rule: the shipper writes only while it holds the site's lease.
pub trait WriterLease: Send + Sync {
    /// Takes the lease or renews it; `true` only while this node is the writer (the fencing
    /// token is still the current one).
    fn hold(&self) -> PortFuture<'_, bool>;
    /// Gives the lease up at a clean stop.
    fn release(&self) -> PortFuture<'_, ()>;
}

/// The conditions the shipper raises, as data; the host maps them onto the conditions handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alarm {
    /// The durability policy is not being met (`RPO_BREACH`).
    RpoBreach {
        /// The policy text (`remote-sync`, `replica-sync`, `local`).
        policy: &'static str,
        /// How long the shipper has been behind, in seconds.
        behind_secs: u64,
    },
    /// The stored chain is damaged or a position is missing (`JOURNAL_GAP`).
    JournalGap {
        /// The journal source.
        source: String,
        /// The generation concerned.
        generation: u64,
        /// What is missing or damaged, as a finding code.
        range: String,
    },
    /// The journal key has no verified backup (`KEY_BACKUP_MISSING`).
    KeyBackupMissing {
        /// The key's name.
        key: String,
    },
}

/// Which alarm to clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlarmKind {
    /// `RPO_BREACH`.
    RpoBreach,
    /// `JOURNAL_GAP`.
    JournalGap,
    /// `KEY_BACKUP_MISSING`.
    KeyBackupMissing,
}

impl Alarm {
    /// The kind of this alarm.
    #[must_use]
    pub fn kind(&self) -> AlarmKind {
        match self {
            Self::RpoBreach { .. } => AlarmKind::RpoBreach,
            Self::JournalGap { .. } => AlarmKind::JournalGap,
            Self::KeyBackupMissing { .. } => AlarmKind::KeyBackupMissing,
        }
    }
}

impl AlarmKind {
    /// The catalogue id of the condition (`spot-sync-conditions`).
    #[must_use]
    pub fn condition_id(self) -> &'static str {
        match self {
            Self::RpoBreach => "RPO_BREACH",
            Self::JournalGap => "JOURNAL_GAP",
            Self::KeyBackupMissing => "KEY_BACKUP_MISSING",
        }
    }
}

/// Raises and clears conditions.
pub trait Alarms: Send + Sync {
    /// Raises `alarm` on `subject` (idempotent while it stays open).
    fn raise<'a>(&'a self, subject: &'a str, alarm: Alarm) -> PortFuture<'a, ()>;
    /// Clears the condition of `kind` on `subject`.
    fn clear<'a>(&'a self, subject: &'a str, kind: AlarmKind) -> PortFuture<'a, ()>;
}

/// The transform every line goes through on its way to the relay: personal fields are sealed
/// here, so the relay never sees one in clear.
pub trait Protector: Send + Sync {
    /// The line to write for `line`.
    ///
    /// # Errors
    /// [`ShipFault::Protect`]; the line is not written.
    fn protect(&self, line: &[u8]) -> Result<Vec<u8>, ShipFault>;
}

/// What a verification pass found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainState {
    /// Hashes, links, the manifest signature and the listed segments are all in order.
    Clean,
    /// Something is wrong; the code is the first finding's.
    Damaged {
        /// The verifier's finding code.
        code: &'static str,
    },
}

/// A light verification of the stored chain of one generation.
pub trait ChainChecker: Send + Sync {
    /// Verifies `generation` of this source.
    fn check(&self, generation: u64) -> PortFuture<'_, Result<ChainState, ShipFault>>;
}

/// Whether the journal key has a verified backup. The provider reports it; `None` means it
/// cannot tell.
pub trait KeyBackup: Send + Sync {
    /// The backup status.
    fn backed_up(&self) -> PortFuture<'_, Option<bool>>;
}

/// The relay as the shipper needs it: the commit driver's seam plus sealing.
pub trait ShipRelay: ChunkRelay {
    /// Seals the open segment when the limits say so; `true` when one was sealed.
    fn seal_if_due(
        &mut self,
        now_millis: i64,
    ) -> impl Future<Output = Result<bool, ShipFault>> + Send;

    /// The last sequence number listed by the signed manifest (sealed segments): durable under
    /// every policy.
    fn sealed_through(&self) -> u64;
}

/// Opens relays: generation 1, or the generation a fork record opens.
pub trait RelayFactory: Send + Sync {
    /// The relay type.
    type Relay: ShipRelay;

    /// Opens (and resumes) the relay of `generation`; `fork` is the record for a generation
    /// after the first.
    fn open<'a>(
        &'a self,
        generation: u64,
        fork: Option<Fork>,
        now_millis: i64,
    ) -> PortFuture<'a, Result<Self::Relay, ShipFault>>;
}
