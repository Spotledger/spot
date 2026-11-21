//! What the shipper is given: the protect transform over the declared fields and the subject
//! keys, or a protector that refuses every line when protection cannot be built.

use std::sync::Arc;

use spot_journal_protect::{
    ProtectTransform, ProtectedParts, SharedFields, SharedKeys, SharedNonces,
};
use spot_journal_ship::{Protector, ShipFault};

use crate::error::PrivacyError;
use crate::source::CatalogueFields;

/// The site's privacy machinery: the declared fields, the subject keys and the nonce source.
/// The transform, the snapshot parts and the rebuild's reveal wrapper are all built over the
/// same three, so a field is protected the same way in the journal, in snapshots and on rebuild.
#[derive(Clone)]
pub struct Privacy {
    fields: Arc<CatalogueFields>,
    keys: SharedKeys,
    nonces: SharedNonces,
}

impl Privacy {
    /// Privacy over `fields`, `keys` and `nonces`.
    #[must_use]
    pub fn new(fields: Arc<CatalogueFields>, keys: SharedKeys, nonces: SharedNonces) -> Self {
        Self {
            fields,
            keys,
            nonces,
        }
    }

    /// The declared fields.
    #[must_use]
    pub fn fields(&self) -> &Arc<CatalogueFields> {
        &self.fields
    }

    /// The fields as the protect layer's shared source.
    #[must_use]
    pub fn shared_fields(&self) -> SharedFields {
        Arc::<CatalogueFields>::clone(&self.fields)
    }

    /// The subject keys.
    #[must_use]
    pub fn keys(&self) -> SharedKeys {
        Arc::clone(&self.keys)
    }

    /// The relay-side transform (what the shipper runs every line through).
    #[must_use]
    pub fn transform(&self) -> ProtectTransform {
        ProtectTransform::new(
            self.shared_fields(),
            Arc::clone(&self.keys),
            Arc::clone(&self.nonces),
        )
    }

    /// The snapshot-side transform (T5).
    #[must_use]
    pub fn parts(&self) -> ProtectedParts {
        ProtectedParts::new(
            self.shared_fields(),
            Arc::clone(&self.keys),
            Arc::clone(&self.nonces),
        )
    }

    /// The shipper's protector: the transform.
    #[must_use]
    pub fn protector(&self) -> Arc<dyn Protector> {
        Arc::new(self.transform())
    }
}

/// A protector for a site that declares personal fields but cannot protect them: fail closed,
/// every line is refused with `journal_privacy.protection_missing`, so nothing reaches the
/// journal in clear. Lines of entities without personal fields are refused too, because the
/// shipper writes in order and never skips an entry.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefusingProtector;

impl Protector for RefusingProtector {
    fn protect(&self, _line: &[u8]) -> Result<Vec<u8>, ShipFault> {
        Err(ShipFault::Protect {
            code: PrivacyError::ProtectionMissing.code(),
        })
    }
}
