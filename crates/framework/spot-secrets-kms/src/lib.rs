//! Customer-managed keys for `spot-secrets`.
//!
//! [`KmsKeyProvider`] implements [`spot_secrets::KeyProvider`] with the key-encryption key held
//! in the customer's KMS. The KMS is reached through a [`KmsClient`]; the one implementation
//! here, [`TransitClient`], speaks the HashiCorp Vault Transit API over a
//! [`spot_egress::Sender`], so every call passes the egress policy. See the README for the threat
//! model.

mod cache;
mod client;
mod derive;
mod error;
mod fsutil;
mod provider;
mod seed;
mod transit;

pub use client::{KeyInfo, KmsCiphertext, KmsClient};
pub use error::KmsError;
pub use provider::{KmsKeyProvider, ProviderConfig};
pub use seed::{Entropy, OsEntropy};
pub use transit::{EgressGate, GuardGate, TransitClient, TransitConfig};
