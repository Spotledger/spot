//! [`ConsumerSpec`]: the registered definition of one named consumer.
//!
//! A consumer is fixed for its life: the topics it wants, an optional filter, the partition
//! count (changing it is a new consumer), the payload mode and where it starts. The TOML/JSON
//! form rejects unknown fields (`deny_unknown_fields`), so a typo is an error and never a
//! silently ignored setting.
//!
//! The reserved platform topic prefixes (`doc_`, `sys_`, `identity_`) may be selected only by
//! an owner that holds the explicit stream right for that class; the syntax is checked here and
//! the right at [`register`](crate::StreamEngine::register).

use serde::{Deserialize, Serialize};
use spot_events_contract::envelope::Position;
use spot_events_contract::{Topic, names::InvalidName};
use spot_storage_contract::{Name, RecordId};

use crate::error::StreamError;

/// The most partitions a consumer may have.
pub const MAX_PARTITIONS: u32 = 64;

/// Which topics a consumer wants: one exact name, or a prefix ending in `.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum TopicSelector {
    /// One exact topic name.
    Exact(String),
    /// Every topic starting with this prefix (`sales.*` selects `sales_invoice`).
    Prefix(String),
}

impl TopicSelector {
    /// Parses a selector from its text.
    ///
    /// # Errors
    ///
    /// [`StreamError::SpecInvalid`] when an exact name is not a topic, or a prefix is not a
    /// name prefix.
    pub fn parse(text: &str) -> Result<Self, StreamError> {
        if let Some(prefix) = text.strip_suffix(".*") {
            if prefix.is_empty() {
                return Err(StreamError::spec("a topic prefix must not be empty"));
            }
            Name::new(prefix)
                .map_err(|_| StreamError::spec("a topic prefix is [a-z][a-z0-9_]*"))?;
            return Ok(Self::Prefix(prefix.to_owned()));
        }
        Topic::any(text)
            .map(|_| Self::Exact(text.to_owned()))
            .map_err(|e: InvalidName| StreamError::spec(e.to_string()))
    }

    /// Whether `topic` is selected.
    #[must_use]
    pub fn matches(&self, topic: &str) -> bool {
        match self {
            Self::Exact(name) => topic == name,
            Self::Prefix(prefix) => topic.starts_with(prefix.as_str()),
        }
    }

    /// The text form (what the selector was written as).
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::Exact(name) => name.clone(),
            Self::Prefix(prefix) => format!("{prefix}.*"),
        }
    }

    /// The reserved platform class this selector touches, if any.
    ///
    /// A selector is reserved exactly when the set of topics it selects overlaps a reserved
    /// class's topics, i.e. when it can ever match a reserved topic:
    /// - `Exact(name)`: reserved when `name` starts with a class (`doc_invoice`) or is the
    ///   bare class name itself (`doc`, `sys`, `identity`). A proper prefix of a class
    ///   (`d`, `do`) is allowed: an exact name selects only itself.
    /// - `Prefix(p)`: reserved when `p` is itself inside a class (`p.starts_with(class)`, e.g.
    ///   `doc_invoice` for class `doc_`) OR `p` is a prefix that a class extends
    ///   (`class.starts_with(p)`, e.g. `d`, `do`, `doc`, `doc_` for class `doc_`): such a `p`
    ///   selects every topic starting with it, which includes every topic in the class. A
    ///   prefix like `docs` is allowed: it is not a prefix of `doc_` (the fourth character
    ///   differs, `s` vs `_`) and `doc_` is not a prefix of `docs`, so no reserved topic can
    ///   ever start with `docs`.
    #[must_use]
    pub fn reserved_class(&self) -> Option<&'static str> {
        match self {
            // An exact name selects only itself: reserved when it is inside a class or is the
            // bare class name. A mere prefix of a class (`d`, `do`, `i`) selects one topic
            // that no reserved topic can equal, so it stays allowed.
            Self::Exact(name) => Topic::RESERVED_PREFIXES.into_iter().find(|class| {
                name.starts_with(class) || name.as_str() == class.trim_end_matches('_')
            }),
            Self::Prefix(prefix) => Topic::RESERVED_PREFIXES
                .into_iter()
                .find(|class| prefix.starts_with(class) || class.starts_with(prefix.as_str())),
        }
    }
}

impl TryFrom<String> for TopicSelector {
    type Error = StreamError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<TopicSelector> for String {
    fn from(value: TopicSelector) -> Self {
        value.as_str()
    }
}

/// How much of the event a consumer receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadMode {
    /// Identifiers and metadata only: the envelope's `data` is empty.
    IdsOnly,
    /// The payload's fields, with protected fields removed or masked by the field guard.
    Fields,
}

/// Where a consumer starts, before its first acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Start {
    /// Only events published from now on (the feed's head at the first pull).
    Tail,
    /// Every event still retained, from the feed's start.
    FromBeginning,
    /// From a stated position (a resume, or a rebuild from a snapshot).
    FromPosition(Position),
}

/// The registered definition of one named consumer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerSpec {
    /// Unique per site.
    pub name: String,
    /// The principal that registered the consumer; its rights filter the stream.
    pub owner: String,
    /// The topics to receive.
    pub topics: Vec<TopicSelector>,
    /// An optional expression over the envelope's `meta` and `subject`, in the shared
    /// expression language (ADR-28); parsed on registration.
    #[serde(default)]
    pub filter: Option<String>,
    /// 1 to [`MAX_PARTITIONS`]; a partition is `hash(subject.id) mod partitions`.
    pub partitions: u32,
    /// How much of each event to receive.
    pub payload_mode: PayloadMode,
    /// Where to start.
    pub start: Start,
}

impl ConsumerSpec {
    /// Checks the definition's own rules (syntax and ranges; the owner's rights are checked at
    /// registration).
    ///
    /// # Errors
    ///
    /// [`StreamError::SpecInvalid`] naming the first problem.
    pub fn validate(&self) -> Result<(), StreamError> {
        Name::new(&self.name).map_err(|_| {
            StreamError::spec("a consumer name is [a-z][a-z0-9_]* and at most 63 bytes")
        })?;
        if self.owner.is_empty() || self.owner.chars().any(char::is_control) {
            return Err(StreamError::spec(
                "a consumer owner must be a printable name",
            ));
        }
        if self.topics.is_empty() {
            return Err(StreamError::spec("a consumer needs at least one topic"));
        }
        if self.partitions == 0 || self.partitions > MAX_PARTITIONS {
            return Err(StreamError::spec(format!(
                "partitions must be 1 to {MAX_PARTITIONS}"
            )));
        }
        if let Some(filter) = &self.filter {
            spot_expr::parse(filter)
                .map_err(|e| StreamError::spec(format!("the filter does not parse: {e}")))?;
        }
        if let Start::FromPosition(p) = &self.start
            && p.generation == 0
            && p.seq != 0
        {
            return Err(StreamError::spec(
                "a from_position start needs generation 0 with seq 0, or a generation above 0",
            ));
        }
        Ok(())
    }

    /// Whether the topic is selected by any of the spec's selectors.
    #[must_use]
    pub fn matches_topic(&self, topic: &str) -> bool {
        self.topics.iter().any(|t| t.matches(topic))
    }

    /// The partition of a subject id: `fnv1a(id) mod partitions`. Fixed for the life of the
    /// consumer, and stable across processes and restarts.
    #[must_use]
    pub fn partition_for(&self, id: &RecordId) -> u32 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in id.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        u32::try_from(hash % u64::from(self.partitions)).unwrap_or(0)
    }

    /// The initial checkpoint for each partition, before any acknowledgement.
    #[must_use]
    pub fn initial_positions(&self) -> Vec<Position> {
        let start = match &self.start {
            Start::FromPosition(p) => *p,
            Start::Tail | Start::FromBeginning => Position {
                generation: 0,
                seq: 0,
            },
        };
        vec![start; self.partitions as usize]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Each row asserts `reserved_class()` for the given selector text (a bare name is
    /// `Exact`, a name ending `.*` is `Prefix`). See the overlap rule on
    /// [`TopicSelector::reserved_class`]: a selector is reserved exactly when the topic set
    /// it selects can include a reserved topic.
    fn selector(text: &str) -> TopicSelector {
        TopicSelector::parse(text).expect(text)
    }

    #[test]
    fn refused_selectors_touch_a_reserved_class() {
        for text in [
            "d.*",
            "do.*",
            "doc.*",
            "doc_.*",
            "doc_invoice.*",
            "doc_invoice",
            "s.*",
            "sy.*",
            "sys.*",
            "sys_audit",
            "i.*",
            "id.*",
            "identity.*",
            "identity_login.*",
            "doc_x.*",
            "doc",
            "sys",
            "identity",
        ] {
            assert!(
                selector(text).reserved_class().is_some(),
                "{text} should be refused as a reserved selector"
            );
        }
    }

    #[test]
    fn allowed_selectors_do_not_touch_a_reserved_class() {
        for text in [
            "sales.*",
            "docs.*",
            "dock.*",
            "systems.*",
            "sy_x.*",
            "idle.*",
            "a.*",
            "sales_invoice",
            "docs",
            "d",
            "do",
            "s",
            "sy",
            "i",
            "id",
            "identit",
        ] {
            assert!(
                selector(text).reserved_class().is_none(),
                "{text} should be allowed (not a reserved selector)"
            );
        }
    }
}
