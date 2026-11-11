//! The wire model: batches, checkpoints, acknowledgements, handshakes and flow-control actions.
//!
//! Every type here is transport-independent (ADR-062 section 3): an edge issues a request and a
//! hub answers, so nothing in this module is a message a hub must send unprompted. The serde
//! names are stable wire contract; a consumer ignores fields it does not know (ADR-056), so a
//! newer peer's extra fields never break an older one.
//!
//! Two directions share the model:
//!
//! * **pull** (down, hub to edge): the edge asks from a sequence and the hub answers with a
//!   [`Batch`] or an empty answer;
//! * **push** (up, edge to hub): the edge sends a [`Batch`] and the hub answers with an [`Ack`]
//!   or a [`Nack`].
//!
//! Both are covered by [`Batch`] and its checksum, so the same integrity and idempotency rules
//! apply in each direction.

use serde::{Deserialize, Serialize};
use spot_events_contract::SourceId;

use crate::checksum::{BATCH_DOMAIN, Checksum, Encoder};
use crate::error::{self, Error, Result};
use crate::ids::{NodeId, ProtocolRange, ProtocolVersion};
use crate::record::Record;

/// A contiguous run of one source's stream: the unit a sender ships and a receiver applies.
///
/// `records` are exactly the stream positions `from_seq..=to_seq` in ascending order, and
/// `checksum` is the SHA-256 of the canonical encoding of the header plus those records. A
/// batch is immutable once built; [`Batch::verify`] re-checks both the run and the checksum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Batch {
    /// The stream the batch belongs to.
    pub source: SourceId,
    /// The first sequence the batch carries.
    pub from_seq: u64,
    /// The last sequence the batch carries.
    pub to_seq: u64,
    /// The records, in ascending sequence order.
    pub records: Vec<Record>,
    /// The integrity checksum over the header and the records.
    pub checksum: Checksum,
}

impl Batch {
    /// Builds a batch from an ascending, contiguous, non-empty run of records.
    ///
    /// Fails with [`error::INVALID_SEQUENCE`] when the records are not exactly the range from the
    /// first record's sequence to the last, and with [`error::INVALID_ARGUMENT`] when the run is
    /// empty.
    pub fn new(source: SourceId, records: Vec<Record>) -> Result<Self> {
        let Some(first) = records.first() else {
            return Err(Error::invalid_argument(
                "a batch carries at least one record",
            ));
        };
        let from_seq = first.seq;
        let mut expected = from_seq;
        for record in &records {
            if record.seq != expected {
                return Err(Error::invalid_sequence(format!(
                    "records are not the contiguous run from {from_seq}: expected {expected}, got {}",
                    record.seq
                )));
            }
            expected = expected.saturating_add(1);
        }
        let to_seq = expected - 1;
        let checksum = checksum_of(&source, from_seq, to_seq, &records);
        Ok(Self {
            source,
            from_seq,
            to_seq,
            records,
            checksum,
        })
    }

    /// The number of sequences the batch spans.
    pub fn len(&self) -> u64 {
        self.to_seq.saturating_sub(self.from_seq).saturating_add(1)
    }

    /// Whether the batch carries no records (never true for a batch built by [`Batch::new`], but
    /// a deserialized batch can claim it).
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Re-checks the run's shape and the checksum, and returns the checksum error for a mismatch.
    ///
    /// The receiver calls this before it looks at a single record: a corrupt or malformed batch
    /// is refused with `sync_protocol.corrupt_batch` and re-requested.
    pub fn verify(&self) -> Result<()> {
        if self.records.is_empty() {
            return Err(Error::invalid_sequence(
                "a batch carries at least one record",
            ));
        }
        if self.from_seq > self.to_seq {
            return Err(Error::invalid_sequence(
                "a batch's from_seq is above its to_seq",
            ));
        }
        if self.len() != self.records.len() as u64 {
            return Err(Error::invalid_sequence(format!(
                "a batch spans {} sequences but carries {} records",
                self.len(),
                self.records.len()
            )));
        }
        let mut expected = self.from_seq;
        for record in &self.records {
            if record.seq != expected {
                return Err(Error::invalid_sequence(format!(
                    "records are not the contiguous run from {}: expected {expected}, got {}",
                    self.from_seq, record.seq
                )));
            }
            expected = expected.saturating_add(1);
        }
        let actual = checksum_of(&self.source, self.from_seq, self.to_seq, &self.records);
        if actual != self.checksum {
            return Err(Error::corrupt_batch(format!(
                "checksum mismatch on {} sequences {}..={}",
                self.source, self.from_seq, self.to_seq
            )));
        }
        Ok(())
    }

    /// The canonical bytes the checksum covers.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.bytes(BATCH_DOMAIN);
        encoder.text(self.source.as_str());
        encoder.u64(self.from_seq);
        encoder.u64(self.to_seq);
        encoder.u64(self.records.len() as u64);
        for record in &self.records {
            record.encode(&mut encoder);
        }
        encoder.finish()
    }
}

fn checksum_of(source: &SourceId, from_seq: u64, to_seq: u64, records: &[Record]) -> Checksum {
    let mut encoder = Encoder::new();
    encoder.bytes(BATCH_DOMAIN);
    encoder.text(source.as_str());
    encoder.u64(from_seq);
    encoder.u64(to_seq);
    encoder.u64(records.len() as u64);
    for record in records {
        record.encode(&mut encoder);
    }
    Checksum::of(&encoder.finish())
}

/// How far one source's stream has been applied: the last sequence the receiver stored durably.
///
/// `last_applied_seq` is a contiguous high-water mark: every sequence at or below it has been
/// applied. It starts at `0` (nothing applied) so the first expected sequence is `1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Checkpoint {
    /// The stream this checkpoint belongs to.
    pub source: SourceId,
    /// The last sequence applied durably.
    pub last_applied_seq: u64,
}

impl Checkpoint {
    /// A checkpoint from its parts.
    pub fn new(source: SourceId, last_applied_seq: u64) -> Self {
        Self {
            source,
            last_applied_seq,
        }
    }

    /// The checkpoint before anything has been applied.
    pub fn start(source: SourceId) -> Self {
        Self {
            source,
            last_applied_seq: 0,
        }
    }

    /// The next sequence the receiver expects.
    pub fn next_expected(&self) -> u64 {
        self.last_applied_seq.saturating_add(1)
    }

    /// Whether the receiver has applied `seq`.
    pub fn covers(&self, seq: u64) -> bool {
        seq <= self.last_applied_seq
    }
}

/// A receiver's acknowledgement: everything through `through_seq` is applied durably.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Ack {
    /// The stream acknowledged.
    pub source: SourceId,
    /// The last sequence known applied.
    pub through_seq: u64,
}

impl Ack {
    /// An acknowledgement from its parts.
    pub fn new(source: SourceId, through_seq: u64) -> Self {
        Self {
            source,
            through_seq,
        }
    }
}

/// Why a receiver refused a batch. A nack always names the sequence the sender should resume
/// from; a sender never infers a skip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Nack {
    /// A batch arrived that does not start at the receiver's next expected sequence. The sender
    /// re-sends from `expected`.
    Gap {
        /// The stream.
        source: SourceId,
        /// The sequence the receiver expects next.
        expected: u64,
    },
    /// A batch failed its checksum or its run's shape. The sender re-sends from `from_seq`.
    Corrupt {
        /// The stream.
        source: SourceId,
        /// The first sequence of the refused batch.
        from_seq: u64,
    },
    /// No protocol version is common to the two peers.
    VersionMismatch {
        /// The receiver's range.
        ours: ProtocolRange,
        /// The sender's range.
        theirs: ProtocolRange,
    },
    /// The two peers' metadata epochs differ beyond the allowed skew (ADR-066).
    MetadataEpoch {
        /// The receiver's epoch.
        ours: u64,
        /// The sender's epoch.
        theirs: u64,
    },
    /// A batch is larger than the receiver's flow-control limit.
    BatchTooLarge {
        /// The stream.
        source: SourceId,
        /// The largest span the receiver accepts.
        limit: u64,
        /// The refused batch's span.
        got: u64,
    },
}

impl Nack {
    /// The stable `sync.*` code this nack reports.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Gap { .. } => error::GAP,
            Self::Corrupt { .. } => error::CORRUPT_BATCH,
            Self::VersionMismatch { .. } => error::VERSION_MISMATCH,
            Self::MetadataEpoch { .. } => error::METADATA_EPOCH,
            Self::BatchTooLarge { .. } => error::BATCH_TOO_LARGE,
        }
    }
}

/// What a receiver decided about one batch.
#[derive(Debug)]
#[non_exhaustive]
pub enum Outcome {
    /// Applied (or already applied); reply with this acknowledgement.
    Ack(Ack),
    /// Refused; reply with this nack and the sender resends from the named sequence.
    Nack(Nack),
    /// An internal failure applying or checkpointing. The session ends and retries; the durable
    /// apply, if any, is absorbed by idempotency on the retry.
    Failed(Error),
}

impl Outcome {
    /// The acknowledgement, if the batch was accepted.
    pub fn into_ack(self) -> Option<Ack> {
        match self {
            Self::Ack(ack) => Some(ack),
            _ => None,
        }
    }

    /// Whether the batch was accepted.
    pub fn is_ack(&self) -> bool {
        matches!(self, Self::Ack(_))
    }
}

/// A handshake opening a session: who the node is, which protocol versions it speaks, and which
/// metadata epoch its catalog is at.
///
/// An edge sends `Hello` first; a hub answers with the negotiated version, or a
/// [`Nack::VersionMismatch`]. There is no hub-initiated message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Hello {
    /// The node's identity.
    pub node: NodeId,
    /// The protocol versions the node speaks.
    pub protocol_range: ProtocolRange,
    /// The metadata (catalog) epoch the node's records were written under (ADR-066).
    pub metadata_epoch: u64,
}

impl Hello {
    /// A handshake.
    pub fn new(node: NodeId, protocol_range: ProtocolRange, metadata_epoch: u64) -> Self {
        Self {
            node,
            protocol_range,
            metadata_epoch,
        }
    }

    /// The highest protocol version `self` and `peer` share, or a [`Nack::VersionMismatch`].
    pub fn negotiate(&self, peer: &Hello) -> core::result::Result<ProtocolVersion, Nack> {
        self.protocol_range
            .negotiate(&peer.protocol_range)
            .ok_or(Nack::VersionMismatch {
                ours: self.protocol_range,
                theirs: peer.protocol_range,
            })
    }

    /// Whether `peer`'s metadata epoch equals this node's. Epoch policy (which skew is allowed)
    /// is a later ADR's decision (ADR-066); this crate only reports equality.
    pub fn same_epoch(&self, peer: &Hello) -> bool {
        self.metadata_epoch == peer.metadata_epoch
    }
}

/// The next step a sender decided on, from one [`Offer`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Ship this batch and expect an [`Ack`] or [`Nack`].
    Send(Batch),
    /// Nothing to send now: no records at or after the cursor, or the in-flight window is full.
    Wait,
}

impl Action {
    /// The batch to send, if any.
    pub fn into_batch(self) -> Option<Batch> {
        match self {
            Self::Send(batch) => Some(batch),
            Self::Wait => None,
        }
    }

    /// Whether the action is to wait.
    pub fn is_wait(&self) -> bool {
        matches!(self, Self::Wait)
    }
}

/// What a sender is offered on one step: the records currently readable from one source, in
/// ascending sequence order.
///
/// The sender takes only what it needs from the offer: records at or after its cursor, contiguous
/// from the cursor, and inside its in-flight window. An offer may repeat records the sender has
/// already sent (a benign re-read of the source stream).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Offer {
    /// The stream being offered.
    pub source: SourceId,
    /// Records in ascending sequence order.
    pub records: Vec<Record>,
}

impl Offer {
    /// An offer.
    pub fn new(source: SourceId, records: Vec<Record>) -> Self {
        Self { source, records }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::record::RecordName;

    fn source() -> SourceId {
        SourceId::new("head.office.app#hub").unwrap()
    }

    fn records(from: u64, count: u64) -> Vec<Record> {
        (0..count)
            .map(|index| {
                Record::new(
                    RecordName::new(&format!("doc_{from}_{index}")).unwrap(),
                    1,
                    from + index,
                    "{}",
                )
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn a_batch_is_its_contiguous_run() {
        let batch = Batch::new(source(), records(3, 4)).unwrap();
        assert_eq!((batch.from_seq, batch.to_seq, batch.len()), (3, 6, 4));
        assert!(batch.verify().is_ok());
    }

    #[test]
    fn a_gap_in_the_run_is_refused() {
        let mut run = records(1, 3);
        run[1].seq = 5;
        let error = Batch::new(source(), run).err().unwrap();
        assert_eq!(error.code, error::INVALID_SEQUENCE);
    }

    #[test]
    fn a_corrupt_batch_is_caught() {
        let mut batch = Batch::new(source(), records(1, 2)).unwrap();
        batch.records[0].payload = "tampered".to_owned();
        let error = batch.verify().err().unwrap();
        assert_eq!(error.code, error::CORRUPT_BATCH);
    }

    #[test]
    fn checkpoint_counts_from_one() {
        let checkpoint = Checkpoint::start(source());
        assert_eq!(checkpoint.next_expected(), 1);
        assert!(checkpoint.covers(0));
        assert!(!checkpoint.covers(1));
    }

    #[test]
    fn handshake_negotiates_or_refuses() {
        let node = NodeId::new("head-office").unwrap();
        let edge = Hello::new(
            node.clone(),
            ProtocolRange::new(ProtocolVersion::new(2), ProtocolVersion::new(3)).unwrap(),
            7,
        );
        let hub = Hello::new(node, ProtocolRange::current(), 7);
        assert_eq!(
            edge.negotiate(&hub),
            Err(Nack::VersionMismatch {
                ours: edge.protocol_range,
                theirs: hub.protocol_range,
            })
        );
        assert!(hub.same_epoch(&edge));
    }
}
