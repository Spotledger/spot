//! The administrator trust anchor returned at enrolment: the only keys whose command
//! signatures this agent accepts. Stored in `trust.toml`; never replaced from a gateway
//! message after enrolment.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use spot_agent_core::TrustStore;
use spot_agent_enroll::keys::Protection;

use crate::error::AgentError;
use crate::state::atomic_write;

/// One trusted key in `trust.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustEntry {
    /// Standard base64 of the 32-byte Ed25519 public key.
    pub public_key: String,
    /// `software` or `hardware`.
    pub protection: String,
}

/// The `trust.toml` file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustFile {
    /// The trusted administrator keys.
    pub admin_keys: Vec<TrustEntry>,
}

/// Parses the contract's `trust_anchor` text: one or more base64 Ed25519 public keys
/// separated by whitespace or commas (assumption recorded in the task report).
///
/// # Errors
/// [`AgentError::GatewayProtocol`] when empty or any key is not a 32-byte valid key.
pub fn parse_trust_anchor(text: &str) -> Result<Vec<[u8; 32]>, AgentError> {
    let bad = || AgentError::GatewayProtocol("the trust anchor is malformed".to_owned());
    let mut keys = Vec::new();
    for part in text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|p| !p.is_empty())
    {
        let raw = STANDARD.decode(part).map_err(|_| bad())?;
        let key: [u8; 32] = raw.try_into().map_err(|_| bad())?;
        ed25519_dalek::VerifyingKey::from_bytes(&key).map_err(|_| bad())?;
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if keys.is_empty() || keys.len() > 16 {
        return Err(bad());
    }
    Ok(keys)
}

impl TrustFile {
    /// Builds the file from keys (software protection: the enrolment does not attest).
    #[must_use]
    pub fn from_keys(keys: &[[u8; 32]]) -> Self {
        Self {
            admin_keys: keys
                .iter()
                .map(|k| TrustEntry {
                    public_key: STANDARD.encode(k),
                    protection: "software".to_owned(),
                })
                .collect(),
        }
    }

    /// Writes the file atomically, mode 0600.
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn save(&self, path: &std::path::Path) -> Result<(), AgentError> {
        let text =
            toml::to_string(self).map_err(|_| AgentError::StateIo("trust.toml".to_owned()))?;
        atomic_write(path, text.as_bytes())
    }

    /// Loads `trust.toml`.
    ///
    /// # Errors
    /// [`AgentError::NotEnrolled`] when missing, [`AgentError::ConfigInvalid`] when malformed.
    pub fn load(path: &std::path::Path) -> Result<Self, AgentError> {
        let text = std::fs::read_to_string(path).map_err(|_| AgentError::NotEnrolled)?;
        toml::from_str(&text)
            .map_err(|_| AgentError::ConfigInvalid("trust.toml is malformed".to_owned()))
    }

    /// The trust store the decision core verifies against.
    ///
    /// # Errors
    /// [`AgentError::ConfigInvalid`] when a key is invalid.
    pub fn to_store(&self) -> Result<TrustStore, AgentError> {
        let bad = || AgentError::ConfigInvalid("trust.toml holds an invalid key".to_owned());
        let mut store = TrustStore::new();
        for entry in &self.admin_keys {
            let raw = STANDARD.decode(&entry.public_key).map_err(|_| bad())?;
            let key: [u8; 32] = raw.try_into().map_err(|_| bad())?;
            let protection = if entry.protection == "hardware" {
                Protection::Hardware
            } else {
                Protection::Software
            };
            store.insert(&key, protection).map_err(|_| bad())?;
        }
        Ok(store)
    }
}
