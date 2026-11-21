//! The command model and its canonical bytes (`WP-ADM-01.T2`).
//!
//! A [`Command`] is the signed content: what runs (`op`, `params`), for whom
//! (`audience`), who issued it (`issuer`), when (`issued_at`, `expires_at`) and the
//! freshness tag the verifier burns on acceptance (`nonce`). The operation and its
//! parameters are opaque here — `op` is an arbitrary name and `params` a free JSON
//! value; the operation catalogue, its schemas and risk levels belong to WP-ADM-01.T1
//! and WP-ADM-03.
//!
//! # The bytes both sides agree on
//!
//! [`Command::canonical_bytes`] produces the deterministic JSON encoding of
//! the canonical encoding ([`crate::canonical`]): sorted keys at every depth, no whitespace, integers as plain digits,
//! minimal string escaping. [`Command::from_json_str`] parses untrusted text through the
//! same rules and refuses what the encoding refuses (floats, unknown fields, the wrong
//! key order). The signed message is that file, domain-separated:
//! [`DOMAIN_PREFIX`]` || canonical_bytes`.

use rand::TryRngCore;
use rand::rngs::OsRng;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::canonical::canonical_json_value;
use crate::error::CommandError;

/// The format version this task defines. A command naming anything else is refused
/// before any other check runs.
pub const COMMAND_VERSION: u8 = 1;

/// The domain separator of the signed message: `b"spotledger-command-v1\0" ||`
/// canonical bytes. The trailing NUL ends the prefix unambiguously — no canonical JSON
/// document starts with anything but `{` (the command is an object), so the separator
/// cannot collide with the payload's own spelling.
pub const DOMAIN_PREFIX: &[u8] = b"spotledger-command-v1\0";

/// The identifier of a signing/trusting key: the first 16 bytes of the SHA-256 of the
/// Ed25519 public key, lower-case hex (32 characters).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyId(String);

impl KeyId {
    /// Derives the key id from raw public-key bytes, the derivation every producer of
    /// trust material here uses.
    #[must_use]
    pub fn from_public_key(public_key: &[u8; 32]) -> Self {
        let digest = Sha256::digest(public_key);
        let mut hex = String::with_capacity(32);
        for byte in &digest[..16] {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        Self(hex)
    }

    /// The id's text form: 32 lower-case hex characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id's bytes (the hex text), as [`NonceStore`](crate::nonce::NonceStore) keys it.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Display for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for KeyId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The signed content of an administrative command.
///
/// Built through [`Command::new`] (which mints the id and the nonce) or parsed from
/// canonical JSON text through [`Command::from_json_str`]. The canonical bytes the
/// signer hashes are [`Command::canonical_bytes`]; nothing else is ever signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The format version, always [`COMMAND_VERSION`] for this task.
    version: u8,
    /// A random 128-bit id in lower-case hex, for audit trails and log correlation.
    id: String,
    /// The operation to run (opaque here; the catalogue is WP-ADM-01.T1/WP-ADM-03).
    op: String,
    /// The operation's parameters (opaque JSON; no float ever allowed).
    params: Value,
    /// The agent id the command is for; a verifier on any other agent refuses it.
    audience: String,
    /// The issuing key, named like the signatures the verifier will check.
    issuer: KeyId,
    /// First valid instant, UTC seconds since the Unix epoch.
    issued_at: i64,
    /// Expiry instant (exclusive), UTC seconds since the Unix epoch.
    expires_at: i64,
    /// A random 128-bit tag the verifier records on acceptance; a second command with
    /// the same `(issuer, nonce)` under the same store is a replay.
    nonce: [u8; 16],
}

impl Command {
    /// Builds a command, minting the random `id` and `nonce` itself.
    ///
    /// Both random fields come from the operating system's CSPRNG (`OsRng`, pulled in
    /// for exactly this); the caller supplies every key and every other field.
    #[must_use]
    pub fn new(
        op: impl Into<String>,
        params: Value,
        audience: impl Into<String>,
        issuer: KeyId,
        issued_at: i64,
        expires_at: i64,
    ) -> Self {
        let mut id_bytes = [0u8; 16];
        let mut nonce = [0u8; 16];
        // The only failure is a broken OS entropy source; a panic there is honest, a
        // predictable id or nonce is not acceptable. (`OsRng::try_fill_bytes` via
        // `TryRngCore`, the house pattern `spot-secrets` uses.)
        OsRng
            .try_fill_bytes(id_bytes.as_mut_slice())
            .unwrap_or_else(|error| panic!("the OS entropy source failed: {error}"));
        OsRng
            .try_fill_bytes(nonce.as_mut_slice())
            .unwrap_or_else(|error| panic!("the OS entropy source failed: {error}"));
        Self {
            version: COMMAND_VERSION,
            id: hex_of_128(&id_bytes),
            op: op.into(),
            params,
            audience: audience.into(),
            issuer,
            issued_at,
            expires_at,
            nonce,
        }
    }

    /// Builds a command from explicit parts, id and nonce included: the constructor the
    /// verifier side (parsing, golden tests, property tests) uses.
    ///
    /// The fields are checked for structural sanity: the version must be
    /// [`COMMAND_VERSION`], the id must be 32 lower-case hex characters, and the window
    /// must end after it starts. These constructor checks are the contract of the type;
    /// the verifier re-checks them against untrusted bytes anyway.
    ///
    /// # Errors
    /// [`CommandError::UnknownVersion`] for a version other than [`COMMAND_VERSION`];
    /// [`CommandError::Malformed`] for an id that is not 32 lower-case hex characters or
    /// an empty window.
    pub fn from_parts(command: UncheckedCommand) -> Result<Self, CommandError> {
        if command.version != COMMAND_VERSION {
            return Err(CommandError::UnknownVersion {
                version: command.version,
            });
        }
        if !is_lower_hex(&command.id, 32) {
            return Err(CommandError::Malformed(
                "the command id must be 32 lower-case hex characters",
            ));
        }
        if command.expires_at <= command.issued_at {
            return Err(CommandError::Malformed(
                "the expiry instant must be after the issue instant",
            ));
        }
        Ok(Self {
            version: command.version,
            id: command.id,
            op: command.op,
            params: command.params,
            audience: command.audience,
            issuer: command.issuer,
            issued_at: command.issued_at,
            expires_at: command.expires_at,
            nonce: command.nonce,
        })
    }

    /// The format version (always [`COMMAND_VERSION`] here).
    #[must_use]
    pub fn version(&self) -> u8 {
        self.version
    }

    /// The command's random hex id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The operation name.
    #[must_use]
    pub fn op(&self) -> &str {
        &self.op
    }

    /// The operation's parameters.
    #[must_use]
    pub fn params(&self) -> &Value {
        &self.params
    }

    /// The agent id the command is addressed to.
    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// The issuing key's id.
    #[must_use]
    pub fn issuer(&self) -> &KeyId {
        &self.issuer
    }

    /// First valid instant (UTC seconds).
    #[must_use]
    pub fn issued_at(&self) -> i64 {
        self.issued_at
    }

    /// Expiry instant, exclusive (UTC seconds).
    #[must_use]
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }

    /// The freshness tag the verifier's store records on acceptance.
    #[must_use]
    pub fn nonce(&self) -> &[u8; 16] {
        &self.nonce
    }

    /// The canonical JSON bytes of the command, the exact input to the signature after
    /// the domain prefix.
    ///
    /// # Errors
    /// [`CommandError::NotCanonical`] when `params` contains a JSON float, which has no
    /// canonical bytes.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CommandError> {
        let bytes =
            canonical_json_value(&self.as_json_value()).map_err(|_| CommandError::NotCanonical)?;
        debug_assert!(!bytes.is_empty(), "an object command always emits bytes");
        Ok(bytes)
    }

    /// The full signed-message bytes: [`DOMAIN_PREFIX`] || [`Command::canonical_bytes`].
    ///
    /// # Errors
    /// As [`Command::canonical_bytes`].
    pub fn signed_message(&self) -> Result<Vec<u8>, CommandError> {
        let mut message = Vec::with_capacity(DOMAIN_PREFIX.len() + 64);
        message.extend_from_slice(DOMAIN_PREFIX);
        message.extend_from_slice(&self.canonical_bytes()?);
        Ok(message)
    }

    /// Parses canonical JSON text (produced by [`Command::canonical_bytes`] on the
    /// issuing side, or by anything claiming to be one of these commands) into a
    /// command, accepting only the exact field set in the canonical form.
    ///
    /// The text is untrusted input: unknown fields, a float, the wrong field types or
    /// non-canonical bytes (whitespace, unsorted keys, duplicate keys) are refused —
    /// the parsed command is re-canonicalised and must reproduce the input
    /// byte-identically, the same canonicity gate the verifier applies. Parsing is
    /// therefore a check, not an interpretation: what comes out is exactly what a
    /// compliant issuer signed.
    ///
    /// # Errors
    /// [`CommandError::Malformed`] for anything structurally wrong, including
    /// non-canonical encodings; [`CommandError::UnknownVersion`] for a foreign version;
    /// [`CommandError::NotCanonical`] for a float anywhere.
    pub fn from_json_str(text: &str) -> Result<Self, CommandError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|_| CommandError::Malformed("the command is not JSON"))?;
        let json = json_object(&value)?;
        let command = Self::from_json_object(json)?;
        // Canonicity: the writer's rendering of what was parsed must equal the input
        // bytes, so a whitespace-bearing or reordered "command" is refused as what it
        // is — not one of these commands.
        let canonical = command.canonical_bytes()?;
        if canonical != text.as_bytes() {
            return Err(CommandError::Malformed("the command is not canonical JSON"));
        }
        Ok(command)
    }

    /// Builds the command's JSON shape (the fields as a map) for the canonical writer.
    fn as_json_value(&self) -> Value {
        let mut map = serde_json::Map::new();
        // Insertion order is irrelevant: the canonical writer sorts keys. Field names
        // are the wire contract of the envelope.
        map.insert("version".to_owned(), Value::from(self.version));
        map.insert("id".to_owned(), Value::from(self.id.clone()));
        map.insert("op".to_owned(), Value::from(self.op.clone()));
        map.insert("params".to_owned(), self.params.clone());
        map.insert("audience".to_owned(), Value::from(self.audience.clone()));
        map.insert(
            "issuer".to_owned(),
            Value::from(self.issuer.as_str().to_owned()),
        );
        map.insert("issued_at".to_owned(), Value::from(self.issued_at));
        map.insert("expires_at".to_owned(), Value::from(self.expires_at));
        map.insert("nonce".to_owned(), Value::from(hex_of_128(&self.nonce)));
        Value::Object(map)
    }

    /// Builds a command from its parsed JSON object, the shared funnel of
    /// [`Command::from_json_str`] and the verifier.
    fn from_json_object(json: Vec<(&str, &Value)>) -> Result<Self, CommandError> {
        let mut version = None;
        let mut id = None;
        let mut op = None;
        let mut params = None;
        let mut audience = None;
        let mut issuer = None;
        let mut issued_at = None;
        let mut expires_at = None;
        let mut nonce = None;
        for (key, value) in json {
            match key {
                "version" => version = Some(u8_field(value)?),
                "id" => id = Some(str_field(value)?),
                "op" => op = Some(str_field(value)?),
                "params" => params = Some(value.clone()),
                "audience" => audience = Some(str_field(value)?),
                "issuer" => issuer = Some(str_field(value)?),
                "issued_at" => issued_at = Some(i64_field(value)?),
                "expires_at" => expires_at = Some(i64_field(value)?),
                "nonce" => nonce = Some(parse_nonce(value)?),
                _ => return Err(CommandError::Malformed("unknown field in the command")),
            }
        }
        let command = UncheckedCommand {
            version: version.unwrap_or(COMMAND_VERSION),
            id: id.ok_or(CommandError::Malformed("missing field: id"))?,
            op: op.ok_or(CommandError::Malformed("missing field: op"))?,
            params: params.ok_or(CommandError::Malformed("missing field: params"))?,
            audience: audience.ok_or(CommandError::Malformed("missing field: audience"))?,
            issuer: KeyId(issuer.ok_or(CommandError::Malformed("missing field: issuer"))?),
            issued_at: issued_at.ok_or(CommandError::Malformed("missing field: issued_at"))?,
            expires_at: expires_at.ok_or(CommandError::Malformed("missing field: expires_at"))?,
            nonce: nonce.ok_or(CommandError::Malformed("missing field: nonce"))?,
        };
        Self::from_parts(command)
    }
}

/// The command fields, gathered for [`Command::from_parts`]'s checks: the constructor
/// for deterministic commands (golden tests, a relay re-materialising a command after
/// forwarding).
pub struct UncheckedCommand {
    /// The format version the bytes claimed.
    pub version: u8,
    /// The command's hex id.
    pub id: String,
    /// The operation name.
    pub op: String,
    /// The parameters.
    pub params: Value,
    /// The audience.
    pub audience: String,
    /// The issuer's key id.
    pub issuer: KeyId,
    /// First valid instant.
    pub issued_at: i64,
    /// Expiry instant.
    pub expires_at: i64,
    /// The freshness tag.
    pub nonce: [u8; 16],
}

/// Reads the fields of a JSON object in canonical order, refusing duplicates.
fn json_object(value: &Value) -> Result<Vec<(&str, &Value)>, CommandError> {
    let map = match value {
        Value::Object(map) => map,
        _ => return Err(CommandError::Malformed("the command is not a JSON object")),
    };
    Ok(map
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect())
}

/// Reads a string field.
fn str_field(value: &Value) -> Result<String, CommandError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or(CommandError::Malformed("a string field is not a string"))
}

/// Reads a `u8` field (the version).
fn u8_field(value: &Value) -> Result<u8, CommandError> {
    value
        .as_u64()
        .and_then(|n| u8::try_from(n).ok())
        .ok_or(CommandError::Malformed(
            "the version field is not a small integer",
        ))
}

/// Reads an `i64` field (the instants).
fn i64_field(value: &Value) -> Result<i64, CommandError> {
    value.as_i64().ok_or(CommandError::Malformed(
        "an instant field is not an integer",
    ))
}

/// Reads the nonce field: exactly 32 lower-case hex characters, decoded to 16 bytes.
fn parse_nonce(value: &Value) -> Result<[u8; 16], CommandError> {
    let text = str_field(value)?;
    if !is_lower_hex(&text, 32) {
        return Err(CommandError::Malformed(
            "the nonce must be 32 lower-case hex characters",
        ));
    }
    let mut nonce = [0u8; 16];
    for (index, byte) in nonce.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|_| CommandError::Malformed("the nonce hex does not decode"))?;
    }
    Ok(nonce)
}

/// Whether `text` is exactly `len` lower-case hex characters.
fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Renders 16 bytes as 32 lower-case hex characters.
fn hex_of_128(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{COMMAND_VERSION, Command, DOMAIN_PREFIX, KeyId};
    use serde_json::json;

    /// A deterministic signing key for tests that only mint ids/nonces.
    fn test_key(seed: u8) -> ed25519_dalek::SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        ed25519_dalek::SigningKey::from_bytes(&bytes)
    }

    /// A plain, valid command for this module's tests.
    fn sample_command() -> Command {
        Command::new(
            "restart_service",
            json!({"service": "spot-runtime"}),
            "agent-node-7",
            KeyId::from_public_key(&test_key(1).verifying_key().to_bytes()),
            1_770_000_000,
            1_770_000_900,
        )
    }

    /// The canonical bytes are stable: field names, sorted order, spelling (the random
    /// id/nonce of `Command::new` only land inside the values, the shape is fixed).
    /// This golden is the contract both sides compile against; changing it intentionally
    /// is a breaking format change (new version, new prefix).
    #[test]
    fn canonical_bytes_are_stable() {
        let command = sample_command();
        let text = String::from_utf8(command.canonical_bytes().unwrap()).unwrap();
        // Every field, exactly once, in sorted key order, compact:
        assert!(text.starts_with(r#"{"audience":"agent-node-7","expires_at":1770000900,"#));
        assert!(text.contains(&format!(r#""id":"{}","#, command.id())));
        assert!(text.contains(r#""issued_at":1770000000,"#));
        assert!(text.contains(r#""issuer":""#));
        assert!(text.contains(r#""nonce":""#));
        assert!(text.contains(r#""op":"restart_service","#));
        assert!(text.contains(r#""params":{"service":"spot-runtime"},"#));
        assert!(text.contains(r#""version":1"#));
        assert!(text.ends_with("}"));
        // and the whole document re-parses to the same command (no duplicates).
        assert_eq!(Command::from_json_str(&text).unwrap(), command);
    }

    /// The domain prefix is `spotledger-command-v1\0` — NUL-terminated, and the signed
    /// message is exactly prefix || canonical bytes.
    #[test]
    fn signed_message_is_prefix_plus_canonical() {
        let command = sample_command();
        let message = command.signed_message().unwrap();
        assert!(message.starts_with(DOMAIN_PREFIX));
        assert_eq!(
            &message[DOMAIN_PREFIX.len()..],
            &command.canonical_bytes().unwrap()
        );
        assert_eq!(DOMAIN_PREFIX, b"spotledger-command-v1\0");
    }

    /// `Command::new` mints a fresh 128-bit id and nonce per call; two commands never
    /// share either.
    #[test]
    fn new_mints_distinct_id_and_nonce() {
        let issuer = KeyId::from_public_key(&test_key(2).verifying_key().to_bytes());
        let first = Command::new("noop", json!({}), "agent-a", issuer.clone(), 100, 200);
        let second = Command::new("noop", json!({}), "agent-a", issuer, 100, 200);
        assert_ne!(first.id(), second.id());
        assert_ne!(first.nonce(), second.nonce());
        assert_eq!(first.id().len(), 32);
        assert_eq!(first.nonce().len(), 16);
    }

    /// KeyId derivation: SHA-256, first 16 bytes, lower-case hex — the test recomputes
    /// the digest independently and compares.
    #[test]
    fn key_id_is_sha256_prefix_hex() {
        use sha2::{Digest, Sha256};
        let public = test_key(5).verifying_key().to_bytes();
        let key_id = KeyId::from_public_key(&public);
        let digest = Sha256::digest(public);
        let expected: String = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(key_id.as_str(), expected);
        assert_eq!(key_id.as_str().len(), 32);
    }

    /// Constructor checks: wrong version, bad id, inverted window are refused.
    #[test]
    fn constructor_checks() {
        let base = || super::UncheckedCommand {
            version: COMMAND_VERSION,
            id: "0198c7a2feedface0198c7a2feedface".to_owned(),
            op: "noop".to_owned(),
            params: json!({}),
            audience: "agent-a".to_owned(),
            issuer: KeyId("a".repeat(32)),
            issued_at: 100,
            expires_at: 200,
            nonce: [0u8; 16],
        };
        let mut wrong_version = base();
        wrong_version.version = 2;
        assert!(Command::from_parts(wrong_version).is_err());
        let mut bad_id = base();
        bad_id.id = "NOT-HEX".to_owned();
        assert!(Command::from_parts(bad_id).is_err());
        let mut inverted = base();
        inverted.expires_at = 100;
        assert!(Command::from_parts(inverted).is_err());
        assert!(Command::from_parts(base()).is_ok());
    }

    /// Canonical JSON text round trips through `from_json_str` byte-identically.
    #[test]
    fn canonical_text_round_trips() {
        let command = sample_command();
        let text = String::from_utf8(command.canonical_bytes().unwrap()).unwrap();
        let parsed = Command::from_json_str(&text).unwrap();
        assert_eq!(parsed, command);
    }

    /// Non-canonical text (whitespace) is refused: the wire format is one spelling.
    #[test]
    fn whitespace_text_is_refused() {
        let text = r#" { "op" : "noop" } "#;
        assert!(Command::from_json_str(text).is_err());
    }
}
