//! The routing policy: settings-driven, hot-reloadable data (ADR-24).
//!
//! A [`RoutingPolicy`] is a document, not code: the platform's settings schema carries it, and an
//! operator can change it without a redeploy. It has three parts:
//!
//! - [`QueueAffinityRule`] — which job queues a node's workers consume. A node A that consumes
//!   `default` and `interactive` never claims a report; a node B that consumes `reports` and
//!   `exports` never claims a click.
//! - [`PathRule`] — a path prefix, exact path or route name that must be served by a labelled set
//!   of nodes (for example `/api/v1/report/` on the `report` nodes).
//! - [`ReplicaReadRule`] — a staleness-tolerant, read-only endpoint whose queries may read a
//!   replica within a maximum lag.
//!
//! [`PolicyStore`] holds the live document behind an [`RwLock`] of an [`Arc`], so a reader takes
//! one snapshot and finishes on the version it started with, while [`PolicyStore::replace`]
//! publishes a new one. This is the hot-reload seam: the settings watcher calls `replace`, every
//! in-flight request keeps its old `Arc`.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::class::WorkloadClass;
use crate::error::{Error, Result};
use crate::node::{NodeProfile, NodeRole, NodeSelector};
use crate::queue::QueueName;

/// The default hop budget: a request may be forwarded at most this many times.
pub const DEFAULT_MAX_HOPS: u32 = 4;
/// The default read-your-writes window: two seconds after a session's last write.
pub const DEFAULT_READ_YOUR_WRITES_MS: u64 = 2_000;
/// The default interactive latency budget: 200 ms.
pub const DEFAULT_INTERACTIVE_BUDGET_MS: u64 = 200;

/// How a [`PathRule`] or [`ReplicaReadRule`] recognises a request.
///
/// Exactly one of the three fields is set. A matcher carries a *specificity* so the router can
/// pick the most specific rule when several match: a route name beats an exact path, which beats
/// a prefix.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathMatcher {
    /// Match any path that starts with this string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Match this exact path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<String>,
    /// Match this route name (the native API's route, e.g. the report-run route).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

impl PathMatcher {
    /// A prefix matcher.
    #[must_use]
    pub fn prefix(value: impl Into<String>) -> Self {
        Self {
            prefix: Some(value.into()),
            exact: None,
            route: None,
        }
    }

    /// An exact-path matcher.
    #[must_use]
    pub fn exact(value: impl Into<String>) -> Self {
        Self {
            prefix: None,
            exact: Some(value.into()),
            route: None,
        }
    }

    /// A route-name matcher.
    #[must_use]
    pub fn route(value: impl Into<String>) -> Self {
        Self {
            prefix: None,
            exact: None,
            route: Some(value.into()),
        }
    }

    /// Whether exactly one of the three fields is set and non-empty.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let set = [
            self.prefix.as_deref(),
            self.exact.as_deref(),
            self.route.as_deref(),
        ]
        .into_iter()
        .filter(|v| v.is_some_and(|s| !s.is_empty()))
        .count();
        set == 1
    }

    /// The most specific match of this matcher against a request path and optional route name,
    /// or `None`. Higher is more specific; the router keeps the maximum.
    #[must_use]
    pub fn specificity(&self, path: &str, route: Option<&str>) -> Option<u64> {
        // Route names are the strongest signal (rank 2), then exact paths (rank 1), then
        // prefixes (rank 0); ties break on the length of the matched text.
        const ROUTE_RANK: u64 = 2 << 32;
        const EXACT_RANK: u64 = 1 << 32;
        if let Some(route_name) = &self.route {
            if route == Some(route_name.as_str()) {
                return Some(ROUTE_RANK + route_name.len() as u64);
            }
            return None;
        }
        if let Some(exact) = &self.exact {
            if path == exact {
                return Some(EXACT_RANK + exact.len() as u64);
            }
            return None;
        }
        if let Some(prefix) = &self.prefix
            && path.starts_with(prefix.as_str())
        {
            return Some(prefix.len() as u64);
        }
        None
    }
}

/// A queue-affinity rule: nodes matching [`NodeSelector`] consume these queues.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueAffinityRule {
    /// Which nodes the rule is about; empty matches every node.
    #[serde(default)]
    pub selector: NodeSelector,
    /// The queues those nodes' workers consume.
    pub queues: Vec<QueueName>,
}

/// A path rule: requests matching [`PathMatcher`] belong to a workload class and are served by
/// nodes matching [`NodeSelector`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    /// Which requests the rule is about.
    pub matcher: PathMatcher,
    /// Which nodes must serve them.
    pub target: NodeSelector,
    /// The workload class the matched requests belong to (defaults to the request's own class).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<WorkloadClass>,
    /// A per-rule hop budget, overriding the policy's [`RoutingPolicy::max_hops`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hops: Option<u32>,
}

/// A replica-read rule: matching, read-only, staleness-tolerant requests may read a replica whose
/// lag is at most [`ReplicaReadRule::max_lag`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaReadRule {
    /// Which requests the rule is about.
    pub matcher: PathMatcher,
    /// The maximum acceptable replication lag, in milliseconds.
    pub max_lag_ms: u64,
    /// Whether the endpoint tolerates stale data. A rule that says `false` is a contradiction and
    /// fails validation: a replica read that does not tolerate staleness is not a replica read.
    #[serde(default = "default_true")]
    pub staleness_tolerant: bool,
}

impl ReplicaReadRule {
    /// The maximum lag as a [`Duration`].
    #[must_use]
    pub fn max_lag(&self) -> Duration {
        Duration::from_millis(self.max_lag_ms)
    }
}

fn default_true() -> bool {
    true
}

/// The whole routing policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingPolicy {
    /// Queue affinity per node, first match wins.
    #[serde(default)]
    pub queue_affinity: Vec<QueueAffinityRule>,
    /// Path and route rules, most specific match wins.
    #[serde(default)]
    pub path_rules: Vec<PathRule>,
    /// Staleness-tolerant read rules, most specific match wins.
    #[serde(default)]
    pub replica_reads: Vec<ReplicaReadRule>,
    /// The forward hop budget.
    #[serde(default = "default_max_hops")]
    pub max_hops: u32,
    /// The read-your-writes window after a session's last write, in milliseconds.
    #[serde(default = "default_read_your_writes_ms")]
    pub read_your_writes_ms: u64,
    /// The interactive latency budget the admission layer and the load simulation use, in
    /// milliseconds.
    #[serde(default = "default_interactive_budget_ms")]
    pub interactive_budget_ms: u64,
}

fn default_max_hops() -> u32 {
    DEFAULT_MAX_HOPS
}

fn default_read_your_writes_ms() -> u64 {
    DEFAULT_READ_YOUR_WRITES_MS
}

fn default_interactive_budget_ms() -> u64 {
    DEFAULT_INTERACTIVE_BUDGET_MS
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            queue_affinity: Vec::new(),
            path_rules: Vec::new(),
            replica_reads: Vec::new(),
            max_hops: DEFAULT_MAX_HOPS,
            read_your_writes_ms: DEFAULT_READ_YOUR_WRITES_MS,
            interactive_budget_ms: DEFAULT_INTERACTIVE_BUDGET_MS,
        }
    }
}

impl RoutingPolicy {
    /// A sensible starting policy for a cluster with dedicated nodes: interactive and gateway
    /// nodes consume `default` and `interactive`, report nodes consume `reports` and `exports`,
    /// AI nodes consume `ai`, and the native API's report route family is routed to the report
    /// nodes with a staleness-tolerant replica read. An operator overrides any part of it in the
    /// settings document; the report prefix is the API's own route family and may be narrowed.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            queue_affinity: vec![
                QueueAffinityRule {
                    selector: NodeSelector::roles(vec![NodeRole::Interactive, NodeRole::Gateway]),
                    queues: vec![QueueName::new("default"), QueueName::new("interactive")],
                },
                QueueAffinityRule {
                    selector: NodeSelector::roles(vec![NodeRole::Report]),
                    queues: vec![QueueName::new("reports"), QueueName::new("exports")],
                },
                QueueAffinityRule {
                    selector: NodeSelector::roles(vec![NodeRole::Ai]),
                    queues: vec![QueueName::new("ai")],
                },
                QueueAffinityRule {
                    selector: NodeSelector::roles(vec![NodeRole::Bulk]),
                    queues: vec![QueueName::new("exports")],
                },
            ],
            path_rules: vec![PathRule {
                matcher: PathMatcher::prefix("/api/v1/report/"),
                target: NodeSelector::roles(vec![NodeRole::Report]),
                class: Some(WorkloadClass::Report),
                max_hops: None,
            }],
            replica_reads: vec![ReplicaReadRule {
                matcher: PathMatcher::prefix("/api/v1/report/"),
                max_lag_ms: 5_000,
                staleness_tolerant: true,
            }],
            ..Self::default()
        }
    }

    /// Parse a policy from TOML or JSON: TOML is tried first, JSON second, so a settings document
    /// of either shape loads.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPolicy`] when neither parser accepts the document.
    pub fn parse(text: &str) -> Result<Self> {
        match toml::from_str::<RoutingPolicy>(text) {
            Ok(policy) => Ok(policy),
            Err(toml_error) => serde_json::from_str::<RoutingPolicy>(text).map_err(|json_error| {
                Error::InvalidPolicy {
                    reason: format!(
                        "the policy is neither valid TOML ({toml_error}) nor valid JSON \
                         ({json_error})"
                    ),
                }
            }),
        }
    }

    /// Parse a policy from TOML.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPolicy`] when the TOML does not describe a policy.
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::InvalidPolicy {
            reason: format!("invalid policy TOML: {e}"),
        })
    }

    /// Parse a policy from JSON.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPolicy`] when the JSON does not describe a policy.
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text).map_err(|e| Error::InvalidPolicy {
            reason: format!("invalid policy JSON: {e}"),
        })
    }

    /// The read-your-writes window as a [`Duration`].
    #[must_use]
    pub fn read_your_writes_window(&self) -> Duration {
        Duration::from_millis(self.read_your_writes_ms)
    }

    /// The interactive latency budget as a [`Duration`].
    #[must_use]
    pub fn interactive_budget(&self) -> Duration {
        Duration::from_millis(self.interactive_budget_ms)
    }

    /// The queues a node's workers consume: the union of the queues of every matching affinity
    /// rule, in first-seen order. When no rule matches, the node consumes the `default` queue.
    #[must_use]
    pub fn queues_for(&self, node: &NodeProfile) -> Vec<QueueName> {
        let mut queues: Vec<QueueName> = Vec::new();
        for rule in &self.queue_affinity {
            if rule.selector.matches(node) {
                for queue in &rule.queues {
                    if !queues.contains(queue) {
                        queues.push(queue.clone());
                    }
                }
            }
        }
        if queues.is_empty() {
            queues.push(QueueName::new("default"));
        }
        queues
    }

    /// Whether a node serves a workload class: its queues include the class' canonical queue.
    #[must_use]
    pub fn node_serves(&self, node: &NodeProfile, class: WorkloadClass) -> bool {
        self.queues_for(node).contains(&class.queue())
    }

    /// The most specific path rule matching a path and optional route name.
    #[must_use]
    pub fn path_rule_for(&self, path: &str, route: Option<&str>) -> Option<&PathRule> {
        self.path_rules
            .iter()
            .filter_map(|rule| rule.matcher.specificity(path, route).map(|s| (s, rule)))
            .max_by_key(|(s, _)| *s)
            .map(|(_, rule)| rule)
    }

    /// The most specific replica-read rule matching a path and optional route name.
    #[must_use]
    pub fn replica_rule_for(&self, path: &str, route: Option<&str>) -> Option<&ReplicaReadRule> {
        self.replica_reads
            .iter()
            .filter_map(|rule| rule.matcher.specificity(path, route).map(|s| (s, rule)))
            .max_by_key(|(s, _)| *s)
            .map(|(_, rule)| rule)
    }

    /// Validate the whole document, reporting the first problem.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPolicy`] naming the offending rule.
    pub fn validate(&self) -> Result<()> {
        if self.max_hops == 0 {
            return Err(Error::InvalidPolicy {
                reason: "max_hops must be at least 1".to_owned(),
            });
        }
        if self.interactive_budget_ms == 0 {
            return Err(Error::InvalidPolicy {
                reason: "interactive_budget_ms must be greater than zero".to_owned(),
            });
        }
        for (index, rule) in self.queue_affinity.iter().enumerate() {
            if rule.queues.is_empty() {
                return Err(Error::InvalidPolicy {
                    reason: format!("queue_affinity rule {index} names no queues"),
                });
            }
            for queue in &rule.queues {
                if !queue.is_valid() {
                    return Err(Error::InvalidPolicy {
                        reason: format!("queue_affinity rule {index} has an invalid queue name"),
                    });
                }
            }
        }
        let mut seen: Vec<(u8, String)> = Vec::new();
        for (index, rule) in self.path_rules.iter().enumerate() {
            if !rule.matcher.is_valid() {
                return Err(Error::InvalidPolicy {
                    reason: format!(
                        "path_rules rule {index} must set exactly one of prefix, exact or route"
                    ),
                });
            }
            if rule.target.is_empty() {
                return Err(Error::InvalidPolicy {
                    reason: format!("path_rules rule {index} targets no node selector"),
                });
            }
            if rule.max_hops == Some(0) {
                return Err(Error::InvalidPolicy {
                    reason: format!("path_rules rule {index} sets max_hops to zero"),
                });
            }
            let key = matcher_key(&rule.matcher);
            if seen.contains(&key) {
                return Err(Error::InvalidPolicy {
                    reason: format!("path_rules rule {index} duplicates an earlier matcher"),
                });
            }
            seen.push(key);
        }
        for (index, rule) in self.replica_reads.iter().enumerate() {
            if !rule.matcher.is_valid() {
                return Err(Error::InvalidPolicy {
                    reason: format!(
                        "replica_reads rule {index} must set exactly one of prefix, exact or route"
                    ),
                });
            }
            if rule.max_lag_ms == 0 {
                return Err(Error::InvalidPolicy {
                    reason: format!("replica_reads rule {index} sets max_lag_ms to zero"),
                });
            }
            if !rule.staleness_tolerant {
                return Err(Error::InvalidPolicy {
                    reason: format!(
                        "replica_reads rule {index} is not staleness_tolerant, which contradicts \
                         a replica read"
                    ),
                });
            }
        }
        Ok(())
    }
}

/// A comparable key for a matcher, for duplicate detection: the kind (0 prefix, 1 exact,
/// 2 route) and the matched text.
fn matcher_key(matcher: &PathMatcher) -> (u8, String) {
    if let Some(prefix) = &matcher.prefix {
        (0, prefix.clone())
    } else if let Some(exact) = &matcher.exact {
        (1, exact.clone())
    } else if let Some(route) = &matcher.route {
        (2, route.clone())
    } else {
        (3, String::new())
    }
}

/// The live policy, hot-reloadable.
///
/// Readers take an [`Arc`] snapshot with [`PolicyStore::load`]; the settings watcher publishes a
/// new document with [`PolicyStore::replace`]. No reader is ever blocked by a reload, and an
/// in-flight decision keeps the version it started with.
#[derive(Debug)]
pub struct PolicyStore {
    live: RwLock<Arc<RoutingPolicy>>,
}

impl Default for PolicyStore {
    fn default() -> Self {
        Self::new(RoutingPolicy::default())
    }
}

impl PolicyStore {
    /// A store over an initial policy.
    #[must_use]
    pub fn new(policy: RoutingPolicy) -> Self {
        Self {
            live: RwLock::new(Arc::new(policy)),
        }
    }

    /// The current policy snapshot.
    #[must_use]
    pub fn load(&self) -> Arc<RoutingPolicy> {
        match self.live.read() {
            Ok(guard) => Arc::clone(&guard),
            // A poisoned lock still holds a valid `Arc`; take it rather than panic in a request.
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Publish a new policy, returning the previous one. The new document is validated first, so a
    /// broken settings change never goes live.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPolicy`] when the new document fails validation; the live policy is left
    /// unchanged.
    pub fn replace(&self, policy: RoutingPolicy) -> Result<Arc<RoutingPolicy>> {
        policy.validate()?;
        let new = Arc::new(policy);
        let mut guard = match self.live.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let old = std::mem::replace(&mut *guard, new);
        Ok(old)
    }

    /// Parse and publish a policy document from TOML or JSON, returning the previous one.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidPolicy`] when the document does not parse or does not validate.
    pub fn reload(&self, text: &str) -> Result<Arc<RoutingPolicy>> {
        let policy = RoutingPolicy::parse(text)?;
        self.replace(policy)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::node::{Capacity, NodeRole};

    fn node(id: &str, roles: Vec<NodeRole>, labels: &[&str]) -> NodeProfile {
        NodeProfile::from_topology(
            id,
            roles,
            labels.iter().map(|l| (*l).to_owned()).collect(),
            Capacity::default(),
        )
    }

    fn sample_policy() -> RoutingPolicy {
        RoutingPolicy {
            queue_affinity: vec![
                QueueAffinityRule {
                    selector: NodeSelector::roles(vec![NodeRole::Interactive]),
                    queues: vec![QueueName::new("default"), QueueName::new("interactive")],
                },
                QueueAffinityRule {
                    selector: NodeSelector::roles(vec![NodeRole::Report]),
                    queues: vec![QueueName::new("reports"), QueueName::new("exports")],
                },
            ],
            path_rules: vec![PathRule {
                matcher: PathMatcher::prefix("/api/v1/report/"),
                target: NodeSelector::roles(vec![NodeRole::Report]),
                class: Some(WorkloadClass::Report),
                max_hops: None,
            }],
            replica_reads: vec![ReplicaReadRule {
                matcher: PathMatcher::prefix("/api/v1/report/"),
                max_lag_ms: 5_000,
                staleness_tolerant: true,
            }],
            ..RoutingPolicy::default()
        }
    }

    #[test]
    fn queues_for_unions_matching_rules() {
        let policy = sample_policy();
        let interactive = node("a", vec![NodeRole::Interactive], &[]);
        let report = node("c", vec![NodeRole::Report], &[]);
        assert_eq!(
            policy.queues_for(&interactive),
            vec![QueueName::new("default"), QueueName::new("interactive")]
        );
        assert_eq!(
            policy.queues_for(&report),
            vec![QueueName::new("reports"), QueueName::new("exports")]
        );
        assert!(policy.node_serves(&report, WorkloadClass::Report));
        assert!(!policy.node_serves(&interactive, WorkloadClass::Report));
    }

    #[test]
    fn unmatched_node_falls_back_to_default() {
        let policy = sample_policy();
        let plain = node("x", vec![NodeRole::Gateway], &[]);
        assert_eq!(policy.queues_for(&plain), vec![QueueName::new("default")]);
    }

    #[test]
    fn standard_policy_is_valid_and_covers_the_lanes() {
        let policy = RoutingPolicy::standard();
        policy.validate().expect("the standard policy is valid");
        assert!(policy.node_serves(
            &node("a", vec![NodeRole::Interactive], &[]),
            WorkloadClass::Interactive
        ));
        assert!(policy.node_serves(
            &node("c", vec![NodeRole::Report], &[]),
            WorkloadClass::Report
        ));
        assert!(policy.node_serves(&node("i", vec![NodeRole::Ai], &[]), WorkloadClass::Ai));
        let rule = policy
            .path_rule_for("/api/v1/report/sales/run", None)
            .expect("report route matches");
        assert_eq!(rule.class, Some(WorkloadClass::Report));
        assert!(
            policy
                .replica_rule_for("/api/v1/report/sales/run", None)
                .is_some()
        );
    }

    #[test]
    fn most_specific_path_rule_wins() {
        let mut policy = sample_policy();
        policy.path_rules.push(PathRule {
            matcher: PathMatcher::exact("/api/v1/report/long/run"),
            target: NodeSelector::labels(vec!["long-reports".to_owned()]),
            class: Some(WorkloadClass::Bulk),
            max_hops: Some(2),
        });
        let rule = policy
            .path_rule_for("/api/v1/report/long/run", None)
            .expect("matches");
        assert_eq!(rule.class, Some(WorkloadClass::Bulk));
        let rule = policy
            .path_rule_for("/api/v1/report/short/run", None)
            .expect("matches");
        assert_eq!(rule.class, Some(WorkloadClass::Report));
    }

    #[test]
    fn route_matcher_beats_prefix() {
        let mut policy = sample_policy();
        policy.path_rules.push(PathRule {
            matcher: PathMatcher::route("report.run"),
            target: NodeSelector::roles(vec![NodeRole::Report]),
            class: Some(WorkloadClass::Report),
            max_hops: None,
        });
        let rule = policy
            .path_rule_for("/api/v1/report/sales/run", Some("report.run"))
            .expect("matches");
        assert_eq!(rule.matcher.route.as_deref(), Some("report.run"));
    }

    #[test]
    fn validation_rejects_bad_documents() {
        let mut policy = sample_policy();
        policy.max_hops = 0;
        assert!(policy.validate().is_err());

        let mut policy = sample_policy();
        policy.path_rules[0].target = NodeSelector::default();
        assert!(policy.validate().is_err());

        let mut policy = sample_policy();
        let duplicate = policy.path_rules[0].clone();
        policy.path_rules.push(duplicate);
        assert!(policy.validate().is_err());

        let mut policy = sample_policy();
        policy.replica_reads[0].staleness_tolerant = false;
        assert!(policy.validate().is_err());

        let mut policy = sample_policy();
        policy.replica_reads[0].max_lag_ms = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn toml_and_json_load_the_same_document() {
        let toml = r#"
max_hops = 3
read_your_writes_ms = 1500

[[queue_affinity]]
queues = ["reports", "exports"]
[queue_affinity.selector]
roles = ["report"]

[[path_rules]]
class = "report"
[path_rules.matcher]
prefix = "/api/v1/report/"
[path_rules.target]
roles = ["report"]

[[replica_reads]]
max_lag_ms = 5000
[replica_reads.matcher]
prefix = "/api/v1/report/"
"#;
        let policy = RoutingPolicy::parse(toml).expect("parses");
        policy.validate().expect("valid");
        assert_eq!(policy.max_hops, 3);
        assert_eq!(policy.read_your_writes_ms, 1500);
        assert_eq!(policy.path_rules.len(), 1);
        assert_eq!(policy.replica_reads.len(), 1);

        let json = serde_json::to_string(&policy).expect("serialises");
        let again = RoutingPolicy::from_json(&json).expect("parses json");
        assert_eq!(policy, again);
    }

    #[test]
    fn store_reloads_and_validates_before_publishing() {
        let store = PolicyStore::new(sample_policy());
        let before = store.load();
        assert_eq!(before.max_hops, DEFAULT_MAX_HOPS);

        let mut next = sample_policy();
        next.max_hops = 7;
        let json = serde_json::to_string(&next).expect("serialises");
        let old = store.reload(&json).expect("reloads");
        assert_eq!(old.max_hops, DEFAULT_MAX_HOPS);
        assert_eq!(store.load().max_hops, 7);

        // A broken document is refused and the live policy is untouched.
        let broken = r#"max_hops = 0"#;
        assert!(store.reload(broken).is_err());
        assert_eq!(store.load().max_hops, 7);
    }
}
