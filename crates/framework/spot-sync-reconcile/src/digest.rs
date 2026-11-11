//! The scope digest: counts, totals, grouped sums and the Merkle-style range digest.
//!
//! A [`ScopeSnapshot`] is one node's consistent read of a scope (see [`crate::DigestSource`]).
//! [`Digest::of`] folds it into the small summary that is actually compared between hub and
//! edge: per-DocType counts, sums of the declared integer total fields, the GL balance by
//! account, the stock quantity by item, and a single 32-byte **range root** over the
//! `(name, version)` pairs.
//!
//! The range root is a Merkle-style digest over a binary trie keyed by
//! `SHA-256(entity, name)`: every node's digest is the hash of its two children, so two nodes
//! whose subtrees agree have equal digests. Comparing two roots tells nothing about *where*
//! they differ; [`crate::reconcile`] descends the two tries in lockstep and only expands a
//! subtree whose digest differs, so a mismatch is narrowed by bisection instead of a full diff.
//! The structure is derived from the key, so the owner and the mirror build the *same* trie for
//! the same keys and can each answer for the same prefix.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};
use spot_storage_contract::{FeedPosition, Name, Version};

use crate::scope::Scope;

/// Length of every hash this crate uses, in bytes (SHA-256).
pub(crate) const HASH_LEN: usize = 32;

/// A SHA-256 digest.
pub(crate) type Hash = [u8; HASH_LEN];

/// The digest of an empty subtree.
pub(crate) const EMPTY_HASH: Hash = [0u8; HASH_LEN];

/// A record's identity and version, the leaf of the range digest.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    /// The record's entity.
    pub entity: Name,
    /// The record's human-readable name (the plan's `name_field`).
    pub name: String,
    /// The record's stored version (ADR-053): the sync unit.
    pub version: Version,
}

impl RecordKey {
    /// A record key.
    pub fn new(entity: Name, name: impl Into<String>, version: Version) -> Self {
        Self {
            entity,
            name: name.into(),
            version,
        }
    }
}

/// One record's contribution to the digest.
///
/// The source has already applied the [`DigestPlan`](crate::DigestPlan): `totals` holds the
/// declared total-field values, and `gl`/`stock` hold the `(key, value)` pairs of the declared
/// grouped sums. Aggregation into per-scope maps is [`Digest::of`]'s job, so it is pure and
/// testable without storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordFacts {
    /// The record's key.
    pub key: RecordKey,
    /// `(field, value)` for each declared integer total field present on this record.
    pub totals: Vec<(Name, i128)>,
    /// `(account, value)` for the GL balance, when the plan declares it.
    pub gl: Vec<(String, i128)>,
    /// `(item, value)` for the stock quantity, when the plan declares it.
    pub stock: Vec<(String, i128)>,
}

impl RecordFacts {
    /// Facts for `key` with no totals and no grouped sums.
    pub fn of(key: RecordKey) -> Self {
        Self {
            key,
            totals: Vec::new(),
            gl: Vec::new(),
            stock: Vec::new(),
        }
    }
}

/// One node's consistent read of a scope, at the safe bookmark it was taken at (ADR-053).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeSnapshot {
    /// The scope read.
    pub scope: Scope,
    /// The safe bookmark: nothing at or before this position can appear after it, so a
    /// snapshot taken here is a consistent point and in-flight writes never cause false drift.
    pub bookmark: FeedPosition,
    /// The records read, in no particular order; the digest sorts them.
    pub records: Vec<RecordFacts>,
}

impl ScopeSnapshot {
    /// A snapshot of `scope` at `bookmark` over `records`.
    pub fn new(scope: Scope, bookmark: FeedPosition, records: Vec<RecordFacts>) -> Self {
        Self {
            scope,
            bookmark,
            records,
        }
    }
}

/// The small summary compared between two nodes for one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    /// The scope summarised.
    pub scope: Scope,
    /// The bookmark the snapshot was taken at.
    pub bookmark: FeedPosition,
    /// How many records the scope holds.
    pub records: u64,
    /// Record count per entity.
    pub counts: BTreeMap<Name, u64>,
    /// Summed integer totals, per `(entity, field)`.
    pub totals: BTreeMap<(Name, Name), i128>,
    /// GL balance per account.
    pub gl: BTreeMap<String, i128>,
    /// Stock quantity per item.
    pub stock: BTreeMap<String, i128>,
    /// The Merkle-style range root over the `(name, version)` pairs.
    pub range: Hash,
}

impl Digest {
    /// Summarises a snapshot.
    pub fn of(snapshot: &ScopeSnapshot) -> Self {
        let mut counts: BTreeMap<Name, u64> = BTreeMap::new();
        let mut totals: BTreeMap<(Name, Name), i128> = BTreeMap::new();
        let mut gl: BTreeMap<String, i128> = BTreeMap::new();
        let mut stock: BTreeMap<String, i128> = BTreeMap::new();
        for facts in &snapshot.records {
            *counts.entry(facts.key.entity.clone()).or_insert(0) += 1;
            for (field, value) in &facts.totals {
                *totals
                    .entry((facts.key.entity.clone(), field.clone()))
                    .or_insert(0) += value;
            }
            for (account, value) in &facts.gl {
                *gl.entry(account.clone()).or_insert(0) += value;
            }
            for (item, value) in &facts.stock {
                *stock.entry(item.clone()).or_insert(0) += value;
            }
        }
        let keys: Vec<RecordKey> = snapshot.records.iter().map(|f| f.key.clone()).collect();
        let range = MerkleTree::build(&keys).root();
        Self {
            scope: snapshot.scope.clone(),
            bookmark: snapshot.bookmark,
            records: snapshot.records.len() as u64,
            counts,
            totals,
            gl,
            stock,
            range,
        }
    }

    /// The record count of one entity.
    pub fn count_of(&self, entity: &Name) -> u64 {
        self.counts.get(entity).copied().unwrap_or(0)
    }

    /// The summed total of one `(entity, field)`.
    pub fn total_of(&self, entity: &Name, field: &Name) -> i128 {
        self.totals
            .get(&(entity.clone(), field.clone()))
            .copied()
            .unwrap_or(0)
    }
}

/// The path of a record's key in the range trie: `SHA-256(entity, name)`.
///
/// The version is deliberately **not** in the path: two nodes that hold the same record at
/// different versions meet at the same leaf and report a version mismatch, rather than a
/// missing and an extra record.
pub(crate) fn key_path(key: &RecordKey) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"spot-sync-reconcile.key.v1\0");
    hasher.update(key.entity.as_str().as_bytes());
    hasher.update([0u8]);
    hasher.update(key.name.as_bytes());
    hasher.finalize().into()
}

/// The hash of one leaf: `SHA-256(entity, name, version)`.
pub(crate) fn leaf_hash(key: &RecordKey) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"spot-sync-reconcile.leaf.v1\0");
    hasher.update(key.entity.as_str().as_bytes());
    hasher.update([0u8]);
    hasher.update(key.name.as_bytes());
    hasher.update([0u8]);
    hasher.update(key.version.0.to_be_bytes());
    hasher.finalize().into()
}

/// True when bit `bit` (0 = most significant) of `hash` is set.
pub(crate) fn bit_set(hash: &Hash, bit: u16) -> bool {
    let byte = usize::from(bit) / 8;
    let shift = 7 - (usize::from(bit) % 8);
    (hash[byte] >> shift) & 1 == 1
}

/// A leaf in the range trie.
#[derive(Clone, Debug)]
pub(crate) struct Leaf {
    /// The record.
    pub key: RecordKey,
    /// Its path in the trie.
    pub path: Hash,
    /// Its leaf hash.
    pub hash: Hash,
}

/// One node of the range trie.
pub(crate) enum Node {
    /// No leaves.
    Empty,
    /// A terminal: one leaf, or several that share a full path (a hash collision).
    Leaves {
        /// The subtree digest.
        digest: Hash,
        /// The leaves, sorted by leaf hash.
        leaves: Vec<Leaf>,
    },
    /// An internal node that splits its leaves at bit `bit`.
    Split {
        /// The subtree digest.
        digest: Hash,
        /// The bit this node splits on.
        bit: u16,
        /// A representative path, used to place a compressed node when asked for a shallower
        /// bit than it splits on.
        rep: Hash,
        /// Leaves whose bit is 0.
        left: Box<Node>,
        /// Leaves whose bit is 1.
        right: Box<Node>,
    },
}

impl Node {
    /// The subtree digest; the empty subtree's digest for [`Node::Empty`].
    pub(crate) fn digest(&self) -> Hash {
        match self {
            Node::Empty => EMPTY_HASH,
            Node::Leaves { digest, .. } | Node::Split { digest, .. } => *digest,
        }
    }

    /// Every leaf under this node, in no particular order.
    pub(crate) fn leaves(&self) -> Vec<&Leaf> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a Leaf>) {
        match self {
            Node::Empty => {}
            Node::Leaves { leaves, .. } => out.extend(leaves.iter()),
            Node::Split { left, right, .. } => {
                left.collect(out);
                right.collect(out);
            }
        }
    }

    /// The two children of this node at bit `bit`, as `(bit-0 side, bit-1 side)`.
    ///
    /// A node that does not split at `bit` (it is a leaf, or it splits deeper because all its
    /// leaves agree on this bit) is returned whole on the side its leaves take.
    pub(crate) fn child_at(&self, bit: u16) -> (&Node, &Node) {
        static EMPTY: Node = Node::Empty;
        match self {
            Node::Empty => (&EMPTY, &EMPTY),
            Node::Leaves { leaves, .. } => {
                if bit_set(&leaves[0].path, bit) {
                    (&EMPTY, self)
                } else {
                    (self, &EMPTY)
                }
            }
            Node::Split {
                bit: split, rep, ..
            } => {
                if *split == bit {
                    match self {
                        Node::Split { left, right, .. } => (left, right),
                        _ => (&EMPTY, &EMPTY),
                    }
                } else if bit_set(rep, bit) {
                    (&EMPTY, self)
                } else {
                    (self, &EMPTY)
                }
            }
        }
    }
}

/// A binary Merkle trie over record keys, keyed by [`key_path`].
pub(crate) struct MerkleTree {
    root: Node,
}

impl MerkleTree {
    /// Builds the trie over `keys` (any order).
    pub(crate) fn build(keys: &[RecordKey]) -> Self {
        let leaves: Vec<Leaf> = keys
            .iter()
            .map(|key| Leaf {
                key: key.clone(),
                path: key_path(key),
                hash: leaf_hash(key),
            })
            .collect();
        Self {
            root: build_node(leaves, 0),
        }
    }

    /// The range root: the digest of the whole trie.
    pub(crate) fn root(&self) -> Hash {
        self.root.digest()
    }

    /// The root node, for [`crate::reconcile`] to descend.
    pub(crate) fn node(&self) -> &Node {
        &self.root
    }
}

fn build_node(mut leaves: Vec<Leaf>, bit: u16) -> Node {
    if leaves.is_empty() {
        return Node::Empty;
    }
    if leaves.len() == 1 || usize::from(bit) >= HASH_LEN * 8 {
        leaves.sort_by_key(|leaf| leaf.hash);
        let digest = leaves_digest(&leaves);
        return Node::Leaves { digest, leaves };
    }
    let (left, right): (Vec<Leaf>, Vec<Leaf>) = leaves
        .into_iter()
        .partition(|leaf| !bit_set(&leaf.path, bit));
    if left.is_empty() || right.is_empty() {
        // Every leaf agrees on this bit: compress the unary path away and try the next bit.
        let all = if left.is_empty() { right } else { left };
        return build_node(all, bit + 1);
    }
    let left_node = build_node(left, bit + 1);
    let right_node = build_node(right, bit + 1);
    let digest = split_digest(&left_node.digest(), &right_node.digest());
    let rep = first_path(&left_node, &right_node);
    Node::Split {
        digest,
        bit,
        rep,
        left: Box::new(left_node),
        right: Box::new(right_node),
    }
}

fn first_path(left: &Node, right: &Node) -> Hash {
    // A representative path of the subtree: any leaf under `left` or `right` shares the bits
    // above this split, which is all `child_at` needs when it is asked for a shallower bit.
    let mut leaves = left.leaves();
    if leaves.is_empty() {
        leaves = right.leaves();
    }
    leaves.first().map_or(EMPTY_HASH, |leaf| leaf.path)
}

fn leaves_digest(leaves: &[Leaf]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"spot-sync-reconcile.leaves.v1\0");
    hasher.update((leaves.len() as u64).to_be_bytes());
    for leaf in leaves {
        hasher.update(leaf.hash);
    }
    hasher.finalize().into()
}

fn split_digest(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"spot-sync-reconcile.split.v1\0");
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn key(entity: &str, name: &str, version: u64) -> RecordKey {
        RecordKey::new(Name::new(entity).unwrap(), name, Version(version))
    }

    #[test]
    fn a_trie_root_changes_with_any_leaf() {
        let base = vec![
            key("item", "A", 1),
            key("item", "B", 1),
            key("item", "C", 1),
        ];
        let root = MerkleTree::build(&base).root();

        let mut changed_version = base.clone();
        changed_version[1].version = Version(2);
        assert_ne!(MerkleTree::build(&changed_version).root(), root);

        let mut extra = base.clone();
        extra.push(key("item", "D", 1));
        assert_ne!(MerkleTree::build(&extra).root(), root);

        let mut missing = base.clone();
        missing.pop();
        assert_ne!(MerkleTree::build(&missing).root(), root);
    }

    #[test]
    fn the_root_is_independent_of_input_order() {
        let a = vec![
            key("item", "A", 1),
            key("item", "B", 2),
            key("party", "P", 1),
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(MerkleTree::build(&a).root(), MerkleTree::build(&b).root());
    }

    #[test]
    fn an_empty_trie_has_the_empty_root() {
        assert_eq!(MerkleTree::build(&[]).root(), EMPTY_HASH);
    }

    #[test]
    fn a_leaf_node_sits_on_one_side_of_a_shallower_bit() {
        let tree = MerkleTree::build(&[key("item", "A", 1)]);
        let node = tree.node();
        // A single leaf never splits: at every bit it is whole on one side and empty on the
        // other.
        for bit in [0u16, 1, 7, 63, 255] {
            let (zero, one) = node.child_at(bit);
            assert!(
                matches!(zero, Node::Empty) ^ matches!(one, Node::Empty),
                "bit {bit}"
            );
        }
    }

    #[test]
    fn a_split_node_returns_its_two_children_at_its_own_bit() {
        // Many keys force at least one real split; every split's children are both present.
        let keys: Vec<RecordKey> = (0..64)
            .map(|i| key("item", &format!("item_{i:03}"), 1))
            .collect();
        let tree = MerkleTree::build(&keys);
        fn walk(node: &Node, count: &mut usize) {
            if let Node::Split {
                bit, left, right, ..
            } = node
            {
                let (zero, one) = node.child_at(*bit);
                assert!(!matches!(zero, Node::Empty));
                assert!(!matches!(one, Node::Empty));
                *count += 1;
                walk(left, count);
                walk(right, count);
            }
        }
        let mut splits = 0;
        walk(tree.node(), &mut splits);
        assert!(splits > 0);
    }
}
