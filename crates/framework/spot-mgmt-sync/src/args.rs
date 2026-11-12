//! The typed arguments of the operations that take any, parsed out of a validated invocation.
//!
//! The catalogue already checked ranges, enums and patterns; parsing here maps the surviving
//! text onto closed Rust types and refuses combinations the schema cannot express (enrolling a
//! hub, a confirmation that does not equal its target).

use serde::{Deserialize, Serialize};

use crate::error::OpError;

/// A position in a node's stream: a generation and a sequence number within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    /// The stream generation.
    pub generation: u64,
    /// The sequence number within the generation.
    pub seq: u64,
}

impl Position {
    /// The pattern the catalogue enforces on a position's text (whole value).
    pub const PATTERN: &'static str = "[0-9]{1,10}:[0-9]{1,18}";

    /// Parses `<generation>:<seq>`.
    ///
    /// # Errors
    /// A short reason when the text is not two numbers separated by a colon.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let (generation, seq) = text
            .split_once(':')
            .ok_or("position needs generation:seq")?;
        let generation = generation
            .parse::<u64>()
            .map_err(|_| "generation is not a number")?;
        let seq = seq.parse::<u64>().map_err(|_| "seq is not a number")?;
        Ok(Self { generation, seq })
    }

    /// Renders the position back to the text [`Position::parse`] reads.
    #[must_use]
    pub fn render(&self) -> String {
        format!("{}:{}", self.generation, self.seq)
    }
}

/// A site's role in the hub and edge vocabulary (ADR-062).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// The authoritative site of the tenant.
    Hub,
    /// A site that syncs with the hub.
    Edge,
}

impl NodeRole {
    /// The two role names, in the order the console shows them.
    pub const NAMES: [&'static str; 2] = ["edge", "hub"];

    /// The catalogue spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::Edge => "edge",
        }
    }

    /// The role spelled `text`, if any.
    #[must_use]
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "hub" => Some(Self::Hub),
            "edge" => Some(Self::Edge),
            _ => None,
        }
    }
}

/// The arguments of `sync.node.enrol`. The invocation carries no secret: the one-time code is
/// in the result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrolArgs {
    /// The node's name (its host name).
    pub node: String,
    /// The node's role; only `edge` can be enrolled.
    pub role: NodeRole,
    /// The partitions the node receives, by name (at least one).
    pub partitions: Vec<String>,
    /// The name of a preset or the id of a stored subscription.
    pub subscription: String,
    /// How many hours the one-time code stays valid (1 to 168).
    pub valid_hours: u16,
}

/// The arguments of the operations that name one node and nothing else: `show`, `suspend`,
/// `resume`, `rotate_certificate`, `notify_contact`; and `revoke`, whose `confirm` the
/// dispatcher has already checked against the node (it is not carried).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeArgs {
    /// The node.
    pub node: String,
}

/// The arguments of `sync.node.resend`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResendArgs {
    /// The node.
    pub node: String,
    /// Where to resume sending from; `None` means from the node's last acknowledged position.
    pub from: Option<Position>,
}

/// The arguments of `sync.quarantine.retry`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineRetryArgs {
    /// The quarantined item's id.
    pub item: String,
}

/// The arguments of `sync.quarantine.discard`; the `confirm` (equal to the item) has been
/// checked by the dispatcher.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineDiscardArgs {
    /// The quarantined item's id.
    pub item: String,
    /// Why it is discarded: a short snake_case code, recorded in the audit.
    pub reason_code: String,
}

/// The arguments of `sync.policy.show`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyShowArgs {
    /// The doctype whose sync policy is shown.
    pub doctype: String,
}

/// The arguments of `sync.role.change`; the `confirm` (equal to `to`) has been checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleChangeArgs {
    /// The role this site changes to.
    pub to: NodeRole,
}

/// A short refusal builder for the dispatcher.
pub(crate) fn bad(op: &str, param: &str, reason: &'static str) -> OpError {
    OpError::BadArguments {
        op: op.to_owned(),
        param: param.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip_and_refuse_junk() {
        let p = Position::parse("3:41").unwrap_or(Position {
            generation: 0,
            seq: 0,
        });
        assert_eq!((p.generation, p.seq), (3, 41));
        assert_eq!(p.render(), "3:41");
        for junk in ["", "3", "a:1", "1:b", ":", "1:2:3"] {
            assert!(Position::parse(junk).is_err(), "{junk:?}");
        }
    }

    #[test]
    fn roles_round_trip() {
        for name in NodeRole::NAMES {
            assert_eq!(NodeRole::from_name(name).map(NodeRole::name), Some(name));
        }
        assert!(NodeRole::from_name("spoke").is_none());
    }
}
