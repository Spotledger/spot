//! Comparing two scope snapshots: findings, and the bisection that finds the records.
//!
//! [`reconcile`] takes the **owner's** snapshot as `local` and the **mirror's** as `remote`
//! (ADR-062: an owner is the only source of truth). It compares the two
//! [`Digest`](crate::Digest)s, and when the range roots differ it descends the two range tries
//! in lockstep: a subtree whose digest matches is skipped whole, so the cost grows with the
//! number of differences, not with the number of documents.
//!
//! Every finding is one of four kinds: [`Finding::MissingAtReceiver`],
//! [`Finding::ExtraAtReceiver`], [`Finding::VersionMismatch`] and [`Finding::TotalMismatch`].

use std::collections::BTreeMap;

use spot_storage_contract::{Name, Version};

use crate::digest::{Digest, HASH_LEN, MerkleTree, Node, RecordKey, ScopeSnapshot};
use crate::error::ReconcileResult;
use crate::scope::Scope;
use crate::source::DigestSource;

/// What a total mismatch is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Measure {
    /// The record count of an entity.
    Count {
        /// The entity.
        entity: Name,
    },
    /// A summed integer total field of an entity.
    Total {
        /// The entity.
        entity: Name,
        /// The field.
        field: Name,
    },
    /// The GL balance of an account.
    GlBalance {
        /// The account.
        account: String,
    },
    /// The stock quantity of an item.
    StockQuantity {
        /// The item.
        item: String,
    },
}

/// One difference between the owner and the mirror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finding {
    /// The owner holds a record the mirror does not.
    MissingAtReceiver {
        /// The record's entity.
        entity: Name,
        /// The record's name.
        name: String,
        /// The version the owner holds.
        version: Version,
    },
    /// The mirror holds a record the owner does not.
    ExtraAtReceiver {
        /// The record's entity.
        entity: Name,
        /// The record's name.
        name: String,
        /// The version the mirror holds.
        version: Version,
    },
    /// Both hold the record at different versions.
    VersionMismatch {
        /// The record's entity.
        entity: Name,
        /// The record's name.
        name: String,
        /// The version the owner holds.
        local: Version,
        /// The version the mirror holds.
        remote: Version,
    },
    /// A count, total, GL balance or stock quantity differs although the record set agrees.
    TotalMismatch {
        /// What differs.
        measure: Measure,
        /// The owner's value.
        local: i128,
        /// The mirror's value.
        remote: i128,
    },
}

impl Finding {
    /// A bounded label for this finding's kind: `missing`, `extra`, `version` or `total`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MissingAtReceiver { .. } => "missing",
            Self::ExtraAtReceiver { .. } => "extra",
            Self::VersionMismatch { .. } => "version",
            Self::TotalMismatch { .. } => "total",
        }
    }
}

/// The outcome of one reconciliation of one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The scope reconciled.
    pub scope: Scope,
    /// The differences found, in no guaranteed order.
    pub findings: Vec<Finding>,
    /// How many range-tree node pairs were compared (bisection rounds).
    pub rounds: usize,
    /// How many digest bytes were compared (two hashes per round).
    pub digest_bytes: usize,
}

impl ReconcileReport {
    /// True when the two nodes agree.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// The findings of one kind, for the dashboard.
    pub fn of_kind(&self, kind: &str) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|finding| finding.kind() == kind)
            .collect()
    }
}

/// Reconciles two scope snapshots: `local` is the owner, `remote` the mirror.
///
/// The `scope` names the reconciliation (it must be the scope both snapshots hold).
pub fn reconcile(local: &ScopeSnapshot, remote: &ScopeSnapshot, scope: &Scope) -> ReconcileReport {
    let local_digest = Digest::of(local);
    let remote_digest = Digest::of(remote);

    let mut findings = Vec::new();
    let mut stats = WalkStats::default();
    if local_digest.range != remote_digest.range {
        let local_tree = MerkleTree::build(&keys_of(local));
        let remote_tree = MerkleTree::build(&keys_of(remote));
        walk(
            local_tree.node(),
            remote_tree.node(),
            0,
            &mut findings,
            &mut stats,
        );
    }
    compare_aggregates(&local_digest, &remote_digest, &mut findings);

    ReconcileReport {
        scope: scope.clone(),
        findings,
        rounds: stats.rounds,
        digest_bytes: stats.bytes,
    }
}

/// Reads both sources and reconciles them: `local` is the owner, `remote` the mirror.
pub async fn reconcile_sources<L, R>(
    local: &L,
    remote: &R,
    scope: &Scope,
) -> ReconcileResult<ReconcileReport>
where
    L: DigestSource,
    R: DigestSource,
{
    let local_snapshot = local.snapshot(scope).await?;
    let remote_snapshot = remote.snapshot(scope).await?;
    Ok(reconcile(&local_snapshot, &remote_snapshot, scope))
}

fn keys_of(snapshot: &ScopeSnapshot) -> Vec<RecordKey> {
    snapshot
        .records
        .iter()
        .map(|facts| facts.key.clone())
        .collect()
}

#[derive(Default)]
struct WalkStats {
    rounds: usize,
    bytes: usize,
}

fn walk(local: &Node, remote: &Node, bit: u16, out: &mut Vec<Finding>, stats: &mut WalkStats) {
    stats.rounds += 1;
    stats.bytes += 2 * HASH_LEN;
    if local.digest() == remote.digest() {
        return;
    }
    match (local, remote) {
        (Node::Empty, Node::Empty) => {}
        (Node::Empty, _) => {
            for leaf in remote.leaves() {
                out.push(Finding::ExtraAtReceiver {
                    entity: leaf.key.entity.clone(),
                    name: leaf.key.name.clone(),
                    version: leaf.key.version,
                });
            }
        }
        (_, Node::Empty) => {
            for leaf in local.leaves() {
                out.push(Finding::MissingAtReceiver {
                    entity: leaf.key.entity.clone(),
                    name: leaf.key.name.clone(),
                    version: leaf.key.version,
                });
            }
        }
        (Node::Leaves { .. }, Node::Leaves { .. }) => compare_leaf_sets(local, remote, out),
        _ => {
            let (local_zero, local_one) = local.child_at(bit);
            let (remote_zero, remote_one) = remote.child_at(bit);
            walk(local_zero, remote_zero, bit + 1, out, stats);
            walk(local_one, remote_one, bit + 1, out, stats);
        }
    }
}

/// Compares two terminal leaf sets: the records of one subtree that share a full key path.
fn compare_leaf_sets(local: &Node, remote: &Node, out: &mut Vec<Finding>) {
    let local_leaves = leaf_versions(local);
    let remote_leaves = leaf_versions(remote);
    for ((entity, name), local_version) in &local_leaves {
        match remote_leaves.get(&(entity.clone(), name.clone())) {
            None => out.push(Finding::MissingAtReceiver {
                entity: entity.clone(),
                name: name.clone(),
                version: *local_version,
            }),
            Some(remote_version) if remote_version != local_version => {
                out.push(Finding::VersionMismatch {
                    entity: entity.clone(),
                    name: name.clone(),
                    local: *local_version,
                    remote: *remote_version,
                });
            }
            Some(_) => {}
        }
    }
    for ((entity, name), remote_version) in &remote_leaves {
        if !local_leaves.contains_key(&(entity.clone(), name.clone())) {
            out.push(Finding::ExtraAtReceiver {
                entity: entity.clone(),
                name: name.clone(),
                version: *remote_version,
            });
        }
    }
}

fn leaf_versions(node: &Node) -> BTreeMap<(Name, String), Version> {
    let mut out = BTreeMap::new();
    for leaf in node.leaves() {
        out.insert(
            (leaf.key.entity.clone(), leaf.key.name.clone()),
            leaf.key.version,
        );
    }
    out
}

fn compare_aggregates(local: &Digest, remote: &Digest, out: &mut Vec<Finding>) {
    let count_keys: Vec<&Name> = union_keys(&local.counts, &remote.counts);
    for entity in count_keys {
        let l = i128::from(local.count_of(entity));
        let r = i128::from(remote.count_of(entity));
        if l != r {
            out.push(Finding::TotalMismatch {
                measure: Measure::Count {
                    entity: entity.clone(),
                },
                local: l,
                remote: r,
            });
        }
    }

    let total_keys: Vec<&(Name, Name)> = union_keys(&local.totals, &remote.totals);
    for key in total_keys {
        let l = local.totals.get(key).copied().unwrap_or(0);
        let r = remote.totals.get(key).copied().unwrap_or(0);
        if l != r {
            out.push(Finding::TotalMismatch {
                measure: Measure::Total {
                    entity: key.0.clone(),
                    field: key.1.clone(),
                },
                local: l,
                remote: r,
            });
        }
    }

    let gl_keys: Vec<&String> = union_keys(&local.gl, &remote.gl);
    for account in gl_keys {
        let l = local.gl.get(account).copied().unwrap_or(0);
        let r = remote.gl.get(account).copied().unwrap_or(0);
        if l != r {
            out.push(Finding::TotalMismatch {
                measure: Measure::GlBalance {
                    account: account.clone(),
                },
                local: l,
                remote: r,
            });
        }
    }

    let stock_keys: Vec<&String> = union_keys(&local.stock, &remote.stock);
    for item in stock_keys {
        let l = local.stock.get(item).copied().unwrap_or(0);
        let r = remote.stock.get(item).copied().unwrap_or(0);
        if l != r {
            out.push(Finding::TotalMismatch {
                measure: Measure::StockQuantity { item: item.clone() },
                local: l,
                remote: r,
            });
        }
    }
}

/// The keys of both maps, each once, in order.
fn union_keys<'a, K: Ord, V>(local: &'a BTreeMap<K, V>, remote: &'a BTreeMap<K, V>) -> Vec<&'a K> {
    let mut keys: Vec<&K> = local.keys().chain(remote.keys()).collect();
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::digest::RecordFacts;
    use spot_storage_contract::FeedPosition;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn facts(entity: &str, record: &str, version: u64) -> RecordFacts {
        RecordFacts::of(RecordKey::new(name(entity), record, Version(version)))
    }

    fn snapshot(scope: &Scope, records: Vec<RecordFacts>) -> ScopeSnapshot {
        ScopeSnapshot::new(scope.clone(), FeedPosition::new(1, 1), records)
    }

    #[test]
    fn identical_snapshots_reconcile_cleanly() {
        let scope = Scope::new("company", "C1");
        let records = vec![facts("item", "A", 1), facts("item", "B", 2)];
        let report = reconcile(
            &snapshot(&scope, records.clone()),
            &snapshot(&scope, records),
            &scope,
        );
        assert!(report.is_clean(), "{:?}", report.findings);
        assert_eq!(report.rounds, 0, "a clean pair needs no bisection");
    }

    #[test]
    fn a_missing_record_is_found() {
        let scope = Scope::new("company", "C1");
        let local = vec![facts("item", "A", 1), facts("item", "B", 2)];
        let remote = vec![facts("item", "A", 1)];
        let report = reconcile(&snapshot(&scope, local), &snapshot(&scope, remote), &scope);
        assert_eq!(report.of_kind("missing").len(), 1, "{:?}", report.findings);
        // The count digest differs too: one fewer item at the mirror.
        assert_eq!(report.of_kind("total").len(), 1, "{:?}", report.findings);
    }

    #[test]
    fn a_version_mismatch_is_found() {
        let scope = Scope::new("company", "C1");
        let local = vec![facts("item", "A", 3)];
        let remote = vec![facts("item", "A", 1)];
        let report = reconcile(&snapshot(&scope, local), &snapshot(&scope, remote), &scope);
        assert_eq!(
            report.findings,
            vec![Finding::VersionMismatch {
                entity: name("item"),
                name: "A".into(),
                local: Version(3),
                remote: Version(1),
            }]
        );
    }

    #[test]
    fn an_altered_total_is_found_without_a_version_change() {
        let scope = Scope::new("company", "C1");
        let mut local = facts("invoice", "INV-1", 1);
        local.totals = vec![(name("total"), 1_000)];
        let mut remote = facts("invoice", "INV-1", 1);
        remote.totals = vec![(name("total"), 900)];
        let report = reconcile(
            &snapshot(&scope, vec![local]),
            &snapshot(&scope, vec![remote]),
            &scope,
        );
        assert_eq!(report.of_kind("total").len(), 1, "{:?}", report.findings);
        assert!(report.of_kind("version").is_empty());
    }
}
