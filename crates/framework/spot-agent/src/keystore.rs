//! The agent identity key store: the [`KeyStore`] trait of `spot-agent-enroll`
//! (`FileKeyStore`, Ed25519, 0600, atomic write, fsync) plus the TPM stub.
//!
//! The private key is never printed, logged or sent: only the public key and
//! signatures leave a store.

use std::path::Path;

use spot_agent_enroll::keys::{FileKeyStore, KeyStore, Protection, PublicKey};

use crate::error::AgentError;

/// Which store holds the identity key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyStoreKind {
    /// A software key file.
    File,
    /// A TPM-held key: not implemented yet.
    Tpm,
}

impl KeyStoreKind {
    /// Parses `file` or `tpm`.
    ///
    /// # Errors
    /// [`AgentError::Usage`] for anything else.
    pub fn parse(text: &str) -> Result<Self, AgentError> {
        match text {
            "file" => Ok(Self::File),
            "tpm" => Ok(Self::Tpm),
            _ => Err(AgentError::Usage(
                "key store must be `file` or `tpm`".to_owned(),
            )),
        }
    }
}

/// The TPM key store stub: every operation reports `agent.key_store_unavailable`. The real
/// TPM wiring is a later task.
#[derive(Debug, Default)]
pub struct TpmKeyStore;

impl TpmKeyStore {
    /// Opens the TPM store.
    ///
    /// # Errors
    /// Always [`AgentError::KeyStoreUnavailable`].
    pub fn open() -> Result<Box<dyn KeyStore>, AgentError> {
        Err(AgentError::KeyStoreUnavailable)
    }
}

/// Opens (creating on first use) the key store of `kind` at `key_path`.
///
/// # Errors
/// [`AgentError::KeyStoreUnavailable`] for the TPM stub; [`AgentError::StatePermissions`]
/// when the key file is accessible beyond its owner; [`AgentError::StateIo`] otherwise.
pub fn open_key_store(
    kind: KeyStoreKind,
    key_path: &Path,
) -> Result<Box<dyn KeyStore>, AgentError> {
    match kind {
        KeyStoreKind::Tpm => TpmKeyStore::open(),
        KeyStoreKind::File => {
            use spot_agent_enroll::error::EnrolError;
            match FileKeyStore::open_or_create(key_path) {
                Ok(store) => Ok(Box::new(store)),
                Err(EnrolError::KeyFilePermissions) => Err(AgentError::StatePermissions(
                    "the key file must be mode 0600".to_owned(),
                )),
                Err(_) => Err(AgentError::StateIo("cannot open the key file".to_owned())),
            }
        }
    }
}

/// The public key and protection of a store, for display.
#[must_use]
pub fn describe(store: &dyn KeyStore) -> (PublicKey, Protection) {
    (store.public_key(), store.protection())
}
