//! The relay's check of the [`HubStream`](crate::HubStream) contract, made before any bundle is
//! built.
//!
//! The trait promises two things the relay depends on and cannot see in its types:
//!
//! * `read_after` returns **whole source transactions** (ADR-064 section 1: a bundle is applied by
//!   the node in one transaction, so a transaction that arrives in two reads is bundled twice and
//!   applied half-and-half);
//! * an entry's **lane is a function of its entity** (ADR-065 section 2: the mandatory lane is
//!   served before bulk, so a lane that changes reorders the versions of one record).
//!
//! A persistent adapter that breaks either does not fail loudly on its own. So every read is
//! checked against what the relay remembers (`last_txn`, `lane_of_entity`), and a read that breaks
//! the contract is refused whole: nothing is queued and nothing the relay remembers moves, so the
//! next tick asks again and a transient adapter fault recovers while a permanent one stays
//! visible.

use crate::error::HubError;
use crate::model::{RelayConfig, StreamEntry};
use spot_sync_obligations::Lane;
use std::collections::{BTreeMap, BTreeSet};

/// What the relay remembers of the stream it reads: the source transaction of the last entry it
/// processed, and the lane each entity came in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StreamContract {
    last_txn: Option<String>,
    lane_of_entity: BTreeMap<String, Lane>,
}

/// The state a read that passed the check leaves behind. It is applied by
/// [`StreamContract::commit`] only, so a refused read changes nothing at all.
#[derive(Debug)]
pub(crate) struct Passed {
    /// The transaction of the read's last entry, when the read is the forward one.
    last_txn: Option<String>,
    /// The entities the read named for the first time.
    added: Vec<(String, Lane)>,
}

impl StreamContract {
    /// The contract state a restored [`RelayState`](crate::RelayState) carries.
    pub(crate) fn restored(
        last_txn: Option<String>,
        lane_of_entity: BTreeMap<String, Lane>,
    ) -> Self {
        Self {
            last_txn,
            lane_of_entity,
        }
    }

    /// The transaction of the last entry processed, for the snapshot.
    pub(crate) fn last_txn(&self) -> Option<&str> {
        self.last_txn.as_deref()
    }

    /// The lane of every entity seen so far, for the snapshot.
    pub(crate) fn lanes(&self) -> &BTreeMap<String, Lane> {
        &self.lane_of_entity
    }

    /// Check one read against the contract and what the relay remembers.
    ///
    /// `floor` is the position every entry must exceed (the cursor for the forward read, the
    /// position the read started after for a catch-up read); positions must strictly ascend from
    /// it. `forward` says whether this read continues the stream the relay is processing: only
    /// then can its first transaction be the continuation of the last one processed. A catch-up
    /// read re-reads positions the relay already passed, so it is checked as one read on its own.
    ///
    /// # Errors
    /// `StreamDisorder` for a position that does not ascend, `StreamSplitTxn` for a transaction
    /// that arrives in more than one piece, `LaneNotEntity` for an entity that changed lane and
    /// `StreamEntityLimit` when the lane map is at its bound. Nothing has been changed then.
    pub(crate) fn check(
        &self,
        entries: &[StreamEntry],
        cfg: &RelayConfig,
        floor: Option<u64>,
        forward: bool,
    ) -> Result<Passed, HubError> {
        let mut order = floor;
        // Positions of each transaction in this read, so a transaction that comes back can be
        // reported with every position it took.
        let mut positions: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        // The transaction whose run is open, and the ones whose run already ended.
        let mut open: Option<&str> = None;
        let mut closed: BTreeSet<&str> = BTreeSet::new();
        // The lane of each entity this read named for the first time.
        let mut fresh: BTreeMap<&str, Lane> = BTreeMap::new();

        for e in entries {
            if let Some(last) = order
                && e.position <= last
            {
                return Err(HubError::StreamDisorder {
                    position: e.position,
                    after: last,
                });
            }
            order = Some(e.position);

            // One source transaction is one contiguous run of the read.
            let txn = e.txn.as_str();
            if open != Some(txn) {
                if let Some(previous) = open {
                    closed.insert(previous);
                }
                if closed.contains(txn) {
                    let mut at = positions.get(txn).cloned().unwrap_or_default();
                    at.push(e.position);
                    return Err(HubError::StreamSplitTxn {
                        txn: txn.to_owned(),
                        positions: at,
                    });
                }
                open = Some(txn);
            }
            positions.entry(txn).or_default().push(e.position);

            // The lane is a function of the entity, for the entries this relay routes. An entry
            // of another tenant or source may legitimately reuse an entity name with another
            // lane: it is never routed, so it cannot reorder anything here.
            if e.tenant != cfg.tenant || e.source != cfg.source {
                continue;
            }
            let entity = e.key.entity.as_str();
            if let Some(&lane) = self
                .lane_of_entity
                .get(entity)
                .or_else(|| fresh.get(entity))
            {
                if lane != e.lane {
                    return Err(HubError::LaneNotEntity {
                        entity: e.key.entity.clone(),
                        was: lane,
                        now: e.lane,
                    });
                }
                continue;
            }
            if self.lane_of_entity.len() + fresh.len() >= cfg.max_entities {
                return Err(HubError::StreamEntityLimit {
                    entity: e.key.entity.clone(),
                    position: e.position,
                    bound: cfg.max_entities,
                });
            }
            fresh.insert(entity, e.lane);
        }

        // A read that continues the last one must not continue its transaction: that transaction
        // was read in pieces.
        if forward
            && let (Some(first), Some(last)) = (entries.first(), self.last_txn.as_deref())
            && first.txn == last
        {
            return Err(HubError::StreamSplitTxn {
                txn: last.to_owned(),
                positions: positions.get(last).cloned().unwrap_or_default(),
            });
        }

        Ok(Passed {
            last_txn: if forward {
                entries.last().map(|e| e.txn.clone())
            } else {
                None
            },
            added: fresh
                .into_iter()
                .map(|(entity, lane)| (entity.to_owned(), lane))
                .collect(),
        })
    }

    /// Apply what a read that passed leaves behind.
    pub(crate) fn commit(&mut self, passed: Passed) {
        for (entity, lane) in passed.added {
            self.lane_of_entity.insert(entity, lane);
        }
        if let Some(txn) = passed.last_txn {
            self.last_txn = Some(txn);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::EntryRole;
    use spot_sync_closure::RecordKey;

    fn entry(position: u64, txn: &str, entity: &str, lane: Lane) -> StreamEntry {
        StreamEntry {
            position,
            tenant: "t1".to_owned(),
            source: "hub".to_owned(),
            origin: None,
            txn: txn.to_owned(),
            role: EntryRole::Root,
            key: RecordKey::new(entity, format!("R{position}")),
            version: 1,
            topic: entity.to_owned(),
            major: 1,
            lane,
        }
    }

    fn cfg() -> RelayConfig {
        RelayConfig::new("t1", "hub")
    }

    #[test]
    fn one_transaction_is_one_contiguous_run() {
        let contract = StreamContract::default();
        let read = vec![
            entry(1, "a", "item", Lane::Bulk),
            entry(2, "b", "item", Lane::Bulk),
            entry(3, "a", "item", Lane::Bulk),
        ];
        let err = contract
            .check(&read, &cfg(), Some(0), true)
            .expect_err("an interleaved transaction must be refused");
        assert_eq!(
            err,
            HubError::StreamSplitTxn {
                txn: "a".to_owned(),
                positions: vec![1, 3],
            }
        );
        assert_eq!(err.code(), "sync_hub.stream_split_txn");
    }

    #[test]
    fn a_transaction_the_next_read_continues_is_a_split() {
        let mut contract = StreamContract::default();
        let first = vec![
            entry(1, "a", "item", Lane::Bulk),
            entry(2, "a", "price", Lane::Bulk),
        ];
        let passed = contract.check(&first, &cfg(), Some(0), true).unwrap();
        assert_eq!(passed.last_txn.as_deref(), Some("a"));
        contract.commit(passed);
        let second = vec![entry(3, "a", "price", Lane::Bulk)];
        let err = contract
            .check(&second, &cfg(), Some(2), true)
            .expect_err("a continued transaction must be refused");
        assert_eq!(
            err,
            HubError::StreamSplitTxn {
                txn: "a".to_owned(),
                positions: vec![3],
            }
        );
        // A catch-up read re-reads positions already passed: it starts a whole transaction.
        assert!(contract.check(&second, &cfg(), Some(2), false).is_ok());
    }

    #[test]
    fn a_lane_change_carries_the_entity_and_both_lanes() {
        let mut contract = StreamContract::default();
        let first = vec![entry(1, "a", "price", Lane::Mandatory)];
        let passed = contract.check(&first, &cfg(), Some(0), true).unwrap();
        contract.commit(passed);
        let second = vec![entry(2, "a", "price", Lane::Bulk)];
        let err = contract
            .check(&second, &cfg(), Some(1), true)
            .expect_err("a lane change must be refused");
        assert_eq!(
            err,
            HubError::LaneNotEntity {
                entity: "price".to_owned(),
                was: Lane::Mandatory,
                now: Lane::Bulk,
            }
        );
        assert_eq!(err.code(), "sync_hub.lane_not_entity");
        assert_eq!(
            err.to_string(),
            "The entity price came in the bulk lane, after the mandatory lane; a record's lane is \
             a function of its entity."
        );
    }

    #[test]
    fn the_entity_bound_refuses_a_new_entity_only() {
        let mut c = cfg();
        c.max_entities = 2;
        let mut contract = StreamContract::default();
        let read = vec![
            entry(1, "a", "item", Lane::Bulk),
            entry(2, "b", "price", Lane::Bulk),
        ];
        let passed = contract.check(&read, &c, Some(0), true).unwrap();
        contract.commit(passed);
        assert_eq!(contract.lanes().len(), 2);
        // A known entity is fine at the bound; a new one is refused.
        assert!(
            contract
                .check(&[entry(3, "c", "item", Lane::Bulk)], &c, Some(2), false)
                .is_ok()
        );
        let err = contract
            .check(&[entry(4, "d", "tax", Lane::Mandatory)], &c, Some(3), false)
            .expect_err("a new entity over the bound must be refused");
        assert_eq!(
            err,
            HubError::StreamEntityLimit {
                entity: "tax".to_owned(),
                position: 4,
                bound: 2,
            }
        );
        assert_eq!(err.code(), "sync_hub.stream_entity_limit");
    }

    #[test]
    fn a_refused_read_changes_nothing_it_would_have_remembered() {
        let mut contract = StreamContract::default();
        let passed = contract
            .check(&[entry(1, "a", "item", Lane::Bulk)], &cfg(), Some(0), true)
            .unwrap();
        contract.commit(passed);
        let read = vec![
            entry(2, "b", "price", Lane::Bulk),
            entry(3, "c", "item", Lane::Mandatory),
        ];
        assert!(
            contract.check(&read, &cfg(), Some(1), true).is_err(),
            "the lane change is refused"
        );
        // `price` was named before the violation and is not remembered: the read is refused whole.
        assert_eq!(contract.lanes().len(), 1);
        assert_eq!(contract.last_txn(), Some("a"));
        assert_eq!(
            contract.lanes().get("item"),
            Some(&Lane::Bulk),
            "the lane of item does not flip on a refused read"
        );
    }

    #[test]
    fn positions_must_ascend_past_the_floor() {
        let contract = StreamContract::default();
        let read = vec![
            entry(2, "a", "item", Lane::Bulk),
            entry(2, "b", "item", Lane::Bulk),
        ];
        assert_eq!(
            contract.check(&read, &cfg(), Some(0), true).unwrap_err(),
            HubError::StreamDisorder {
                position: 2,
                after: 2
            }
        );
        let read = vec![entry(1, "a", "item", Lane::Bulk)];
        assert_eq!(
            contract.check(&read, &cfg(), Some(1), true).unwrap_err(),
            HubError::StreamDisorder {
                position: 1,
                after: 1
            }
        );
    }

    #[test]
    fn an_entry_of_another_tenant_does_not_pin_a_lane() {
        let contract = StreamContract::default();
        let mut foreign = entry(1, "a", "item", Lane::Mandatory);
        foreign.tenant = "t2".to_owned();
        let read = vec![foreign, entry(2, "b", "item", Lane::Bulk)];
        let passed = contract.check(&read, &cfg(), Some(0), true).unwrap();
        assert_eq!(passed.added, vec![("item".to_owned(), Lane::Bulk)]);
    }
}
