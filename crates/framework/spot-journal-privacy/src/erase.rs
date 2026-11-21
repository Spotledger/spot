//! Erasing a data subject: a permission check, the retention classes that forbid erasing, the
//! key destroyed, an audit record that carries a salted hash and never the subject.

use std::sync::Arc;

use spot_journal_pii::{SubjectKeys, erase};
use spot_journal_protect::SharedKeys;

use crate::decl::Declared;
use crate::error::PrivacyError;
use crate::source::CatalogueFields;

/// The permission that allows erasing a subject (held by the data-protection role; default deny).
pub const ERASE_PERMISSION: &str = "journal.erase_subject";

/// Whether a requester may erase a subject.
pub trait ErasePermission: Send + Sync {
    /// `true` only for a requester holding [`ERASE_PERMISSION`].
    fn may_erase(&self, requester: &str) -> bool;
}

/// The salted hash of a subject, as an audit record carries it.
pub trait SubjectHasher: Send + Sync {
    /// Lower-case hex of the tenant-salted hash of `subject`.
    fn hash(&self, subject: &str) -> String;
}

/// What the audit record (`sys_journal_admin`) says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminAction {
    /// `subject_erased`: the key was destroyed.
    SubjectErased {
        /// How many fields stayed readable (legal retention).
        retained_fields: usize,
    },
    /// `subject_erase_refused`: nothing was destroyed.
    SubjectEraseRefused {
        /// The refusal's code.
        code: &'static str,
    },
}

/// Writes `sys_journal_admin` records.
pub trait AdminAudit: Send + Sync {
    /// Records `action` on the subject whose salted hash is `subject_hash`, by `requester`.
    ///
    /// # Errors
    /// A fixed sentence when the record cannot be written; the erase is then not performed.
    fn record(
        &self,
        action: &AdminAction,
        subject_hash: &str,
        requester: &str,
    ) -> Result<(), &'static str>;
}

/// A field erasure did not reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retained {
    /// The entity.
    pub entity: String,
    /// The field path (never a value).
    pub path: String,
}

/// What an erase did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErasureOutcome {
    /// The salted hash of the subject (never the subject).
    pub subject_hash: String,
    /// Fields erasure reached.
    pub erased_fields: usize,
    /// Fields it did not (held under a retention that forbids erasing). Non-empty means a
    /// partial erase, explicitly.
    pub retained: Vec<Retained>,
}

impl ErasureOutcome {
    /// Whether some fields stayed readable.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.retained.is_empty()
    }
}

/// Erases subjects of one site.
pub struct Eraser {
    fields: Arc<CatalogueFields>,
    keys: SharedKeys,
    permission: Arc<dyn ErasePermission>,
    hasher: Arc<dyn SubjectHasher>,
    audit: Arc<dyn AdminAudit>,
}

impl Eraser {
    /// An eraser over the site's declared fields and subject keys.
    #[must_use]
    pub fn new(
        fields: Arc<CatalogueFields>,
        keys: SharedKeys,
        permission: Arc<dyn ErasePermission>,
        hasher: Arc<dyn SubjectHasher>,
        audit: Arc<dyn AdminAudit>,
    ) -> Self {
        Self {
            fields,
            keys,
            permission,
            hasher,
            audit,
        }
    }

    /// Erases `subject` on behalf of `requester`.
    ///
    /// A requester without the permission is refused (and the refusal audited); a subject whose
    /// every declared field is retained is refused naming the first entity and field; otherwise
    /// the key is destroyed (idempotent), the audit record is written, and the outcome says what
    /// stayed readable.
    ///
    /// # Errors
    /// [`PrivacyError::PermissionDenied`], [`PrivacyError::RetentionForbids`],
    /// [`PrivacyError::SubjectInvalid`] and [`PrivacyError::KeyStore`] (nothing is claimed as
    /// erased then).
    pub fn erase_subject(
        &self,
        subject: &str,
        requester: &str,
    ) -> Result<ErasureOutcome, PrivacyError> {
        if subject.is_empty() {
            return Err(PrivacyError::SubjectInvalid);
        }
        let subject_hash = self.hasher.hash(subject);
        if !self.permission.may_erase(requester) {
            self.refused(
                "journal_privacy.permission_denied",
                &subject_hash,
                requester,
            );
            return Err(PrivacyError::PermissionDenied);
        }
        let declared = self.fields.declared();
        let (erasable, kept): (Vec<&Declared>, Vec<&Declared>) =
            declared.iter().partition(|field| field.erasable);
        let retained: Vec<Retained> = kept
            .iter()
            .map(|field| Retained {
                entity: field.entity.clone(),
                path: field.path.clone(),
            })
            .collect();
        if erasable.is_empty() && !retained.is_empty() {
            self.refused(
                "journal_privacy.retention_forbids",
                &subject_hash,
                requester,
            );
            return Err(PrivacyError::RetentionForbids {
                entity: retained[0].entity.clone(),
                path: retained[0].path.clone(),
            });
        }
        // The audit record comes first: an erase that cannot be audited is not performed.
        self.audit
            .record(
                &AdminAction::SubjectErased {
                    retained_fields: retained.len(),
                },
                &subject_hash,
                requester,
            )
            .map_err(PrivacyError::KeyStore)?;
        let keys: &dyn SubjectKeys = &*self.keys;
        erase(subject, keys, None)
            .map_err(|_| PrivacyError::KeyStore("the subject key could not be destroyed"))?;
        Ok(ErasureOutcome {
            subject_hash,
            erased_fields: erasable.len(),
            retained,
        })
    }

    fn refused(&self, code: &'static str, subject_hash: &str, requester: &str) {
        // A refusal that cannot be audited is still a refusal.
        let _ = self.audit.record(
            &AdminAction::SubjectEraseRefused { code },
            subject_hash,
            requester,
        );
    }
}
