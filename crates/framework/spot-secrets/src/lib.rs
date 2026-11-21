//! Envelope encryption for secrets stored in site configuration and settings documents.
//!
//! [`SecretBox`] seals a [`SecretString`] under a fresh data key; a [`KeyProvider`] wraps that
//! data key under a versioned key-encryption key. Only the [`Sealed`] form is stored.

mod blindindex;
mod crypto;
mod error;
mod kdf;
mod keyring;
mod local;
mod provider;
mod sealed;
mod secret;
mod secretbox;

pub use blindindex::{BlindIndex, INDEX_LEN, RESERVED_INFIX, check_scope};
pub use error::SecretError;
pub use kdf::{
    DERIVE_PREFIX, INDEX_KEY_INFO, MAX_DERIVE_INFO_LEN, MAX_DERIVED_LEN, MIN_DERIVED_LEN,
};
pub use keyring::MemoryKeyring;
pub use local::LocalKeyring;
pub use provider::{EnvKey, KeyProvider, KeyRef, ProviderFuture, WrappedKey};
pub use sealed::Sealed;
pub use secret::{SecretBytes, SecretString};
pub use secretbox::SecretBox;
