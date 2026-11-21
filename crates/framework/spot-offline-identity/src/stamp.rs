//! Offline write metadata and the local journal (ADR-070 rule 12.13 i).
//!
//! Each write made while the node is disconnected carries an [`OfflineStamp`]: author,
//! assignment, the snapshot epoch and its age at the time of the write, and a device-local
//! sequence number. Stamps are appended to an [`OfflineJournal`], a hash chain built on
//! `spot-audit-chain` ([`ChainEntry`], `genesis_hash`), so an entry cannot be altered, removed,
//! reordered or inserted without the hub finding out ([`verify_entries`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_audit_chain::{AuditRecord, ChainEntry, Hash, PayloadValue, genesis_hash};

use crate::audit::{STAMP_ACTOR_PREFIX, audit_record, int, stamp_actor, text};
use crate::error::OfflineError;
use crate::types::{NodeId, TenantId, Tier};

const KIND: &str = concat!("offline", ".write");

/// The metadata of one offline write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineStamp {
    /// The tenant.
    pub tenant: TenantId,
    /// The node that wrote it.
    pub node: NodeId,
    /// The user who acted.
    pub author: String,
    /// The role assignment the right came from.
    pub assignment_id: String,
    /// The tier of the action.
    pub tier: Tier,
    /// The snapshot epoch the node held.
    pub epoch: u64,
    /// How old that snapshot's lease basis was, in milliseconds on the node's monotonic clock.
    pub epoch_age_ms: u64,
    /// The device-local sequence number; strictly increasing by one per write.
    pub device_seq: u64,
}

impl OfflineStamp {
    /// The audit record the journal chains.
    #[must_use]
    pub fn to_record(&self, at_micros: i64) -> AuditRecord {
        audit_record(
            KIND,
            &stamp_actor(&self.author),
            at_micros,
            &[
                ("tenant", text(self.tenant.as_str())),
                ("node", text(self.node.as_str())),
                ("assignment_id", text(&self.assignment_id)),
                ("tier", int(self.tier.index() as u64)),
                ("epoch", int(self.epoch)),
                ("epoch_age_ms", int(self.epoch_age_ms)),
                ("device_seq", int(self.device_seq)),
            ],
        )
    }

    /// Reads a stamp back from a journal record.
    ///
    /// # Errors
    /// [`OfflineError::StampMalformed`] when the record is not a well-formed stamp.
    pub fn from_record(record: &AuditRecord) -> Result<Self, OfflineError> {
        let bad = || OfflineError::StampMalformed;
        if record.kind != KIND {
            return Err(bad());
        }
        let author = record
            .actor
            .strip_prefix(STAMP_ACTOR_PREFIX)
            .ok_or_else(bad)?
            .to_owned();
        let p = &record.payload;
        let tier = match number(p, "tier")? {
            0 => Tier::One,
            1 => Tier::Two,
            2 => Tier::Three,
            _ => return Err(bad()),
        };
        Ok(Self {
            tenant: TenantId::new(string(p, "tenant")?).map_err(|_| bad())?,
            node: NodeId::new(string(p, "node")?).map_err(|_| bad())?,
            author,
            assignment_id: string(p, "assignment_id")?.to_owned(),
            tier,
            epoch: number(p, "epoch")?,
            epoch_age_ms: number(p, "epoch_age_ms")?,
            device_seq: number(p, "device_seq")?,
        })
    }
}

fn string<'a>(p: &'a BTreeMap<String, PayloadValue>, key: &str) -> Result<&'a str, OfflineError> {
    match p.get(key) {
        Some(PayloadValue::Text(t)) => Ok(t),
        _ => Err(OfflineError::StampMalformed),
    }
}

fn number(p: &BTreeMap<String, PayloadValue>, key: &str) -> Result<u64, OfflineError> {
    match p.get(key) {
        Some(PayloadValue::Int(i)) => u64::try_from(*i).map_err(|_| OfflineError::StampMalformed),
        _ => Err(OfflineError::StampMalformed),
    }
}

/// Checks a run of journal entries: contiguous sequence numbers after `prev_seq`, each
/// `prev_hash` the hash before it (starting from `prev_hash`), each stored hash the hash of
/// its content.
///
/// # Errors
/// [`OfflineError::JournalTampered`] naming the first entry that fails.
pub fn verify_entries(
    prev_seq: u64,
    prev_hash: Hash,
    entries: &[ChainEntry],
) -> Result<(), OfflineError> {
    let mut seq = prev_seq;
    let mut hash = prev_hash;
    for entry in entries {
        let expected = seq.saturating_add(1);
        if entry.seq != expected || entry.prev_hash != hash || entry.recompute_hash() != entry.hash
        {
            return Err(OfflineError::JournalTampered { seq: entry.seq });
        }
        seq = entry.seq;
        hash = entry.hash;
    }
    Ok(())
}

/// Where journal entries are kept. `append` must be durable when it returns.
pub trait JournalStore {
    /// All entries, oldest first.
    ///
    /// # Errors
    /// [`OfflineError::JournalStore`] when the store cannot read.
    fn load(&self) -> Result<Vec<ChainEntry>, OfflineError>;
    /// Appends one entry.
    ///
    /// # Errors
    /// [`OfflineError::JournalStore`] when the entry is not durably stored.
    fn append(&mut self, entry: &ChainEntry) -> Result<(), OfflineError>;
}

/// An in-memory journal store.
#[derive(Clone, Debug, Default)]
pub struct MemoryJournalStore {
    /// The entries.
    pub entries: Vec<ChainEntry>,
    /// When set, appends fail.
    pub fail: bool,
}

impl JournalStore for MemoryJournalStore {
    fn load(&self) -> Result<Vec<ChainEntry>, OfflineError> {
        Ok(self.entries.clone())
    }

    fn append(&mut self, entry: &ChainEntry) -> Result<(), OfflineError> {
        if self.fail {
            return Err(OfflineError::JournalStore);
        }
        self.entries.push(entry.clone());
        Ok(())
    }
}

/// The node's append-only, tamper-evident journal of offline writes. Its chain site is the
/// node id, so a journal cannot be presented as another node's.
pub struct OfflineJournal<S: JournalStore> {
    site: String,
    store: S,
    head_seq: u64,
    head_hash: Hash,
    last_device_seq: u64,
}

impl<S: JournalStore> OfflineJournal<S> {
    /// Opens the journal of `node`, verifying the whole chain.
    ///
    /// # Errors
    /// [`OfflineError::JournalTampered`] when the stored chain fails, or
    /// [`OfflineError::JournalStore`] when it cannot be read. A journal that fails is not
    /// opened for appending: the node must not extend a damaged chain.
    pub fn open(node: &NodeId, store: S) -> Result<Self, OfflineError> {
        let site = node.as_str().to_owned();
        let entries = store.load()?;
        let genesis = genesis_hash(&site);
        verify_entries(0, genesis, &entries)?;
        let (head_seq, head_hash) = entries.last().map_or((0, genesis), |e| (e.seq, e.hash));
        let last_device_seq = match entries.last() {
            Some(e) => OfflineStamp::from_record(&e.record)?.device_seq,
            None => 0,
        };
        Ok(Self {
            site,
            store,
            head_seq,
            head_hash,
            last_device_seq,
        })
    }

    /// The device sequence number the next stamp must carry.
    #[must_use]
    pub fn next_device_seq(&self) -> u64 {
        self.last_device_seq.saturating_add(1)
    }

    /// The chain site (the node id).
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The store (tests read it).
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Appends `stamp`. The write it describes must not proceed unless this succeeds.
    ///
    /// # Errors
    /// [`OfflineError::JournalSequence`] unless `stamp.device_seq` is the next number;
    /// [`OfflineError::StampMalformed`] if the stamp names another node than this journal's;
    /// [`OfflineError::JournalStore`] when the entry is not durably stored.
    pub fn append(
        &mut self,
        stamp: &OfflineStamp,
        at_micros: i64,
    ) -> Result<ChainEntry, OfflineError> {
        if stamp.node.as_str() != self.site {
            return Err(OfflineError::StampMalformed);
        }
        if stamp.device_seq != self.next_device_seq() {
            return Err(OfflineError::JournalSequence {
                previous: self.last_device_seq,
                got: stamp.device_seq,
            });
        }
        let record = stamp.to_record(at_micros);
        record
            .validate()
            .map_err(|_| OfflineError::StampMalformed)?;
        let entry = ChainEntry::new(self.head_seq.saturating_add(1), self.head_hash, record);
        self.store.append(&entry)?;
        self.head_seq = entry.seq;
        self.head_hash = entry.hash;
        self.last_device_seq = stamp.device_seq;
        Ok(entry)
    }
}
