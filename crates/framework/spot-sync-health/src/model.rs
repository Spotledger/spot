//! The types of the view and their stable JSON form.

use serde::Serialize;
use spot_conditions::{ConditionParams, Millis, Severity};

/// The most rows either list carries; past it the view says `truncated`.
pub const MAX_ROWS: usize = 200;

/// The most subjects a grouped row names; the rest are counted in `more`.
pub const SAMPLE_SIZE: usize = 5;

/// The tenant-wide state: one word the screen turns into one colour and one sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nothing needs a person. A state the surface states, not the absence of news.
    Calm,
    /// Something needs a person, and nothing is red.
    Attention,
    /// Something red needs a person now.
    Action,
}

impl Status {
    /// The message key of the one-line headline for this status.
    #[must_use]
    pub const fn headline_key(self) -> &'static str {
        match self {
            Self::Calm => concat!("sync", ".health", ".headline", ".calm"),
            Self::Attention => concat!("sync", ".health", ".headline", ".attention"),
            Self::Action => concat!("sync", ".health", ".headline", ".action"),
        }
    }
}

/// The colour of a node row: the worst of its facts and its open conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Colour {
    /// Nothing wrong.
    Green,
    /// Minor, or being handled.
    Amber,
    /// A person is needed or damage is possible.
    Red,
}

/// What the freshness and skew checks say about a node, as the caller reduces them.
///
/// This crate does not depend on those crates, so it owns the vocabulary the row needs; the
/// caller maps its freshness state and skew decision onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// In contact and within its freshness limits.
    Connected,
    /// In contact but behind by more than the lag threshold.
    Lagging,
    /// Not heard from within its freshness limit.
    Silent,
    /// Deliberately suspended by an operator.
    Suspended,
    /// Its software is too old for the skew rules: it cannot sync until upgraded.
    TooOld,
}

impl NodeStatus {
    /// The colour this status gives a row.
    #[must_use]
    pub const fn colour(self) -> Colour {
        match self {
            Self::Connected => Colour::Green,
            Self::Lagging | Self::Silent | Self::Suspended => Colour::Amber,
            Self::TooOld => Colour::Red,
        }
    }
}

/// A typed route id in the Admin Center. Never a URL: the interface owns the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Route {
    /// One node's detail page.
    Node,
    /// One record's page (a hold, a missing dependency).
    Record,
    /// One stream's page.
    Stream,
    /// The tenant's sync overview.
    Tenant,
    /// The generic condition detail, for a subject kind this crate does not know.
    Condition,
}

impl Route {
    /// The stable route id the Admin Center resolves.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Node => concat!("admin", ".sync", ".node"),
            Self::Record => concat!("admin", ".sync", ".record"),
            Self::Stream => concat!("admin", ".sync", ".stream"),
            Self::Tenant => concat!("admin", ".sync", ".tenant"),
            Self::Condition => concat!("admin", ".sync", ".condition"),
        }
    }
}

impl Serialize for Route {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.id())
    }
}

impl Route {
    /// The route for a condition's subject kind.
    #[must_use]
    pub fn for_subject_kind(kind: &str) -> Self {
        match kind {
            "node" => Self::Node,
            "record" => Self::Record,
            "stream" => Self::Stream,
            "tenant" => Self::Tenant,
            _ => Self::Condition,
        }
    }
}

/// One row of the `needs you` list: every open item with the same remedy, collapsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NeedsYou {
    /// The condition ids in the row, sorted, without duplicates.
    pub conditions: Vec<String>,
    /// The worst severity in the row.
    pub severity: Severity,
    /// The message key of what to do.
    pub remedy_key: String,
    /// The parameters of the worst, oldest item, for the explanation (ids and formatted values).
    pub params: ConditionParams,
    /// When the oldest item in the row opened (hub time).
    pub since: Millis,
    /// How many items the row stands for.
    pub count: usize,
    /// Up to [`SAMPLE_SIZE`] subjects, oldest first. Ids only.
    pub subjects_sample: Vec<String>,
    /// How many items are not in the sample (the `+N more`).
    pub more: usize,
    /// Where the link goes.
    pub link: Route,
}

/// One node as the hub sees it, as of `as_of`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeRow {
    /// The node id.
    pub node: String,
    /// When the hub last heard from the node (hub time).
    pub last_contact: Millis,
    /// How long ago that was at the view's `now`.
    pub silent_for: Millis,
    /// Positions between the node's applied prefix and the hub's head.
    pub lag_positions: u64,
    /// Estimated time to catch up at the node's recent rate, if it has one.
    pub lag_estimate: Option<Millis>,
    /// The software version tag the node reported.
    pub version: String,
    /// What the freshness and skew checks say.
    pub status: NodeStatus,
    /// The worst of the status and the node's open conditions.
    pub colour: Colour,
    /// The hub's view is as-of this time (ADR-065 section 8), not now.
    pub as_of: Millis,
}

/// How much is open, at every level, so a count never needs a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Open `info` conditions.
    pub info: usize,
    /// Open `amber` conditions.
    pub amber: usize,
    /// Open `red` conditions.
    pub red: usize,
    /// Open items that need a person (before grouping).
    pub needs_you_items: usize,
    /// Rows in the `needs you` list before the cap.
    pub needs_you_rows: usize,
    /// Nodes known, before the cap.
    pub nodes: usize,
}

/// The whole view, for one tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthView {
    /// The one word.
    pub status: Status,
    /// The message key of the one-line headline.
    pub headline_key: String,
    /// The time the view was built (hub time).
    pub now: Millis,
    /// The short list. At most [`MAX_ROWS`] rows.
    pub needs_you: Vec<NeedsYou>,
    /// Node rows, worst first. At most [`MAX_ROWS`] rows.
    pub nodes: Vec<NodeRow>,
    /// The counts.
    pub counts: Counts,
    /// True when either list was cut at [`MAX_ROWS`].
    pub truncated: bool,
}

impl HealthView {
    /// The stable JSON form: compact, keys in declaration order, parameters sorted by name.
    ///
    /// # Errors
    ///
    /// Never in practice: every field is plain data. The error is serde's, kept so the caller
    /// need not trust that.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}
