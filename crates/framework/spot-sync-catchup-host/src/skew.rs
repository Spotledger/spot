//! Version skew at the hub: the node's decode capability is recorded at `hello`, a record outside
//! it is held (with whatever depends on it), and a node that reconnects with a higher capability
//! has its holds released in position order.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use spot_sync_skew::{Decision, Major, NodeReport, Settings, session_decision};

/// What the hub knows of a node's last hello.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Capability {
    software: (u32, u32, u32),
    decodable: BTreeMap<String, BTreeSet<Major>>,
}

fn capability_of(report: &NodeReport) -> Capability {
    let v = report.software_version;
    Capability {
        software: (v.major, v.minor, v.patch),
        decodable: report
            .decodable_majors
            .iter()
            .map(|(topic, majors)| (topic.to_string(), majors.clone()))
            .collect(),
    }
}

/// The answer to a hello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// Whether the session runs (the skew library's decision, with its catalogue code when not).
    pub decision: Decision,
    /// The node came back with a higher capability than its last hello: release its holds, queue
    /// the held records again in position order.
    pub upgraded: bool,
}

/// The hub's side of version skew.
pub struct SkewGate {
    hub: NodeReport,
    settings: Settings,
    seen: Mutex<BTreeMap<String, Capability>>,
}

impl SkewGate {
    /// A gate for a hub with this report and `allowed_release_skew`.
    #[must_use]
    pub fn new(hub: NodeReport, allowed_release_skew: u32) -> Self {
        Self {
            hub,
            settings: Settings {
                allowed_release_skew,
            },
            seen: Mutex::new(BTreeMap::new()),
        }
    }

    /// Records the node's capability and decides the session.
    pub fn on_hello(&self, node: &str, report: &NodeReport) -> Hello {
        let now = capability_of(report);
        let previous = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(node.to_owned(), now.clone());
        let upgraded = previous.is_some_and(|before| {
            now.software > before.software
                || now.decodable.iter().any(|(topic, majors)| {
                    before
                        .decodable
                        .get(topic)
                        .is_none_or(|old| !majors.is_subset(old))
                })
        });
        Hello {
            decision: session_decision(&self.hub, report, self.settings),
            upgraded,
        }
    }

    /// Whether a record of `topic` at `major` must be held for this node (it cannot decode it).
    /// A record that depends on a held record is held with it by the caller ("dependency held").
    #[must_use]
    pub fn must_hold(&self, node: &NodeReport, topic: &str, major: Major) -> bool {
        !node.can_decode(topic, major)
    }
}
