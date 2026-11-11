//! The typed arguments of the operations that take any, parsed out of a validated invocation.
//!
//! The catalogue already checked ranges, enums and patterns; parsing here maps the surviving
//! text onto closed Rust types and refuses combinations the schema cannot express (a policy
//! that does not take `on_unreachable`, a point-in-time rebuild of `latest`).

use serde::{Deserialize, Serialize};

use crate::error::OpError;

/// A position in the journal: a generation and a sequence number within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    /// The journal generation (it advances when the journal is rebuilt or re-based).
    pub generation: u64,
    /// The sequence number within the generation.
    pub seq: u64,
}

/// What a `remote-sync` tenant does when the object store cannot be reached (ADR-061 section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnUnreachable {
    /// Commit locally, raise an RPO-breach alarm and re-ship as soon as possible.
    Degrade,
    /// Refuse commits.
    Halt,
}

impl OnUnreachable {
    /// The catalogue spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Degrade => "degrade",
            Self::Halt => "halt",
        }
    }

    /// The choice spelled `text`, if any.
    #[must_use]
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "degrade" => Some(Self::Degrade),
            "halt" => Some(Self::Halt),
            _ => None,
        }
    }
}

/// A tenant's durability policy (ADR-061 section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub enum PolicySetting {
    /// Acknowledge when the local commit is durable.
    Local,
    /// Acknowledge when the synchronous standby has the commit.
    ReplicaSync,
    /// Acknowledge when the entry is durable in the object store.
    RemoteSync {
        /// What to do when the store cannot be reached.
        on_unreachable: OnUnreachable,
    },
}

impl PolicySetting {
    /// The three policy names, in the order the console shows them.
    pub const NAMES: [&'static str; 3] = ["local", "replica-sync", "remote-sync"];
}

/// How much of the journal a rebuild replays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RebuildMode {
    /// The latest snapshot (or the origin) plus every journal segment after it.
    Full,
    /// Keep the existing state and apply only the segments it lacks.
    RollForward,
    /// Stop at a chosen position or instant.
    PointInTime,
}

impl RebuildMode {
    /// The three mode names, in the order the console shows them.
    pub const NAMES: [&'static str; 3] = ["full", "roll-forward", "point-in-time"];

    /// The mode spelled `text`, if any.
    #[must_use]
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "full" => Some(Self::Full),
            "roll-forward" => Some(Self::RollForward),
            "point-in-time" => Some(Self::PointInTime),
            _ => None,
        }
    }

    /// The catalogue spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::RollForward => "roll-forward",
            Self::PointInTime => "point-in-time",
        }
    }
}

/// Where a rebuild stops.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "kebab-case")]
pub enum RebuildTarget {
    /// The newest position the journal holds.
    Latest,
    /// A named position.
    At {
        /// The position to stop at.
        position: Position,
    },
    /// The last position before an instant (RFC 3339, checked for shape and ranges).
    Before {
        /// The instant, as the caller wrote it.
        instant: String,
    },
}

impl RebuildTarget {
    /// The pattern the catalogue enforces on the `target` text (whole value).
    pub const PATTERN: &'static str = "latest|at [0-9]{1,10}:[0-9]{1,18}|before [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}([.][0-9]{1,9})?(Z|[+-][0-9]{2}:[0-9]{2})";

    /// Parses the `target` text.
    ///
    /// # Errors
    /// A short reason when the text is none of `latest`, `at <generation>:<seq>` or
    /// `before <rfc3339>`, or when a number or an instant field is out of range.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        if text == "latest" {
            return Ok(Self::Latest);
        }
        if let Some(rest) = text.strip_prefix("at ") {
            let (generation, seq) = rest
                .split_once(':')
                .ok_or("position needs generation:seq")?;
            let generation = generation
                .parse::<u64>()
                .map_err(|_| "generation is not a number")?;
            let seq = seq.parse::<u64>().map_err(|_| "seq is not a number")?;
            return Ok(Self::At {
                position: Position { generation, seq },
            });
        }
        if let Some(instant) = text.strip_prefix("before ") {
            check_instant(instant)?;
            return Ok(Self::Before {
                instant: instant.to_owned(),
            });
        }
        Err("target is latest, at <generation>:<seq> or before <rfc3339>")
    }

    /// Renders the target back to the text [`RebuildTarget::parse`] reads.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Latest => "latest".to_owned(),
            Self::At { position } => format!("at {}:{}", position.generation, position.seq),
            Self::Before { instant } => format!("before {instant}"),
        }
    }
}

/// Checks the field ranges of an RFC 3339 instant the pattern already shaped.
fn check_instant(text: &str) -> Result<(), &'static str> {
    let field = |from: usize, to: usize| -> Result<u32, &'static str> {
        text.get(from..to)
            .and_then(|digits| digits.parse::<u32>().ok())
            .ok_or("instant is not RFC 3339")
    };
    let (month, day) = (field(5, 7)?, field(8, 10)?);
    let (hour, minute, second) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err("instant has an impossible date");
    }
    if hour > 23 || minute > 59 || second > 60 {
        return Err("instant has an impossible time");
    }
    if !text.ends_with('Z') {
        // The pattern guarantees a trailing `+hh:mm` or `-hh:mm`.
        let tail = text.get(text.len().saturating_sub(5)..).unwrap_or("");
        let hours = tail.get(0..2).and_then(|d| d.parse::<u32>().ok());
        let minutes = tail.get(3..5).and_then(|d| d.parse::<u32>().ok());
        if hours.is_none_or(|h| h > 23) || minutes.is_none_or(|m| m > 59) {
            return Err("instant has an impossible offset");
        }
    }
    Ok(())
}

/// The arguments of `journal.rebuild.plan`; also the plan part of `journal.rebuild.run`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildPlanArgs {
    /// Where the rebuild stops.
    pub target: RebuildTarget,
    /// How much it replays.
    pub mode: RebuildMode,
}

impl RebuildPlanArgs {
    /// Builds the arguments, refusing the one combination that has no meaning.
    ///
    /// # Errors
    /// A short reason when `mode` is `point-in-time` and the target is `latest` (a point in
    /// time needs a point).
    pub fn new(target: RebuildTarget, mode: RebuildMode) -> Result<Self, &'static str> {
        if mode == RebuildMode::PointInTime && target == RebuildTarget::Latest {
            return Err("a point-in-time rebuild needs an `at` or `before` target");
        }
        Ok(Self { target, mode })
    }
}

/// The arguments of `journal.rebuild.run`: the plan a person has seen, and its hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildRunArgs {
    /// The plan's arguments; the executor re-plans and refuses when the hash differs.
    pub plan: RebuildPlanArgs,
    /// The hash of the plan the person confirmed (lower-case hex).
    pub confirm_plan_hash: String,
}

/// The arguments of `journal.snapshot.list`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotListArgs {
    /// How many snapshots to return, newest first.
    pub limit: u32,
}

/// The arguments of `journal.reader.grant`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderGrantArgs {
    /// Who is granted read access.
    pub grantee: String,
    /// How many days the grant lasts (1 to 365).
    pub days: u16,
    /// Whether the grantee is outside the tenant (an external auditor).
    pub external: bool,
}

/// The arguments of `journal.reader.revoke`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderRevokeArgs {
    /// Whose grant is revoked.
    pub grantee: String,
}

/// The arguments of `journal.subject.erase`.
///
/// The `confirm` literal has been checked by the dispatcher; it is not carried.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectEraseArgs {
    /// The data subject whose per-subject key is destroyed.
    pub subject: String,
}

impl std::fmt::Debug for SubjectEraseArgs {
    /// The subject is a personal identifier: it never reaches a log through `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SubjectEraseArgs { subject: [redacted] }")
    }
}

/// A short refusal builder for the dispatcher.
pub(crate) fn bad(op: &str, param: &str, reason: &'static str) -> OpError {
    OpError::BadArguments {
        op: op.to_owned(),
        param: param.to_owned(),
        reason,
    }
}
