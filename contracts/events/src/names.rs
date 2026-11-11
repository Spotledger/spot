//! Validated names: topics, event sources and command ids.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use spot_storage_contract::Name;

/// Why a string is not a valid name of the given kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidName {
    /// Which kind of name, for example `topic`.
    pub kind: &'static str,
    /// The rejected input.
    pub input: String,
    /// Plain-language reason.
    pub reason: &'static str,
}

impl fmt::Display for InvalidName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} {:?}: {}", self.kind, self.input, self.reason)
    }
}

impl std::error::Error for InvalidName {}

fn invalid(kind: &'static str, input: &str, reason: &'static str) -> InvalidName {
    InvalidName {
        kind,
        input: input.to_owned(),
        reason,
    }
}

/// An event type, for example `doc_posted` or `stock_below_reorder_level`.
///
/// A topic is a storage [`Name`] (`[a-z][a-z0-9_]*`, at most 63 bytes), because it is stored in
/// the outbox's `topic`. The prefixes `doc_` and `sys_` are reserved for the platform
/// ([`crate::lifecycle`] and future system events): an app topic may not use them. Collisions
/// between apps are checked when an app is installed (WP-EXT-03).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Topic(Name);

impl Topic {
    /// Reserved topic prefixes.
    pub const RESERVED_PREFIXES: [&'static str; 3] = ["doc_", "sys_", "identity_"];

    /// Validates an app topic: a storage name without a reserved prefix.
    pub fn app(name: &str) -> Result<Self, InvalidName> {
        let t = Self::any(name)?;
        if t.is_platform() {
            return Err(invalid(
                "topic",
                name,
                "the prefixes doc_, sys_ and identity_ are reserved for the platform",
            ));
        }
        Ok(t)
    }

    /// Validates any topic, including platform topics.
    pub fn any(name: &str) -> Result<Self, InvalidName> {
        Name::new(name).map(Self).map_err(|_| {
            invalid(
                "topic",
                name,
                "a topic is [a-z][a-z0-9_]* and at most 63 bytes",
            )
        })
    }

    /// True for `doc_*`, `sys_*` and `identity_*` topics.
    pub fn is_platform(&self) -> bool {
        Self::RESERVED_PREFIXES
            .iter()
            .any(|p| self.0.as_str().starts_with(p))
    }

    /// The topic as the storage name stored in the outbox.
    pub fn name(&self) -> &Name {
        &self.0
    }

    /// The topic.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Topic {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Topic {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Topic::any(&s).map_err(serde::de::Error::custom)
    }
}

/// The database that produced an event: a site, or one partition of it such as a branch edge
/// (ADR-23, ADR-34). Positions, transaction ids and document versions are only comparable
/// between events of the **same** source.
///
/// Written `<site>` or `<site>#<partition>`: the site is a lower-case host name (dot-separated
/// labels of `a-z0-9-`, 1 to 253 bytes) and the partition a storage name, for example
/// `acme.spotledger.app#branch_khi_01`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(String);

impl SourceId {
    /// Parses and validates a source id.
    pub fn new(s: &str) -> Result<Self, InvalidName> {
        let (site, partition) = match s.split_once('#') {
            Some((site, p)) => (site, Some(p)),
            None => (s, None),
        };
        check_host(site).map_err(|reason| invalid("source id", s, reason))?;
        if let Some(p) = partition {
            Name::new(p)
                .map_err(|_| invalid("source id", s, "the partition must be a storage name"))?;
        }
        Ok(Self(s.to_owned()))
    }

    /// The site part.
    pub fn site(&self) -> &str {
        self.0
            .split_once('#')
            .map_or(self.0.as_str(), |(site, _)| site)
    }

    /// The partition part, if any.
    pub fn partition(&self) -> Option<&str> {
        self.0.split_once('#').map(|(_, p)| p)
    }

    /// The source id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn check_host(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 253 {
        return Err("the site must be 1 to 253 bytes");
    }
    for label in name.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err("the site must be a lower-case host name");
        }
    }
    Ok(())
}

/// The id of the command that caused an event: the native API's `Idempotency-Key` (ADR-055),
/// or an id a job or script assigns. 1 to 128 visible ASCII characters. Retrying an uncertain
/// command with the same id lets consumers and the API recognise the retry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(String);

impl CommandId {
    /// Validates a command id.
    pub fn new(s: &str) -> Result<Self, InvalidName> {
        if s.is_empty() || s.len() > 128 || !s.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(invalid(
                "command id",
                s,
                "a command id is 1 to 128 visible ASCII characters",
            ));
        }
        Ok(Self(s.to_owned()))
    }

    /// The command id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! string_serde {
    ($t:ty, $ctor:path) => {
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                $ctor(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

string_serde!(SourceId, SourceId::new);
string_serde!(CommandId, CommandId::new);
