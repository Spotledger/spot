//! How one refused entry is carried by a quarantine item.
//!
//! A balance entry is not a replicated document: there is no DocType and no version. The
//! quarantine queue's store nevertheless keeps every item as one immutable record image, so this
//! module packs the entry into a [`ReceivedRecord`] under a dedicated entity name, with the
//! reason code beside it. The record name is the entry id, so a second placement of the same
//! `(node, entry)` finds the first one and changes nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_quarantine::QuarantineError;
use spot_replica_apply::ReceivedRecord;
use spot_shared_balance::{
    BalanceId, Entry, EntryId, EntryKind, Millis, NodeId, RejectReason, Sign,
};
use spot_storage_contract::{DocumentData, Fields, Name, RecordId, Value};
use spot_sync_protocol::{RecordName, SourceId};

/// The entity name a refused entry is stored under in the quarantine queue. It is not a
/// DocType: it only gives the payload a stable shape for a store that keys records by entity.
pub const ENTRY_ENTITY: &str = "shared_balance_entry";

/// The record-name prefix (the entry id follows it).
pub const ENTRY_NAME_PREFIX: &str = "shared_balance_entry:";

/// The single field carrying the entry body.
const ENTRY_FIELD: &str = "entry";
/// The field carrying the reason code.
const REASON_FIELD: &str = "reason";
/// The field carrying the node, for a person reading the payload without the metadata.
const NODE_FIELD: &str = "node";

/// The record version every refused entry is stored at: an entry never has versions.
const ENTRY_VERSION: u64 = 1;

/// The immutable body of one refused entry.
///
/// Money stays an integer count of minor units (ADR-12); the node time is whole milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EntryPayload {
    node: String,
    seq: u64,
    balance: String,
    kind: String,
    amount: i64,
    at: u64,
}

impl EntryPayload {
    fn of(entry: &Entry) -> Self {
        Self {
            node: entry.id.node.0.clone(),
            seq: entry.id.seq,
            balance: entry.balance.0.clone(),
            kind: kind_name(entry.kind),
            amount: entry.amount,
            at: entry.at.0,
        }
    }

    fn to_entry(&self) -> Option<Entry> {
        let kind = parse_kind(&self.kind)?;
        Entry::new(
            EntryId {
                node: NodeId::new(self.node.clone()),
                seq: self.seq,
            },
            BalanceId::new(self.balance.clone()),
            kind,
            self.amount,
            Millis(self.at),
        )
        .ok()
    }
}

fn kind_name(kind: EntryKind) -> String {
    match kind {
        EntryKind::Credit => "credit".to_owned(),
        EntryKind::Debit => "debit".to_owned(),
        EntryKind::Adjustment(Sign::Up) => "adjustment_up".to_owned(),
        EntryKind::Adjustment(Sign::Down) => "adjustment_down".to_owned(),
    }
}

fn parse_kind(text: &str) -> Option<EntryKind> {
    match text {
        "credit" => Some(EntryKind::Credit),
        "debit" => Some(EntryKind::Debit),
        "adjustment_up" => Some(EntryKind::Adjustment(Sign::Up)),
        "adjustment_down" => Some(EntryKind::Adjustment(Sign::Down)),
        _ => None,
    }
}

/// The record name of one entry's quarantine item: unique per `(node, entry id)`.
#[must_use]
pub fn record_name(entry: &Entry) -> String {
    format!("{ENTRY_NAME_PREFIX}{}", entry.id)
}

/// The source a refused entry is filed under: the node that wrote it.
fn source_of(entry: &Entry) -> Option<SourceId> {
    SourceId::new(entry.id.node.0.as_str()).ok()
}

/// Packs `entry`, refused for `reason`, into the record image the quarantine queue stores.
///
/// # Errors
/// [`QuarantineError::Store`] when the node id is not a valid source or the entry cannot be
/// encoded. Nothing has been stored at that point, so the caller keeps the entry and retries.
pub(crate) fn record_of(
    entry: &Entry,
    reason: RejectReason,
) -> Result<(SourceId, ReceivedRecord), QuarantineError> {
    let failed = || QuarantineError::Store {
        why: "a refused entry could not be packed for the quarantine queue".to_owned(),
    };
    let source = source_of(entry).ok_or_else(failed)?;
    let name = RecordName::new(&record_name(entry)).map_err(|_| failed())?;
    let entity = Name::new(ENTRY_ENTITY).map_err(|_| failed())?;
    let mut fields = Fields::new();
    for (field, value) in [
        (ENTRY_FIELD, entry_body(entry)?),
        (REASON_FIELD, Value::Text(reason.code().to_owned())),
        (NODE_FIELD, Value::Text(entry.id.node.0.clone())),
    ] {
        let name = Name::new(field).map_err(|_| failed())?;
        fields.insert(name, value);
    }
    let record = ReceivedRecord {
        name,
        version: ENTRY_VERSION,
        seq: entry.id.seq,
        source: source.clone(),
        topic: None,
        conversation: None,
        meta_version: 0,
        document: DocumentData {
            entity,
            id: RecordId::new_v7(),
            fields,
            children: BTreeMap::new(),
        },
        entries: Vec::new(),
    };
    Ok((source, record))
}

fn entry_body(entry: &Entry) -> Result<Value, QuarantineError> {
    serde_json::to_string(&EntryPayload::of(entry))
        .map(Value::Json)
        .map_err(|_| QuarantineError::Store {
            why: "a refused entry could not be encoded".to_owned(),
        })
}

/// The entry a stored quarantine record carries, when it is one of this adapter's items.
///
/// This is what an operator screen or a reconciliation reads: the payload of a held entry is
/// never rewritten, so the value it returns is the value that arrived.
#[must_use]
pub fn entry_of(record: &ReceivedRecord) -> Option<Entry> {
    if record.document.entity.as_str() != ENTRY_ENTITY {
        return None;
    }
    let field = Name::new(ENTRY_FIELD).ok()?;
    let Value::Json(text) = record.document.fields.get(&field)? else {
        return None;
    };
    serde_json::from_str::<EntryPayload>(text).ok()?.to_entry()
}
