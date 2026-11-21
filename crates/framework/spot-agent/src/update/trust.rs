//! The update trust anchor and the persisted revocation list.
//!
//! The anchor is a file the package installs next to the binary (never fetched from the
//! gateway): JSON, `{"keys": [{"public_key": "<base64, 32 bytes>", "valid_from": 0,
//! "valid_to": null}]}`. An anchor with no key is refused: it would trust nothing and hide a
//! broken install behind "every update is refused".
//!
//! The revocation list lives in the state directory as JSON (the shape of
//! [`wire::revocation_to_json`](super::wire::revocation_to_json)) and is replaced only by a
//! list whose issuer is a trusted key, valid now, with a verifying signature and a strictly
//! higher generation, so a replayed older list can never lower what the agent holds.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use spot_updater::{PublicKey, TrustedUpdateKey, UpdateRevocation, UpdateTrust};

use super::error::UpdateError;
use super::wire::{revocation_from_json, revocation_to_json};

/// Largest anchor or revocation file read, in bytes.
const MAX_FILE: u64 = 256 * 1024;

fn trust_err(why: &str) -> UpdateError {
    UpdateError::Trust(why.to_owned())
}

/// Reads the trust anchor file at `path`.
///
/// # Errors
/// [`UpdateError::Trust`] when the file is missing, too large, malformed or names no key.
pub fn load_anchor(path: &Path) -> Result<UpdateTrust, UpdateError> {
    let text = read_capped(path)?.ok_or_else(|| trust_err("the trust anchor file is missing"))?;
    parse_anchor(&text)
}

/// Parses the anchor text. See the module docs for the shape.
///
/// # Errors
/// [`UpdateError::Trust`] for malformed JSON, a key that is not 32 bytes of base64 or not a
/// curve point, a window that is not integers, or no key at all.
pub fn parse_anchor(text: &str) -> Result<UpdateTrust, UpdateError> {
    let value: Value =
        serde_json::from_str(text).map_err(|_| trust_err("the trust anchor is not JSON"))?;
    let listed = value
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| trust_err("the trust anchor has no `keys` array"))?;
    if listed.is_empty() || listed.len() > 64 {
        return Err(trust_err(
            "the trust anchor must list between 1 and 64 keys",
        ));
    }
    let mut keys = Vec::with_capacity(listed.len());
    for entry in listed {
        let encoded = entry
            .get("public_key")
            .and_then(Value::as_str)
            .ok_or_else(|| trust_err("a trust anchor key has no `public_key`"))?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| trust_err("a trust anchor key is not base64"))?;
        let public = PublicKey::from_bytes(&bytes)
            .map_err(|_| trust_err("a trust anchor key is not a valid public key"))?;
        let valid_from = match entry.get("valid_from") {
            None | Some(Value::Null) => 0,
            Some(v) => v
                .as_i64()
                .ok_or_else(|| trust_err("`valid_from` is an integer"))?,
        };
        let valid_to = match entry.get("valid_to") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_i64()
                    .ok_or_else(|| trust_err("`valid_to` is an integer"))?,
            ),
        };
        keys.push(TrustedUpdateKey::new(public, valid_from, valid_to));
    }
    Ok(UpdateTrust::new(keys))
}

/// Whether `list` is issued by a key of the anchor that is valid at `now`, with a verifying
/// signature. Says nothing about generations.
#[must_use]
pub fn list_is_trusted(trust: &UpdateTrust, list: &UpdateRevocation, now: i64) -> bool {
    let Some(issuer) = trust.key(&list.body.issued_by) else {
        return false;
    };
    issuer.valid_at(now)
        && issuer
            .public()
            .verify(&list.body.signed_bytes(), list.signature())
            .is_ok()
}

fn read_capped(path: &Path) -> Result<Option<String>, UpdateError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(trust_err("a trust file cannot be inspected")),
    };
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err(trust_err("a trust file is not a regular file of sane size"));
    }
    fs::read_to_string(path)
        .map(Some)
        .map_err(|_| trust_err("a trust file cannot be read"))
}

/// Where the newest verified revocation list is kept.
pub trait RevocationStore: Send + Sync {
    /// The stored list, if any.
    ///
    /// # Errors
    /// [`UpdateError::Trust`] when the stored list exists but cannot be read.
    fn load(&self) -> Result<Option<UpdateRevocation>, UpdateError>;

    /// Replaces the stored list, durably.
    ///
    /// # Errors
    /// [`UpdateError::Trust`] when it cannot be written.
    fn save(&self, list: &UpdateRevocation) -> Result<(), UpdateError>;
}

/// The revocation list in a file of the state directory.
#[derive(Clone, Debug)]
pub struct FileRevocationStore {
    path: PathBuf,
}

impl FileRevocationStore {
    /// A store over `path` (the file need not exist yet).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl RevocationStore for FileRevocationStore {
    fn load(&self) -> Result<Option<UpdateRevocation>, UpdateError> {
        let Some(text) = read_capped(&self.path)? else {
            return Ok(None);
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| trust_err("the stored revocation list is not JSON"))?;
        revocation_from_json(&value)
            .map(Some)
            .map_err(|_| trust_err("the stored revocation list is malformed"))
    }

    fn save(&self, list: &UpdateRevocation) -> Result<(), UpdateError> {
        let bytes = revocation_to_json(list).to_string().into_bytes();
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|_| trust_err("the revocation directory cannot be created"))?;
        }
        let temporary = self.path.with_extension("tmp");
        let write = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
        })();
        write.map_err(|_| trust_err("the revocation list cannot be written"))
    }
}

/// An in-memory store, for tests and for a host that keeps the list elsewhere.
#[derive(Debug, Default)]
pub struct MemoryRevocationStore {
    held: Mutex<Option<UpdateRevocation>>,
}

impl MemoryRevocationStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store already holding `list`.
    #[must_use]
    pub fn with(list: UpdateRevocation) -> Self {
        Self {
            held: Mutex::new(Some(list)),
        }
    }
}

impl RevocationStore for MemoryRevocationStore {
    fn load(&self) -> Result<Option<UpdateRevocation>, UpdateError> {
        self.held
            .lock()
            .map(|held| held.clone())
            .map_err(|_| trust_err("the revocation store is poisoned"))
    }

    fn save(&self, list: &UpdateRevocation) -> Result<(), UpdateError> {
        let mut held = self
            .held
            .lock()
            .map_err(|_| trust_err("the revocation store is poisoned"))?;
        *held = Some(list.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_updater::{RevokedUpdateKey, SigningKey, UpdateRevocationBody};

    use super::*;

    fn list(generation: u64) -> UpdateRevocation {
        let signing = SigningKey::from_seed(&[9; 32]);
        UpdateRevocation::issue(
            UpdateRevocationBody {
                generation,
                issued_at: 0,
                issued_by: signing.public().key_id(),
                keys: vec![RevokedUpdateKey {
                    key_id: SigningKey::from_seed(&[5; 32]).public().key_id(),
                    reason: "compromised".to_owned(),
                }],
            },
            &signing,
        )
    }

    fn anchor_text(extra: &str) -> String {
        let key = STANDARD.encode(SigningKey::from_seed(&[1; 32]).public().as_bytes());
        format!(r#"{{"keys":[{{"public_key":"{key}"{extra}}}]}}"#)
    }

    #[test]
    fn the_trust_anchor_is_strict() {
        let trust = parse_anchor(&anchor_text(r#","valid_from":5,"valid_to":null"#)).unwrap();
        assert_eq!(trust.keys().len(), 1);
        assert!(parse_anchor(r#"{"keys":[]}"#).is_err());
        assert!(parse_anchor("[]").is_err());
        assert!(parse_anchor("not json").is_err());
        assert!(parse_anchor(r#"{"keys":[{"public_key":"AAAA"}]}"#).is_err());
        assert!(parse_anchor(r#"{"keys":[{"valid_from":0}]}"#).is_err());
        assert!(parse_anchor(&anchor_text(r#","valid_to":1.5"#)).is_err());
    }

    #[test]
    fn an_anchor_file_is_read_and_a_missing_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-trust.json");
        assert!(load_anchor(&path).is_err());
        std::fs::write(&path, anchor_text("")).unwrap();
        assert_eq!(load_anchor(&path).unwrap().keys().len(), 1);
    }

    #[test]
    fn the_file_store_round_trips_and_a_corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update").join("revocations.json");
        let store = FileRevocationStore::new(&path);
        assert_eq!(store.load().unwrap(), None);
        let original = list(4);
        store.save(&original).unwrap();
        assert_eq!(store.load().unwrap(), Some(original));
        std::fs::write(&path, b"{not json").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn a_list_is_trusted_only_from_an_anchor_key_in_its_window() {
        let anchor = UpdateTrust::new(vec![TrustedUpdateKey::new(
            SigningKey::from_seed(&[9; 32]).public(),
            100,
            Some(200),
        )]);
        let signed = list(1);
        assert!(list_is_trusted(&anchor, &signed, 150));
        assert!(!list_is_trusted(&anchor, &signed, 50), "before the window");
        assert!(!list_is_trusted(&anchor, &signed, 200), "after the window");
        let stranger = UpdateTrust::new(vec![TrustedUpdateKey::new(
            SigningKey::from_seed(&[1; 32]).public(),
            0,
            None,
        )]);
        assert!(!list_is_trusted(&stranger, &signed, 150));
    }
}
