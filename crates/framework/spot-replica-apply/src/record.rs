//! The received record: the unit the apply path stores, and the origin fields that mark a mirror.

use spot_storage_contract::{DocumentData, Fields, Name, Value};
use spot_sync_protocol::{Record, RecordName, SourceId};

use crate::error::Reason;
use crate::wire::{DocPayload, RecordPayload};

/// The names of the fields a mirrored record carries its origin in (ADR-058 rule 5: a record
/// stored by a node that does not own it carries its source tenant, source id and source
/// version).
///
/// The exact field names are metadata work (ADR-063's `origin_node`/`owner` system fields); they
/// are configuration here so this crate never guesses a schema. The default is the ADR-058
/// reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OriginFields {
    /// The field holding the source node (the sender's [`SourceId`]).
    pub node: String,
    /// The field holding the source record id.
    pub id: String,
    /// The field holding the source version.
    pub version: String,
}

impl Default for OriginFields {
    fn default() -> Self {
        Self {
            node: "origin_node".to_owned(),
            id: "origin_id".to_owned(),
            version: "origin_version".to_owned(),
        }
    }
}

impl OriginFields {
    /// Origin fields with explicit names.
    #[must_use]
    pub fn new(node: &str, id: &str, version: &str) -> Self {
        Self {
            node: node.to_owned(),
            id: id.to_owned(),
            version: version.to_owned(),
        }
    }

    /// The three names as validated storage names.
    ///
    /// # Errors
    /// [`Reason::InvalidOriginField`] when a configured name is not a storage name.
    pub fn names(&self) -> Result<(Name, Name, Name), Reason> {
        let one = |field: &str| {
            Name::new(field).map_err(|_| Reason::InvalidOriginField {
                field: field.to_owned(),
            })
        };
        Ok((one(&self.node)?, one(&self.id)?, one(&self.version)?))
    }
}

/// One record version to apply: its apply key, the stream position it came from, the sender, and
/// the document image with its entries.
///
/// The apply key is `(name, version)` (ADR-023, ADR-062 G4): the same pair applied twice changes
/// nothing the second time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedRecord {
    /// The apply key's name part.
    pub name: RecordName,
    /// The apply key's version part; at least 1.
    pub version: u64,
    /// The record's position in its source's stream; at least 1.
    pub seq: u64,
    /// The node that sent it.
    pub source: SourceId,
    /// The exchange topic it arrived on, when it is a tenant-link topic (ADR-058).
    pub topic: Option<Name>,
    /// The conversation it belongs to, for a message topic.
    pub conversation: Option<String>,
    /// The sender's metadata version (ADR-066).
    pub meta_version: u64,
    /// The root document at this version.
    pub document: DocumentData,
    /// The ledger and stock entries that came with it.
    pub entries: Vec<DocumentData>,
}

impl ReceivedRecord {
    /// Builds a record from the sync unit and its payload text.
    ///
    /// # Errors
    /// [`Reason::InvalidPayload`] when the payload is not a record image, or [`Reason::InvalidId`]
    /// for a bad record id inside it.
    pub fn from_payload(
        source: &SourceId,
        name: RecordName,
        version: u64,
        seq: u64,
        payload: &str,
    ) -> Result<Self, Reason> {
        let decoded = RecordPayload::decode(payload)?;
        let document = decoded.document.to_document()?;
        let mut entries = Vec::with_capacity(decoded.entries.len());
        for entry in &decoded.entries {
            entries.push(entry.to_document()?);
        }
        Ok(Self {
            name,
            version,
            seq,
            source: source.clone(),
            topic: None,
            conversation: None,
            meta_version: decoded.meta_version,
            document,
            entries,
        })
    }

    /// Builds a record from one sync [`Record`] (its name, version, sequence and payload).
    ///
    /// # Errors
    /// As [`from_payload`](ReceivedRecord::from_payload).
    pub fn from_record(source: &SourceId, record: &Record) -> Result<Self, Reason> {
        Self::from_payload(
            source,
            record.name.clone(),
            record.version,
            record.seq,
            &record.payload,
        )
    }

    /// The record image as a payload.
    #[must_use]
    pub fn to_payload(&self) -> RecordPayload {
        RecordPayload {
            document: DocPayload::from_document(&self.document),
            entries: self.entries.iter().map(DocPayload::from_document).collect(),
            meta_version: self.meta_version,
        }
    }

    /// The record image as the JSON text a sync record carries.
    ///
    /// # Errors
    /// [`Reason::InvalidPayload`] when serialisation fails.
    pub fn payload_json(&self) -> Result<String, Reason> {
        self.to_payload().encode()
    }

    /// The record's apply key as text (`name@version`), for messages and the audit marker.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// Every document in the bundle: the root first, then the entries.
    pub fn documents(&self) -> impl Iterator<Item = &DocumentData> {
        std::iter::once(&self.document).chain(self.entries.iter())
    }

    /// The origin values a mirror carries: the sender, the source id and the source version.
    #[must_use]
    pub fn origin_values(&self) -> (String, String, i64) {
        (
            self.source.as_str().to_owned(),
            self.document.id.to_string(),
            i64::try_from(self.version).unwrap_or(i64::MAX),
        )
    }
}

/// Writes the origin fields into `fields`, so a record stored by a non-owner is marked a mirror
/// (ADR-058 rule 5, ADR-063).
///
/// # Errors
/// [`Reason::InvalidOriginField`] when a configured name is not a storage name.
pub fn mark_origin(
    fields: &mut Fields,
    origin: &OriginFields,
    record: &ReceivedRecord,
) -> Result<(), Reason> {
    let (node, id, version) = origin.names()?;
    let (source, source_id, source_version) = record.origin_values();
    fields.insert(node, Value::Text(source));
    fields.insert(id, Value::Text(source_id));
    fields.insert(version, Value::Int(source_version));
    Ok(())
}

/// The version a mirror holds, read from its origin field, or `None` when it has none.
#[must_use]
pub fn held_version(fields: &Fields, origin: &OriginFields) -> Option<u64> {
    let name = Name::new(&origin.version).ok()?;
    match fields.get(&name) {
        Some(Value::Int(v)) => u64::try_from(*v).ok(),
        _ => None,
    }
}

/// Copies a document with `fields` replaced.
#[must_use]
pub fn with_fields(doc: &DocumentData, fields: Fields) -> DocumentData {
    DocumentData {
        entity: doc.entity.clone(),
        id: doc.id,
        fields,
        children: doc.children.clone(),
    }
}
