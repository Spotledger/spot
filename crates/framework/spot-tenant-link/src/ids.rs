//! Checked identifiers of the link: [`LinkId`], [`ExternalPeerId`], [`Topic`], [`Digest`] and
//! [`Nonce`].
//!
//! Every type validates its shape on construction and on deserialisation, so a malformed value
//! can never reach a store column, a file name or a wire field unvalidated.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::util::{decode_hex, hex_lower, sha256};

/// A tenant link id: a UUIDv7 whose time and entropy the caller supplies.
///
/// This crate never reads a clock or a random source; [`LinkId::new_v7`] takes both.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LinkId(Uuid);

impl LinkId {
    /// Builds a UUIDv7 from a Unix-millisecond timestamp and ten bytes of caller entropy.
    #[must_use]
    pub fn new_v7(unix_ms: u64, entropy: [u8; 10]) -> Self {
        Self(uuid::Builder::from_unix_timestamp_millis(unix_ms, &entropy).into_uuid())
    }

    /// Wraps an existing UUID; only version 7 is accepted, as for a tenant id.
    ///
    /// # Errors
    /// [`Error::InvalidId`] when the UUID is not version 7.
    pub fn from_uuid(id: Uuid) -> Result<Self> {
        if id.get_version_num() == 7 {
            Ok(Self(id))
        } else {
            Err(Error::InvalidId {
                reason: "a link id must be a UUIDv7",
            })
        }
    }

    /// The underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.0.as_bytes());
    }
}

impl fmt::Display for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}

impl fmt::Debug for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LinkId({self})")
    }
}

impl FromStr for LinkId {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let uuid = Uuid::parse_str(text).map_err(|_| Error::InvalidId {
            reason: "a link id is a hyphenated UUIDv7",
        })?;
        Self::from_uuid(uuid)
    }
}

impl From<LinkId> for String {
    fn from(value: LinkId) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for LinkId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// The identity of a peer that is not a platform tenant (ADR-058 rule 12).
///
/// It is an opaque reference chosen by the tenant (for example a connector or vault entry
/// name), never a credential: the credential itself stays in the secrets vault.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExternalPeerId(String);

impl ExternalPeerId {
    /// Validates an external peer id: 1 to 128 characters, a lower-case letter first, then
    /// lower-case letters, digits, `.`, `_`, `:` or `-`.
    ///
    /// # Errors
    /// [`Error::InvalidParty`] when the value is malformed.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let mut chars = value.chars();
        let first = chars.next();
        let ok = !value.is_empty()
            && value.len() <= 128
            && first.is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | ':' | '-')
            });
        if ok {
            Ok(Self(value))
        } else {
            Err(Error::InvalidParty {
                reason: "an external peer id is 1 to 128 characters, a lower-case letter first",
            })
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExternalPeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ExternalPeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ExternalPeerId({})", self.0)
    }
}

impl TryFrom<String> for ExternalPeerId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<ExternalPeerId> for String {
    fn from(value: ExternalPeerId) -> Self {
        value.0
    }
}

/// An exchange topic: two or more dotted lower-case segments, such as `bpm.message`.
///
/// A topic names the *kind of information* that may cross a link. Apps register topics in
/// their manifest (ADR-058 rule 7); the platform carries them and never interprets them.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Topic(String);

impl Topic {
    /// Validates a topic: 1 to 128 characters, at least two segments of `[a-z][a-z0-9_]*`
    /// separated by `.`.
    ///
    /// # Errors
    /// [`Error::InvalidTopic`] when the value is malformed.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let segments: Vec<&str> = value.split('.').collect();
        let segment_ok = |segment: &&str| {
            let mut chars = segment.chars();
            chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        };
        if value.len() > 128 {
            return Err(Error::InvalidTopic {
                reason: "a topic has at most 128 characters",
            });
        }
        if segments.len() < 2 || !segments.iter().all(segment_ok) {
            return Err(Error::InvalidTopic {
                reason: "a topic is dotted lower-case segments, at least two of them",
            });
        }
        Ok(Self(value))
    }

    /// The topic as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Topic({})", self.0)
    }
}

impl TryFrom<String> for Topic {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<Topic> for String {
    fn from(value: Topic) -> Self {
        value.0
    }
}

/// A SHA-256 digest: a transcript link, a message digest or an agreement digest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest([u8; 32]);

impl Digest {
    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(sha256(bytes))
    }

    /// Wraps raw digest bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex_lower(&self.0))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({self})")
    }
}

impl TryFrom<String> for Digest {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        decode_hex::<32>(&value).map(Self).ok_or(Error::InvalidId {
            reason: "a digest is 64 hexadecimal characters",
        })
    }
}

impl From<Digest> for String {
    fn from(value: Digest) -> Self {
        value.to_string()
    }
}

/// A fresh nonce for one handshake message, supplied by the caller (never generated here).
///
/// A nonce may appear once in one handshake; a repeat is refused as a replay.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Nonce([u8; 16]);

impl Nonce {
    /// Wraps 16 caller-supplied bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Nonce({})", hex_lower(&self.0))
    }
}

impl TryFrom<String> for Nonce {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        decode_hex::<16>(&value).map(Self).ok_or(Error::InvalidId {
            reason: "a nonce is 32 hexadecimal characters",
        })
    }
}

impl From<Nonce> for String {
    fn from(value: Nonce) -> Self {
        hex_lower(&value.0)
    }
}
