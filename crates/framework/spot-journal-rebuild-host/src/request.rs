//! What an operator asks for and the three modes it becomes.

use serde::{Deserialize, Serialize};

/// Where the rebuild ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetSpec {
    /// The end of the journal: positions are preserved.
    Latest,
    /// A position of a generation (`at <generation>:<seq>`): a point in time, which forks.
    At {
        /// The generation.
        generation: u64,
        /// The sequence number.
        seq: u64,
    },
    /// The last position recorded before this instant (`before <timestamp>`), resolved by the
    /// journal side.
    Before {
        /// Unix milliseconds.
        millis: i64,
    },
}

/// A rebuild request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildRequest {
    /// The site (and so the tenant's database) to rebuild.
    pub site: String,
    /// The tenant the journal must belong to.
    pub tenant: String,
    /// Where it ends.
    pub target: TargetSpec,
    /// The recorded head `(generation, seq)` of a database restored from a backup, for a
    /// roll-forward; `None` rebuilds into an empty database.
    pub backup_head: Option<(u64, u64)>,
    /// Print the plan and its cost without touching the database.
    pub dry_run: bool,
    /// Images between two durable checkpoints (default 5,000).
    pub checkpoint_every: u64,
}

impl RebuildRequest {
    /// A request for `site` of `tenant` to the end of the journal into an empty database.
    #[must_use]
    pub fn latest(site: &str, tenant: &str) -> Self {
        Self {
            site: site.to_owned(),
            tenant: tenant.to_owned(),
            target: TargetSpec::Latest,
            backup_head: None,
            dry_run: false,
            checkpoint_every: 5_000,
        }
    }
}

/// Which of the three modes ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// An empty database: the latest verified snapshot plus every segment after it.
    Full,
    /// A database restored from a backup: only the segments after the backup's head.
    RollForward,
    /// A position before the end: a new generation, the abandoned one stays in the journal.
    PointInTime,
}
