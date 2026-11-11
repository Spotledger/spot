//! Fault injection over the in-memory stack.
//!
//! A [`FaultSpec`] is the typed injection a corpus row carries. [`FaultSpec::inject`] seeds the
//! matching subsystem of [`spot_remedies::FakeWorld`] — the in-memory stand-in the remedy
//! framework defines for jobs, cache, retrieval indexes, edge sync, quarantine, BPM cases and
//! documents — and the simulator observes the result through the same typed ports the real
//! subsystems implement.
//!
//! Deliberately absent: any handle on a real database, clock or network. A fault is data, the
//! injection is a seed call, and the same row produces the same world every run.

use std::collections::BTreeMap;

use serde::Deserialize;
use spot_remedies::fixture::FakeWorld;
use spot_remedies::{CaseLife, DocState, IndexHealth, JobLife, QuarantineState};

/// One fault to inject.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FaultSpec {
    /// A job exhausted its attempts and is failed.
    JobFailed {
        /// The job id.
        job: String,
        /// How many attempts it made.
        #[serde(default = "default_attempts")]
        attempts: u32,
    },
    /// A job is running under a lease that has expired.
    JobStuck {
        /// The job id.
        job: String,
        /// When the lease expired (unix seconds).
        lease_expires_at: i64,
    },
    /// A BPM case is stuck.
    CaseStuck {
        /// The case id.
        case_id: String,
    },
    /// A sync item is held in quarantine.
    QuarantineHeld {
        /// The quarantine item id.
        item: String,
    },
    /// A cache namespace holds poisoned entries.
    CachePoisoned {
        /// The namespace.
        namespace: String,
        /// How many entries it holds.
        entries: usize,
    },
    /// The permission cache is stale after a role assignment changed.
    PermissionCacheStale {
        /// The namespace.
        namespace: String,
        /// How many entries it holds.
        entries: usize,
    },
    /// A configuration change left the config cache stale.
    ConfigCacheStale {
        /// The namespace.
        namespace: String,
        /// How many entries it holds.
        entries: usize,
    },
    /// A retrieval index is stale.
    IndexStale {
        /// The index name.
        index: String,
        /// Its generation.
        #[serde(default)]
        generation: u64,
    },
    /// Edge sync has drifted from its source.
    EdgeDrift {
        /// The edge reference.
        edge: String,
        /// The count the source says.
        expected: u64,
        /// The count the edge holds.
        actual: u64,
    },
    /// A failed app upgrade left generated state stale.
    AppUpgradeStale {
        /// The index left stale.
        index: String,
        /// Its generation.
        #[serde(default)]
        generation: u64,
        /// An optional cache namespace left stale.
        #[serde(default)]
        namespace: Option<String>,
        /// How many entries that namespace holds.
        #[serde(default)]
        entries: usize,
    },
    /// A submitted financial document's derived field is wrong; the hard floor applies.
    FinanceDocument {
        /// The document reference.
        document: String,
        /// The field that looks wrong.
        field: String,
    },
}

/// The default attempt count for a failed job.
fn default_attempts() -> u32 {
    5
}

impl FaultSpec {
    /// Seeds the fault into the in-memory tenant.
    pub fn inject(&self, world: &mut FakeWorld) {
        match self {
            Self::JobFailed { job, attempts } => {
                world.jobs.seed(job, JobLife::Failed, *attempts, None);
            }
            Self::JobStuck {
                job,
                lease_expires_at,
            } => {
                world
                    .jobs
                    .seed(job, JobLife::Running, 1, Some(*lease_expires_at));
            }
            Self::CaseStuck { case_id } => world.case.seed(case_id, CaseLife::Stuck),
            Self::QuarantineHeld { item } => world.quarantine.seed(item, QuarantineState::Held),
            Self::CachePoisoned { namespace, entries }
            | Self::PermissionCacheStale { namespace, entries }
            | Self::ConfigCacheStale { namespace, entries } => {
                world.cache.seed(namespace, *entries)
            }
            Self::IndexStale { index, generation } => {
                world.index.seed(index, IndexHealth::Stale, *generation);
            }
            Self::EdgeDrift {
                edge,
                expected,
                actual,
            } => world.edge.seed(edge, *expected, *actual),
            Self::AppUpgradeStale {
                index,
                generation,
                namespace,
                entries,
            } => {
                world.index.seed(index, IndexHealth::Stale, *generation);
                if let Some(namespace) = namespace {
                    world.cache.seed(namespace, *entries);
                }
            }
            Self::FinanceDocument { document, .. } => {
                world.document.seed(document, DocState::Submitted, true);
            }
        }
    }

    /// The remedy parameters the fault's targets supply, keyed by the parameter name a remedy
    /// declares. A scripted agent fills a remedy's required parameters from this map; nothing
    /// here comes from the incident's expected resolution.
    #[must_use]
    pub fn targets(&self) -> BTreeMap<String, String> {
        let mut targets = BTreeMap::new();
        match self {
            Self::JobFailed { job, .. } | Self::JobStuck { job, .. } => {
                targets.insert("job_id".to_owned(), job.clone());
            }
            Self::CaseStuck { case_id } => {
                targets.insert("case_id".to_owned(), case_id.clone());
            }
            Self::QuarantineHeld { item } => {
                targets.insert("quarantine_id".to_owned(), item.clone());
            }
            Self::CachePoisoned { namespace, entries }
            | Self::PermissionCacheStale { namespace, entries }
            | Self::ConfigCacheStale { namespace, entries } => {
                targets.insert("namespace".to_owned(), namespace.clone());
                targets.insert("entries".to_owned(), entries.to_string());
            }
            Self::IndexStale { index, .. } => {
                targets.insert("index".to_owned(), index.clone());
            }
            Self::EdgeDrift {
                edge,
                expected,
                actual,
            } => {
                targets.insert("edge".to_owned(), edge.clone());
                targets.insert("expected".to_owned(), expected.to_string());
                targets.insert("actual".to_owned(), actual.to_string());
            }
            Self::AppUpgradeStale {
                index,
                namespace,
                entries,
                ..
            } => {
                targets.insert("index".to_owned(), index.clone());
                if let Some(namespace) = namespace {
                    targets.insert("namespace".to_owned(), namespace.clone());
                    targets.insert("entries".to_owned(), entries.to_string());
                }
            }
            Self::FinanceDocument { document, field } => {
                targets.insert("document".to_owned(), document.clone());
                targets.insert("field".to_owned(), field.clone());
            }
        }
        targets
    }

    /// The signals the injected state itself produces, before the corpus row's own symptoms.
    #[must_use]
    pub fn signals(&self) -> Vec<String> {
        let signals: &[&str] = match self {
            Self::JobFailed { .. } => &[
                concat!("job", ".", "failed"),
                concat!("job", ".", "attempts_exhausted"),
            ],
            Self::JobStuck { .. } => &[
                concat!("job", ".", "running"),
                concat!("job", ".", "lease_expired"),
            ],
            Self::CaseStuck { .. } => &[concat!("case", ".", "stuck")],
            Self::QuarantineHeld { .. } => &[
                concat!("quarantine", ".", "held"),
                concat!("sync", ".", "item_quarantined"),
            ],
            Self::CachePoisoned { .. } => &[
                concat!("cache", ".", "poisoned"),
                concat!("cache", ".", "optimistic"),
            ],
            Self::PermissionCacheStale { .. } => &[
                concat!("permission", ".", "stale"),
                concat!("cache", ".", "optimistic"),
            ],
            Self::ConfigCacheStale { .. } => &[
                concat!("config", ".", "stale"),
                concat!("cache", ".", "optimistic"),
            ],
            Self::IndexStale { .. } => &[concat!("index", ".", "stale")],
            Self::EdgeDrift { .. } => &[concat!("edge", ".", "drift")],
            Self::AppUpgradeStale { .. } => &[
                concat!("app", ".", "upgrade_failed"),
                concat!("index", ".", "stale"),
            ],
            Self::FinanceDocument { .. } => &[
                concat!("document", ".", "submitted_financial"),
                concat!("finance", ".", "field_wrong"),
            ],
        };
        signals.iter().map(|signal| (*signal).to_owned()).collect()
    }

    /// The state target the fault created (`job:job-1041`, `cache:config`, ...).
    #[must_use]
    pub fn primary_target(&self) -> String {
        match self {
            Self::JobFailed { job, .. } | Self::JobStuck { job, .. } => format!("job:{job}"),
            Self::CaseStuck { case_id } => format!("case:{case_id}"),
            Self::QuarantineHeld { item } => format!("quarantine:{item}"),
            Self::CachePoisoned { namespace, .. }
            | Self::PermissionCacheStale { namespace, .. }
            | Self::ConfigCacheStale { namespace, .. } => format!("cache:{namespace}"),
            Self::IndexStale { index, .. } => format!("index:{index}"),
            Self::EdgeDrift { edge, .. } => format!("edge:{edge}"),
            Self::AppUpgradeStale { index, .. } => format!("index:{index}"),
            Self::FinanceDocument { document, .. } => format!("doc:{document}"),
        }
    }

    /// Whether the success target names a record this fault injected.
    #[must_use]
    pub fn matches_target(&self, target: &str) -> bool {
        if target == self.primary_target() {
            return true;
        }
        matches!(
            (self, split_target(target)),
            (Self::AppUpgradeStale { namespace: Some(namespace), .. }, Some(("cache", value)))
                if namespace.as_str() == value
        )
    }

    /// A one-line, secret-free description for the report.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::JobFailed { job, attempts } => {
                format!("job `{job}` failed after {attempts} attempts")
            }
            Self::JobStuck {
                job,
                lease_expires_at,
            } => format!("job `{job}` is running with a lease expired at {lease_expires_at}"),
            Self::CaseStuck { case_id } => format!("case `{case_id}` is stuck"),
            Self::QuarantineHeld { item } => format!("quarantine item `{item}` is held"),
            Self::CachePoisoned { namespace, entries } => {
                format!("cache namespace `{namespace}` holds {entries} poisoned entries")
            }
            Self::PermissionCacheStale { namespace, entries } => {
                format!("permission cache `{namespace}` holds {entries} stale entries")
            }
            Self::ConfigCacheStale { namespace, entries } => {
                format!("config cache `{namespace}` holds {entries} stale entries")
            }
            Self::IndexStale { index, .. } => format!("retrieval index `{index}` is stale"),
            Self::EdgeDrift {
                edge,
                expected,
                actual,
            } => format!("edge `{edge}` holds {actual} edges, expected {expected}"),
            Self::AppUpgradeStale { index, .. } => {
                format!("a failed app upgrade left index `{index}` stale")
            }
            Self::FinanceDocument { document, field } => format!(
                "submitted financial document `{document}` has a wrong derived field `{field}`"
            ),
        }
    }
}

/// Splits `kind:value` into its two parts.
fn split_target(target: &str) -> Option<(&str, &str)> {
    target.split_once(':')
}
