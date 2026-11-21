//! Message ids and conversation names.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

/// A message id: a UUIDv7 built from caller-supplied time and entropy (ADR-33).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MessageId(Uuid);

impl MessageId {
    /// Builds an id from a Unix-millisecond instant and ten bytes of entropy.
    #[must_use]
    pub fn new_v7(unix_ms: u64, entropy: [u8; 10]) -> Self {
        Self(uuid::Builder::from_unix_timestamp_millis(unix_ms, &entropy).into_uuid())
    }

    /// Wraps an existing UUID.
    ///
    /// # Errors
    /// [`Error::InvalidId`] when it is not a UUIDv7.
    pub fn from_uuid(id: Uuid) -> Result<Self> {
        if id.get_version_num() == 7 {
            Ok(Self(id))
        } else {
            Err(Error::InvalidId {
                reason: "a message id must be a UUIDv7",
            })
        }
    }

    /// The UUID.
    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    pub(crate) fn bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}

impl fmt::Debug for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MessageId({self})")
    }
}

impl FromStr for MessageId {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let uuid = Uuid::parse_str(text).map_err(|_| Error::InvalidId {
            reason: "a message id is a hyphenated UUIDv7",
        })?;
        Self::from_uuid(uuid)
    }
}

impl From<MessageId> for String {
    fn from(value: MessageId) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for MessageId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// Where a message id comes from. The host supplies time and entropy; this crate reads neither.
pub trait IdSource: Send + Sync {
    /// A fresh id for an item created at `now` (Unix milliseconds).
    fn next_id(&self, now: u64) -> MessageId;
}

/// A named ordered stream of messages on one link. Sequence numbers run per conversation.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Conversation(String);

impl Conversation {
    /// Validates a conversation name: 1 to 64 characters of lower-case letters, digits and
    /// `_ - . :`, starting with a letter or digit.
    ///
    /// # Errors
    /// [`Error::InvalidConversation`] when the name is malformed.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let mut chars = value.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest_ok = chars.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.' | ':')
        });
        if value.len() <= 64 && first_ok && rest_ok {
            Ok(Self(value))
        } else {
            Err(Error::InvalidConversation {
                reason: "1 to 64 lower-case letters, digits and _ - . :",
            })
        }
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Conversation({})", self.0)
    }
}

impl TryFrom<String> for Conversation {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<Conversation> for String {
    fn from(value: Conversation) -> Self {
        value.0
    }
}
