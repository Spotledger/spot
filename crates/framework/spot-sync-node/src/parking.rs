//! The in-memory parking store: a reference double for tests and the simulator.

use std::collections::{BTreeMap, VecDeque};

use spot_sync_closure::RecordKey;

use crate::model::ParkedBundle;
use crate::traits::ParkingStore;

/// One queue per tenant, each ordered by arrival (`seq`).
#[derive(Debug, Clone, Default)]
pub struct MemoryParking {
    queues: BTreeMap<String, VecDeque<ParkedBundle>>,
}

impl MemoryParking {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The parked bundles of a tenant, oldest first.
    #[must_use]
    pub fn parked(&self, tenant: &str) -> Vec<&ParkedBundle> {
        self.queues
            .get(tenant)
            .map(|q| q.iter().collect())
            .unwrap_or_default()
    }
}

impl ParkingStore for MemoryParking {
    fn park(&mut self, parked: ParkedBundle) {
        let queue = self.queues.entry(parked.tenant.clone()).or_default();
        let at = queue.partition_point(|p| p.seq <= parked.seq);
        queue.insert(at, parked);
    }

    fn ready(&mut self, tenant: &str, applied_keys: &[RecordKey]) -> Vec<ParkedBundle> {
        let Some(queue) = self.queues.get_mut(tenant) else {
            return Vec::new();
        };
        for p in queue.iter_mut() {
            for key in applied_keys {
                p.missing.remove(key);
            }
        }
        let mut ready = Vec::new();
        let mut keep = VecDeque::with_capacity(queue.len());
        for p in queue.drain(..) {
            if p.missing.is_empty() {
                ready.push(p);
            } else {
                keep.push_back(p);
            }
        }
        *queue = keep;
        ready
    }

    fn take_oldest(&mut self, tenant: &str) -> Option<ParkedBundle> {
        self.queues.get_mut(tenant)?.pop_front()
    }

    fn len(&self, tenant: &str) -> usize {
        self.queues.get(tenant).map_or(0, VecDeque::len)
    }

    fn bytes(&self, tenant: &str) -> usize {
        self.queues
            .get(tenant)
            .map_or(0, |q| q.iter().map(|p| p.bytes).sum())
    }

    fn expire(&mut self, now_ms: u64, max_age_ms: u64) -> Vec<ParkedBundle> {
        let mut expired = Vec::new();
        for queue in self.queues.values_mut() {
            let mut keep = VecDeque::with_capacity(queue.len());
            for p in queue.drain(..) {
                if now_ms.saturating_sub(p.parked_at_ms) > max_age_ms {
                    expired.push(p);
                } else {
                    keep.push_back(p);
                }
            }
            *queue = keep;
        }
        expired.sort_by_key(|p| p.seq);
        expired
    }
}
