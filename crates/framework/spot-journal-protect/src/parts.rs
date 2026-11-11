//! The snapshot-side transform: protect the image records a snapshot writer is given, and reveal
//! the ones its reader returns.

use spot_journal_snapshot::ImageRecord;

use crate::error::ProtectError;
use crate::image::{Identity, SharedFields, SharedKeys, SharedNonces, protect_image, reveal_image};

/// Protects and reveals snapshot [`ImageRecord`]s.
///
/// The snapshot writer takes records "already protected": the database read that feeds it holds
/// clear values, so every record goes through [`ProtectedParts::protect_record`] (or
/// [`ProtectedParts::protect_records`]) before `SnapshotWriter::write_part`. Records the reader
/// returns hold envelopes and go through [`ProtectedParts::reveal_record`] before they are used
/// directly; a rebuild reveals them in its [`crate::RevealSink`] instead.
#[derive(Clone)]
pub struct ProtectedParts {
    fields: SharedFields,
    keys: SharedKeys,
    nonces: SharedNonces,
}

impl ProtectedParts {
    /// A transform over `fields`, the tenant's `keys` and a `nonces` source.
    #[must_use]
    pub fn new(fields: SharedFields, keys: SharedKeys, nonces: SharedNonces) -> Self {
        Self {
            fields,
            keys,
            nonces,
        }
    }

    /// `record` with its personal fields sealed; the record is returned as it is when its entity
    /// has none.
    ///
    /// # Errors
    /// [`ProtectError`]; the record is not to be written then.
    pub fn protect_record(&self, record: ImageRecord) -> Result<ImageRecord, ProtectError> {
        let identity = Identity {
            entity: &record.entity,
            id: &record.id,
            version: record.version,
        };
        let sealed = protect_image(
            &self.fields,
            &self.keys,
            &self.nonces,
            &identity,
            &record.image,
        )?;
        Ok(match sealed {
            Some(image) => ImageRecord { image, ..record },
            None => record,
        })
    }

    /// Every record protected, in order: all of them or none of them.
    ///
    /// # Errors
    /// The first [`ProtectError`].
    pub fn protect_records(
        &self,
        records: impl IntoIterator<Item = ImageRecord>,
    ) -> Result<Vec<ImageRecord>, ProtectError> {
        records
            .into_iter()
            .map(|record| self.protect_record(record))
            .collect()
    }

    /// `record` with its personal fields opened, or replaced by the placeholder once their
    /// subject's key is destroyed.
    ///
    /// # Errors
    /// [`ProtectError::Reveal`] on a key store outage or a tampered envelope.
    pub fn reveal_record(&self, record: ImageRecord) -> Result<ImageRecord, ProtectError> {
        let identity = Identity {
            entity: &record.entity,
            id: &record.id,
            version: record.version,
        };
        let opened = reveal_image(&self.fields, &self.keys, &identity, &record.image)?;
        Ok(match opened {
            Some(image) => ImageRecord { image, ..record },
            None => record,
        })
    }
}
