//! The relay-side transform: seal the personal fields of every state image before a line reaches
//! the relay, so no segment or tail chunk ever holds one in clear.

use serde_json::Value;
use spot_events_contract::envelope::EventEnvelope;
use spot_journal_rebuild::{EventClass, classify};
use spot_journal_relay::{Appended, Batch, Relay, RelayError};
use spot_journal_store::JournalStore;

use crate::error::{AppendError, ProtectError};
use crate::image::{Identity, SharedFields, SharedKeys, SharedNonces, protect_image};

/// Seals the personal fields of the state images in relay lines.
///
/// One transform serves one tenant: its [`SharedKeys`] are that tenant's subject keys and its
/// tenant journal key, so an envelope it writes cannot be opened with another tenant's keys.
///
/// Only **state** entries (topic `doc_state`) are transformed. Audit and domain lines are passed
/// through as the very same bytes: see the crate documentation for why a domain event is the
/// declarer's responsibility.
#[derive(Clone)]
pub struct ProtectTransform {
    fields: SharedFields,
    keys: SharedKeys,
    nonces: SharedNonces,
}

impl ProtectTransform {
    /// A transform over `fields`, the tenant's `keys` and a `nonces` source.
    #[must_use]
    pub fn new(fields: SharedFields, keys: SharedKeys, nonces: SharedNonces) -> Self {
        Self {
            fields,
            keys,
            nonces,
        }
    }

    /// The line to write for `line`: the same bytes for a line that is not a state entry or whose
    /// entity has no personal fields, and otherwise the envelope with its image protected.
    ///
    /// # Errors
    /// [`ProtectError`], and a line that is not an event envelope is one: whether it is a state
    /// entry cannot be told, so it is not written.
    pub fn transform_line(&self, line: &[u8]) -> Result<Vec<u8>, ProtectError> {
        let text = std::str::from_utf8(line)
            .map_err(|_| ProtectError::EntryInvalid("a line is not valid UTF-8"))?;
        let mut envelope = EventEnvelope::from_json(text)
            .map_err(|_| ProtectError::EntryInvalid("a line is not an event envelope"))?;
        if classify(envelope.topic.as_str()) != EventClass::State {
            return Ok(line.to_vec());
        }
        let entity = envelope.subject.entity.clone();
        let id = envelope.subject.id.to_string();
        let version = envelope.subject_version;
        let Value::Object(data) = &mut envelope.data else {
            return Err(ProtectError::EntryInvalid(
                "a state entry's data is not an object",
            ));
        };
        let sealed = match data.get("image") {
            None | Some(Value::Null) => return Ok(line.to_vec()),
            Some(image) => {
                let version = version.ok_or(ProtectError::EntryInvalid(
                    "a state entry has no subject version",
                ))?;
                let identity = Identity {
                    entity: &entity,
                    id: &id,
                    version,
                };
                protect_image(&self.fields, &self.keys, &self.nonces, &identity, image)?
            }
        };
        let Some(sealed) = sealed else {
            return Ok(line.to_vec());
        };
        data.insert("image".to_owned(), sealed);
        envelope
            .to_json()
            .map(String::into_bytes)
            .map_err(|_| ProtectError::EntryInvalid("the protected entry cannot be written"))
    }

    /// `batch` with every line transformed: all of it or none of it.
    ///
    /// # Errors
    /// The first [`ProtectError`]; no part of the batch is returned then.
    pub fn transform_batch(&self, batch: Batch) -> Result<Batch, ProtectError> {
        let mut entries = Vec::with_capacity(batch.entries.len());
        for (position, line) in batch.entries {
            entries.push((position, self.transform_line(&line)?));
        }
        Ok(Batch {
            batch_id: batch.batch_id,
            entries,
        })
    }

    /// Protects `batch` and hands it to `relay`. The relay is called only with a fully protected
    /// batch: when protecting any line fails the call returns [`AppendError::Protect`] and the
    /// relay has not been touched, so nothing is written for the batch.
    ///
    /// # Errors
    /// [`AppendError::Protect`] or [`AppendError::Relay`].
    pub async fn append_batch<S: JournalStore>(
        &self,
        relay: &mut Relay<S>,
        batch: Batch,
        now_millis: i64,
    ) -> Result<Appended, AppendError> {
        let protected = self.transform_batch(batch).map_err(AppendError::Protect)?;
        relay
            .append_batch(protected, now_millis)
            .await
            .map_err(|error: RelayError| AppendError::Relay(error))
    }
}
