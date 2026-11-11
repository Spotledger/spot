//! The envelope of a record topic (ADR-058 rule 5): a record version in the replication wire
//! image, so the receiver applies it through the one apply path of `spot-replica-apply`.
//!
//! This crate does not re-model records: the envelope wraps `RecordPayload`, and the receiver's
//! side turns it into a `ReceivedRecord`.

use serde::{Deserialize, Serialize};
use spot_replica_apply::{ReceivedRecord, RecordPayload, WireFields};
use spot_storage_contract::Name;
use spot_sync_protocol::{RecordName, SourceId};
use spot_tenant_link::{FieldList, FieldName, Topic};

use crate::canonical;
use crate::error::{Error, Result};
use crate::ids::Conversation;

/// One record version on a record topic. `Debug` shows the name and version, never the image.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// The record name (the apply key with the version).
    pub name: RecordName,
    /// The record version.
    pub version: u64,
    /// The record image.
    pub record: RecordPayload,
}

impl std::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Envelope")
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Envelope {
    /// The canonical JSON a message carries.
    ///
    /// # Errors
    /// [`Error::InvalidPayload`] when the image cannot be encoded canonically.
    pub fn encode(&self) -> Result<String> {
        let text = serde_json::to_string(self).map_err(|_| Error::InvalidPayload {
            reason: "the record envelope cannot be encoded",
        })?;
        canonical::canonicalize(&text)
    }

    /// Decodes a message payload.
    ///
    /// # Errors
    /// [`Error::InvalidPayload`] when the payload is not an envelope.
    pub fn decode(payload: &str) -> Result<Self> {
        serde_json::from_str(payload).map_err(|_| Error::InvalidPayload {
            reason: "the payload is not a record envelope",
        })
    }

    /// Checks that the image decodes to storage documents.
    ///
    /// # Errors
    /// [`Error::InvalidPayload`] when a document, entry or child row is malformed.
    pub fn validate(&self) -> Result<()> {
        for doc in std::iter::once(&self.record.document).chain(self.record.entries.iter()) {
            doc.to_document().map_err(|_| Error::InvalidPayload {
                reason: "the record image is not valid",
            })?;
        }
        Ok(())
    }

    /// Whether every field the record carries (root, entries, child rows) is permitted by the
    /// grant's field list. A field whose name is not a valid field name is not permitted.
    #[must_use]
    pub fn within(&self, fields: &FieldList) -> bool {
        let permitted = |map: &WireFields| {
            map.keys()
                .all(|name| FieldName::new(name.as_str()).is_ok_and(|n| fields.includes(&n)))
        };
        std::iter::once(&self.record.document)
            .chain(self.record.entries.iter())
            .all(|doc| {
                permitted(&doc.fields)
                    && doc
                        .children
                        .values()
                        .all(|rows| rows.iter().all(|row| permitted(&row.fields)))
            })
    }

    /// The envelope with every field outside `fields` removed, and the names removed.
    #[must_use]
    pub fn filtered(mut self, fields: &FieldList) -> (Self, Vec<String>) {
        let mut withheld = Vec::new();
        let mut keep = |map: &mut WireFields| {
            map.retain(|name, _| {
                let ok = FieldName::new(name.as_str()).is_ok_and(|n| fields.includes(&n));
                if !ok {
                    withheld.push(name.clone());
                }
                ok
            });
        };
        let mut docs = std::iter::once(&mut self.record.document)
            .chain(self.record.entries.iter_mut())
            .collect::<Vec<_>>();
        for doc in &mut docs {
            keep(&mut doc.fields);
            for rows in doc.children.values_mut() {
                for row in rows {
                    keep(&mut row.fields);
                }
            }
        }
        (self, withheld)
    }

    /// The record the apply path of `spot-replica-apply` takes.
    ///
    /// # Errors
    /// [`Error::InvalidPayload`] when the image is not a valid record image.
    pub fn into_received(
        self,
        source: &SourceId,
        seq: u64,
        topic: &Topic,
        conversation: &Conversation,
    ) -> Result<ReceivedRecord> {
        let text = self.record.encode().map_err(|_| Error::InvalidPayload {
            reason: "the record image cannot be encoded",
        })?;
        let mut record = ReceivedRecord::from_payload(source, self.name, self.version, seq, &text)
            .map_err(|_| Error::InvalidPayload {
                reason: "the record image is not valid",
            })?;
        record.topic = Name::new(topic.as_str()).ok();
        record.conversation = Some(conversation.as_str().to_owned());
        Ok(record)
    }
}
