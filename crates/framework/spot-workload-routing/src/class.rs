//! Workload classes: what kind of work a request or job is (ADR-24).
//!
//! The class is the routing engine's shared vocabulary with `spot-ai-isolation` (WP-AI-05). Both
//! sides agree on this enum, which lives here because the router decides *where* work runs first;
//! admission control adapts later. A class is the coarse "lane" of the platform:
//!
//! - [`WorkloadClass::Interactive`] — a user is waiting; latency matters most.
//! - [`WorkloadClass::Report`] — a report or dashboard query; may be heavy, staleness-tolerant.
//! - [`WorkloadClass::Bulk`] — a bulk export or import; long-running, throughput-bound.
//! - [`WorkloadClass::Ai`] — model inference or retrieval; heavy and bursty.
//! - [`WorkloadClass::Background`] — schedules, cleanup, maintenance; may yield to everything.
//!
//! [`WorkloadClass::is_heavy`] is the "heavy lane" definition the brief shares with
//! `spot-ai-isolation`: report, bulk and AI work must never share a worker pool with interactive
//! work, because one long job would otherwise sit in front of a user's click.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::queue::QueueName;

/// The lane a request or job belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkloadClass {
    /// A user is waiting on the result.
    Interactive,
    /// A report or dashboard query.
    Report,
    /// A bulk export or import.
    Bulk,
    /// Model inference or retrieval.
    Ai,
    /// Schedules, cleanup and maintenance.
    Background,
}

impl WorkloadClass {
    /// Every class, in declaration order. The policy validator and the catalogue tests walk it.
    pub const ALL: [WorkloadClass; 5] = [
        WorkloadClass::Interactive,
        WorkloadClass::Report,
        WorkloadClass::Bulk,
        WorkloadClass::Ai,
        WorkloadClass::Background,
    ];

    /// The class' canonical name: lower-case, one word, as it appears in a policy document.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadClass::Interactive => "interactive",
            WorkloadClass::Report => "report",
            WorkloadClass::Bulk => "bulk",
            WorkloadClass::Ai => "ai",
            WorkloadClass::Background => "background",
        }
    }

    /// Parse a class name.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownClass`] when the name is not one of [`WorkloadClass::ALL`].
    pub fn parse(name: &str) -> Result<Self> {
        WorkloadClass::ALL
            .into_iter()
            .find(|class| class.as_str() == name)
            .ok_or_else(|| Error::UnknownClass {
                name: name.to_owned(),
            })
    }

    /// The queue a node must consume to serve this class. This is the link between a class and
    /// [`crate::policy::RoutingPolicy::queues_for`]: a node whose queues include this one serves
    /// the class.
    #[must_use]
    pub fn queue(self) -> QueueName {
        QueueName::new(match self {
            WorkloadClass::Interactive => "interactive",
            WorkloadClass::Report => "reports",
            WorkloadClass::Bulk => "exports",
            WorkloadClass::Ai => "ai",
            WorkloadClass::Background => "background",
        })
    }

    /// Whether the class belongs to the "heavy lane": report, bulk and AI work share a pool that
    /// never carries interactive traffic. This is the definition `spot-ai-isolation` consumes.
    #[must_use]
    pub fn is_heavy(self) -> bool {
        matches!(
            self,
            WorkloadClass::Report | WorkloadClass::Bulk | WorkloadClass::Ai
        )
    }

    /// Whether a user is waiting on the result (the latency-sensitive lane).
    #[must_use]
    pub fn is_interactive(self) -> bool {
        self == WorkloadClass::Interactive
    }
}

impl std::fmt::Display for WorkloadClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for class in WorkloadClass::ALL {
            assert_eq!(WorkloadClass::parse(class.as_str()).expect("parses"), class);
        }
    }

    #[test]
    fn unknown_name_is_typed() {
        let err = WorkloadClass::parse("ledger").expect_err("unknown");
        assert_eq!(err.code(), crate::codes::UNKNOWN_CLASS);
    }

    #[test]
    fn heavy_lane_is_report_bulk_ai() {
        assert!(!WorkloadClass::Interactive.is_heavy());
        assert!(WorkloadClass::Report.is_heavy());
        assert!(WorkloadClass::Bulk.is_heavy());
        assert!(WorkloadClass::Ai.is_heavy());
        assert!(!WorkloadClass::Background.is_heavy());
    }

    #[test]
    fn each_class_has_its_own_queue() {
        let queues: Vec<String> = WorkloadClass::ALL
            .into_iter()
            .map(|class| class.queue().into_string())
            .collect();
        let mut unique = queues.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), queues.len());
    }
}
