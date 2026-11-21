//! The persisted agent identity: what the agent keeps after a successful enrolment.
//!
//! [`AgentIdentity`] is written as TOML **next to the agent's key file** (by
//! [`AgentIdentity::save`], atomically, owner-only on Unix) and loaded with
//! [`AgentIdentity::load`], which rejects a file with extra or missing fields: the
//! shape is part of the contract, and an identity file that grew anonymous extra keys
//! must fail loudly rather than be half-trusted.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::EnrolError;
use crate::keys::atomic_write_owner_only;
use crate::message::PublicKeyB64;

/// The agent's durable identity after enrolment.
///
/// `agent_id` is assigned by the Admin Center and is not secret, but it *belongs* to
/// the agent identity as a whole, so the file is treated as sensitive: written
/// owner-only, `Debug`-redacted as a whole.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentity {
    /// The durable agent identity the Admin Center assigned.
    pub agent_id: String,
    /// The pinned server public key, as the base64 text the exchange carried..
    pub server_public_key: PublicKeyB64,
    /// The instant of enrolment (whole seconds since the Unix epoch, UTC).
    pub enrolled_at: i64,
}

impl AgentIdentity {
    /// Serialises to the canonical TOML text of this file format.
    fn to_text(&self) -> Result<String, EnrolError> {
        toml::to_string(self).map_err(|_| EnrolError::IdentityFileIo)
    }

    /// Writes the identity atomically (temp file + rename) next to `key_path`, with
    /// owner-only permissions on Unix. The usual file name is the key file's path
    /// plus a suffix; the caller decides.
    ///
    /// # Errors
    /// [`EnrolError::IdentityFileIo`] when the serialisation or the write fails.
    pub fn save(&self, key_path: impl AsRef<Path>) -> Result<(), EnrolError> {
        let text = self.to_text()?;
        let mut path = key_path.as_ref().as_os_str().to_owned();
        path.push(".identity");
        atomic_write_owner_only(std::path::Path::new(&path), text.as_bytes()).map_err(|error| {
            match error {
                EnrolError::KeyFilePermissions => EnrolError::IdentityFileIo,
                other => other,
            }
        })
    }

    /// Loads and validates the identity written next to `key_path`.
    ///
    /// # Errors
    /// [`EnrolError::IdentityFileIo`] when the file cannot be read;
    /// [`EnrolError::IdentityFileMalformed`] when it does not parse, has extra or
    /// missing fields (serde with `deny_unknown_fields`), or fields of the wrong
    /// shape; [`EnrolError::KeyFilePermissions`] when the file is readable beyond its
    /// owner (Unix).
    pub fn load(key_path: impl AsRef<Path>) -> Result<Self, EnrolError> {
        let mut path = key_path.as_ref().as_os_str().to_owned();
        path.push(".identity");
        let path = std::path::PathBuf::from(path);
        #[cfg(unix)]
        {
            let file = std::fs::File::open(&path).map_err(|_| EnrolError::IdentityFileIo)?;
            let metadata = file.metadata().map_err(|_| EnrolError::IdentityFileIo)?;
            let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions());
            if mode & 0o077 != 0 {
                return Err(EnrolError::KeyFilePermissions);
            }
        }
        let raw = std::fs::read(&path).map_err(|_| EnrolError::IdentityFileIo)?;
        let text = std::str::from_utf8(&raw).map_err(|_| EnrolError::IdentityFileMalformed)?;
        let identity: Self = toml::from_str(text).map_err(|_| EnrolError::IdentityFileMalformed)?;
        identity.validate()?;
        Ok(identity)
    }

    /// The field-level checks beyond deserialisation.
    ///
    /// # Errors
    /// [`EnrolError::IdentityFileMalformed`] when the agent id is empty, or the server
    /// key does not decode.
    fn validate(&self) -> Result<(), EnrolError> {
        if self.agent_id.is_empty() || self.agent_id.len() > 128 {
            return Err(EnrolError::IdentityFileMalformed);
        }
        self.server_public_key
            .verifying_key()
            .map_err(|_| EnrolError::IdentityFileMalformed)?;
        Ok(())
    }

    /// The pinned server key, decoded; a stored file with a bad key text is malformed.
    ///
    /// # Errors
    /// [`EnrolError::IdentityFileMalformed`] when the key does not decode.
    pub fn server_public_key(&self) -> Result<crate::keys::PublicKey, EnrolError> {
        let bytes: [u8; 32] = self
            .server_public_key
            .bytes()
            .map_err(|_| EnrolError::IdentityFileMalformed)?;
        crate::keys::PublicKey::from_bytes(&bytes).map_err(|_| EnrolError::IdentityFileMalformed)
    }
}

impl fmt::Debug for AgentIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentIdentity")
            .field("agent_id", &self.agent_id)
            .field("server_public_key", &"[pinned]")
            .field("enrolled_at", &self.enrolled_at)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::AgentIdentity;
    use crate::error::EnrolError;
    use crate::message::PublicKeyB64;

    fn identity() -> AgentIdentity {
        // A real curve point, as a genuine enrolment would store.
        let server_store =
            crate::keys::MemoryKeyStore::from_seed([4u8; 32], crate::keys::Protection::Software);
        let server_key: PublicKeyB64 = crate::keys::KeyStore::public_key(&server_store).to_b64();
        AgentIdentity {
            agent_id: "agent-0198c7a2".to_owned(),
            server_public_key: server_key,
            enrolled_at: 1_770_000_000,
        }
    }

    #[test]
    fn save_and_load_round_trip_next_to_the_key() {
        let space = tempfile::tempdir().unwrap();
        let key_path = space.path().join("agent.key");
        let identity = identity();
        identity.save(&key_path).unwrap();
        let loaded = AgentIdentity::load(&key_path).unwrap();
        assert_eq!(loaded, identity);
        // And the permissions are owner-only.
        #[cfg(unix)]
        {
            let identity_path = key_path.with_extension("key.identity");
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&identity_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }

    #[test]
    fn load_rejects_extra_and_missing_fields() {
        let space = tempfile::tempdir().unwrap();
        let key_path = space.path().join("agent.key");
        identity().save(&key_path).unwrap();
        let identity_path = key_path.with_extension("key.identity");
        let text = std::fs::read_to_string(&identity_path).unwrap();
        // Extra field.
        let extra = format!("{text}surprise = \"no\"\n");
        std::fs::write(&identity_path, extra).unwrap();
        assert!(matches!(
            AgentIdentity::load(&key_path),
            Err(EnrolError::IdentityFileMalformed)
        ));
        // Missing field.
        let missing: String = text
            .lines()
            .filter(|line| !line.starts_with("enrolled_at"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&identity_path, missing).unwrap();
        assert!(matches!(
            AgentIdentity::load(&key_path),
            Err(EnrolError::IdentityFileMalformed)
        ));
        // Wrong-shaped key: 31 bytes of base64 decode to a non-32-byte value.
        identity().save(&key_path).unwrap();
        let good_key_text = crate::codec::enc64(
            crate::keys::KeyStore::public_key(&crate::keys::MemoryKeyStore::from_seed(
                [4u8; 32],
                crate::keys::Protection::Software,
            ))
            .as_bytes(),
        );
        let bad_key = text.replacen(&good_key_text, &good_key_text[..good_key_text.len() - 4], 1);
        std::fs::write(&identity_path, bad_key).unwrap();
        assert!(matches!(
            AgentIdentity::load(&key_path),
            Err(EnrolError::IdentityFileMalformed)
        ));
    }

    #[test]
    fn debug_hides_the_pinned_key() {
        let identity = identity();
        let rendered = format!("{identity:?}");
        assert!(!rendered.contains(&crate::codec::enc64(&[4u8; 32])));
    }
}
