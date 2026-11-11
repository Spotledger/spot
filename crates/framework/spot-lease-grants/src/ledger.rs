//! What the hub has told each node, and what each node has told the hub (the hub half of
//! replay detection for offline identity).
//!
//! A node that runs offline holds a signed snapshot, a lease and a hub nonce on its own disk. An
//! attacker with write access to that disk can put an older copy back. Nothing on the node can
//! see it, but the hub can, if it remembers per node:
//!
//! * the **nonce** it last put in a signed reply (strictly increasing, never reused);
//! * the highest snapshot **epoch** and the **hash** at that epoch it last acknowledged;
//! * the newest hub time it signed;
//! * the highest nonce and hub clock the node itself ever reported;
//! * whether the node is **flagged** for re-enrolment.
//!
//! That is one [`NodeLedger`] per node, kept in a [`LedgerStore`]. The store is a trait with a
//! compare-and-set write, so two hub processes cannot both issue the same nonce; the in-memory
//! [`MemoryLedgerStore`] is the reference and [`contract`] is the suite any durable store must
//! pass. No database implementation lives here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_offline_identity::{HubMs, NodeId, SnapshotHash};

/// The hub's memory of one node. `Default` is a node the hub has never answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeLedger {
    /// The nonce of the newest reply the hub signed for the node (zero before the first).
    pub issued_nonce: u64,
    /// The hub time of the newest reply the hub signed for the node.
    pub issued_hub_ms: HubMs,
    /// The highest snapshot epoch the hub acknowledged to the node (zero before the first).
    pub acked_epoch: u64,
    /// The hash of the snapshot at [`NodeLedger::acked_epoch`].
    pub acked_hash: Option<SnapshotHash>,
    /// The highest hub nonce the node itself has reported.
    pub reported_nonce: u64,
    /// The highest hub clock the node itself has reported.
    pub reported_attested: HubMs,
    /// Set when the node reported less than the hub gave it. Only a re-enrolment clears it.
    pub requires_reenrolment: bool,
    /// How many times the node has been enrolled afresh (zero for the first enrolment).
    pub enrolment: u64,
}

impl NodeLedger {
    /// Whether `next` is a legal successor of `self`: nothing the hub issued or the node
    /// reported goes down, and a flag clears only with a new enrolment. A store applies this on
    /// every write, so a bug in the caller cannot lower a nonce.
    #[must_use]
    pub fn may_become(&self, next: &Self) -> bool {
        if next.enrolment < self.enrolment
            || next.issued_nonce < self.issued_nonce
            || next.issued_hub_ms < self.issued_hub_ms
        {
            return false;
        }
        if next.enrolment > self.enrolment {
            return !next.requires_reenrolment;
        }
        next.acked_epoch >= self.acked_epoch
            && next.reported_nonce >= self.reported_nonce
            && next.reported_attested >= self.reported_attested
            && (next.requires_reenrolment || !self.requires_reenrolment)
    }
}

/// Why a [`LedgerStore`] did not take a write or a read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    /// The stored ledger is not the one the caller read: someone else wrote in between.
    Conflict,
    /// The write would lower something that only goes up (see [`NodeLedger::may_become`]).
    Rule,
    /// The store could not be reached. The hub fails closed.
    Unavailable,
}

/// The per-node [`NodeLedger`]s.
pub trait LedgerStore {
    /// The ledger of `node`; the default ledger for a node never seen.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Unavailable`] when the store cannot answer.
    fn get(&self, node: &NodeId) -> Result<NodeLedger, LedgerError>;

    /// Replaces the ledger of `node` with `next` when it is still `expected`.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Conflict`] when the stored ledger differs from `expected` (nothing is
    /// written); [`LedgerError::Rule`] when `expected` may not become `next`;
    /// [`LedgerError::Unavailable`] when the write is not durable.
    fn replace(
        &self,
        node: &NodeId,
        expected: &NodeLedger,
        next: NodeLedger,
    ) -> Result<(), LedgerError>;
}

#[derive(Debug, Default)]
struct Inner {
    nodes: BTreeMap<NodeId, NodeLedger>,
    fail: bool,
}

/// An in-memory [`LedgerStore`]. Clones share the same ledgers (it is a handle), so the guard
/// and the standing view see one truth.
#[derive(Clone, Debug, Default)]
pub struct MemoryLedgerStore {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryLedgerStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every read and write fail (tests of the fail-closed paths).
    pub fn set_failing(&self, fail: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.fail = fail;
        }
    }
}

impl LedgerStore for MemoryLedgerStore {
    fn get(&self, node: &NodeId) -> Result<NodeLedger, LedgerError> {
        let inner = self.inner.lock().map_err(|_| LedgerError::Unavailable)?;
        if inner.fail {
            return Err(LedgerError::Unavailable);
        }
        Ok(inner.nodes.get(node).copied().unwrap_or_default())
    }

    fn replace(
        &self,
        node: &NodeId,
        expected: &NodeLedger,
        next: NodeLedger,
    ) -> Result<(), LedgerError> {
        let mut inner = self.inner.lock().map_err(|_| LedgerError::Unavailable)?;
        if inner.fail {
            return Err(LedgerError::Unavailable);
        }
        let current = inner.nodes.get(node).copied().unwrap_or_default();
        if &current != expected {
            return Err(LedgerError::Conflict);
        }
        if !current.may_become(&next) {
            return Err(LedgerError::Rule);
        }
        inner.nodes.insert(node.clone(), next);
        Ok(())
    }
}

/// The contract any [`LedgerStore`] must pass.
pub mod contract {
    use super::{LedgerError, LedgerStore, NodeId, NodeLedger};

    fn id(s: &str) -> NodeId {
        NodeId::new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Runs the contract against fresh stores from `make`.
    ///
    /// # Panics
    ///
    /// When the store breaks a rule.
    pub fn run<S: LedgerStore>(make: impl Fn() -> S) {
        let (a, b) = (id("node-a"), id("node-b"));

        // A node never seen has the default ledger.
        let store = make();
        assert_eq!(store.get(&a), Ok(NodeLedger::default()));

        // A write applies when the caller read the current ledger, and is per node.
        let first = NodeLedger {
            issued_nonce: 1,
            issued_hub_ms: 10,
            acked_epoch: 1,
            acked_hash: Some([7; 32]),
            ..NodeLedger::default()
        };
        assert_eq!(store.replace(&a, &NodeLedger::default(), first), Ok(()));
        assert_eq!(store.get(&a), Ok(first));
        assert_eq!(store.get(&b), Ok(NodeLedger::default()));

        // A stale `expected` is a conflict and writes nothing: two writers cannot both issue
        // nonce 2.
        let second = NodeLedger {
            issued_nonce: 2,
            issued_hub_ms: 20,
            ..first
        };
        assert_eq!(store.replace(&a, &first, second), Ok(()));
        let rival = NodeLedger {
            issued_nonce: 2,
            issued_hub_ms: 21,
            ..first
        };
        assert_eq!(store.replace(&a, &first, rival), Err(LedgerError::Conflict));
        assert_eq!(store.get(&a), Ok(second));

        // Nothing that only goes up may go down.
        let third = NodeLedger {
            reported_nonce: 5,
            reported_attested: 15,
            ..second
        };
        assert_eq!(store.replace(&a, &second, third), Ok(()));
        for lowered in [
            NodeLedger {
                issued_nonce: 1,
                ..third
            },
            NodeLedger {
                issued_hub_ms: 19,
                ..third
            },
            NodeLedger {
                acked_epoch: 0,
                ..third
            },
            NodeLedger {
                reported_nonce: 4,
                ..third
            },
            NodeLedger {
                reported_attested: 14,
                ..third
            },
        ] {
            assert_eq!(
                store.replace(&a, &third, lowered),
                Err(LedgerError::Rule),
                "{lowered:?} must be refused"
            );
            assert_eq!(store.get(&a), Ok(third));
        }

        // The flag sets, and clears only with a new enrolment.
        let flagged = NodeLedger {
            requires_reenrolment: true,
            ..third
        };
        assert_eq!(store.replace(&a, &third, flagged), Ok(()));
        let unflagged_same = NodeLedger {
            requires_reenrolment: false,
            ..flagged
        };
        assert_eq!(
            store.replace(&a, &flagged, unflagged_same),
            Err(LedgerError::Rule)
        );
        let reenrolled = NodeLedger {
            requires_reenrolment: false,
            enrolment: 1,
            acked_epoch: 0,
            acked_hash: None,
            ..flagged
        };
        assert_eq!(store.replace(&a, &flagged, reenrolled), Ok(()));
        // The nonce high-water survives a re-enrolment: it cannot be lowered by one.
        let reset = NodeLedger {
            issued_nonce: 0,
            ..reenrolled
        };
        assert_eq!(
            store.replace(&a, &reenrolled, reset),
            Err(LedgerError::Rule)
        );
    }
}
