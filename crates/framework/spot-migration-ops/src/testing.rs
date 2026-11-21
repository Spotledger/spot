//! Test doubles: an in-memory [`PlanStore`] and a fixed [`Clock`].

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::context::{Clock, PlanHash, PlanState, PlanStore, PlanSummary, StoredPlan};
use crate::error::LifecycleError;

/// A clock that returns what it was told.
#[derive(Debug, Default)]
pub struct FixedClock(AtomicI64);

impl FixedClock {
    /// A clock at `now`.
    #[must_use]
    pub fn new(now: i64) -> Self {
        Self(AtomicI64::new(now))
    }

    /// Moves the clock.
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A [`PlanStore`] in memory.
#[derive(Debug, Default)]
pub struct InMemoryPlanStore {
    plans: Mutex<BTreeMap<PlanHash, StoredPlan>>,
}

impl InMemoryPlanStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl PlanStore for InMemoryPlanStore {
    fn save(&self, plan: StoredPlan) -> Result<(), LifecycleError> {
        let mut plans = self.plans.lock().map_err(|_| LifecycleError::Store)?;
        match plans.get(&plan.hash) {
            Some(existing) if existing.json == plan.json && existing.author == plan.author => {
                Ok(())
            }
            Some(_) => Err(LifecycleError::PlanChanged),
            None => {
                plans.insert(plan.hash.clone(), plan);
                Ok(())
            }
        }
    }

    fn load(&self, hash: &PlanHash) -> Result<Option<StoredPlan>, LifecycleError> {
        let plans = self.plans.lock().map_err(|_| LifecycleError::Store)?;
        Ok(plans.get(hash).cloned())
    }

    fn list(&self) -> Result<Vec<PlanSummary>, LifecycleError> {
        let plans = self.plans.lock().map_err(|_| LifecycleError::Store)?;
        let mut out: Vec<PlanSummary> = plans
            .values()
            .map(|p| PlanSummary {
                hash: p.hash.clone(),
                author: p.author.clone(),
                created_at: p.created_at,
                state: p.state,
                version: p.version,
            })
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.hash.cmp(&b.hash)));
        Ok(out)
    }

    fn mark_state(
        &self,
        hash: &PlanHash,
        expected: PlanState,
        expected_version: u64,
        next: PlanState,
    ) -> Result<u64, LifecycleError> {
        let mut plans = self.plans.lock().map_err(|_| LifecycleError::Store)?;
        let plan = plans.get_mut(hash).ok_or(LifecycleError::UnknownPlan)?;
        if plan.state != expected || plan.version != expected_version {
            return Err(LifecycleError::PlanChanged);
        }
        plan.state = next;
        plan.version = plan.version.saturating_add(1);
        Ok(plan.version)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn hash(c: char) -> PlanHash {
        PlanHash::parse(&c.to_string().repeat(64)).unwrap()
    }

    fn plan(c: char) -> StoredPlan {
        StoredPlan {
            hash: hash(c),
            json: "{}".to_owned(),
            author: "user:alice".to_owned(),
            created_at: 10,
            state: PlanState::Proposed,
            version: 1,
        }
    }

    #[test]
    fn round_trip_and_list() {
        let store = InMemoryPlanStore::new();
        store.save(plan('a')).unwrap();
        store.save(plan('a')).unwrap();
        assert_eq!(
            store.load(&hash('a')).unwrap().unwrap().author,
            "user:alice"
        );
        assert!(store.load(&hash('b')).unwrap().is_none());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn same_hash_with_other_text_is_plan_changed() {
        let store = InMemoryPlanStore::new();
        store.save(plan('a')).unwrap();
        let mut other = plan('a');
        other.json = "{\"x\":1}".to_owned();
        assert_eq!(store.save(other), Err(LifecycleError::PlanChanged));
    }

    #[test]
    fn state_change_is_compare_and_set() {
        let store = InMemoryPlanStore::new();
        store.save(plan('a')).unwrap();
        let v = store
            .mark_state(&hash('a'), PlanState::Proposed, 1, PlanState::Approved)
            .unwrap();
        assert_eq!(v, 2);
        // The loser of a race sees plan_changed.
        assert_eq!(
            store.mark_state(&hash('a'), PlanState::Proposed, 1, PlanState::Rejected),
            Err(LifecycleError::PlanChanged)
        );
        assert_eq!(
            store.mark_state(&hash('b'), PlanState::Proposed, 1, PlanState::Rejected),
            Err(LifecycleError::UnknownPlan)
        );
    }

    #[test]
    fn hashes_must_be_64_lowercase_hex() {
        assert!(PlanHash::parse("abc").is_err());
        assert!(PlanHash::parse(&"G".repeat(64)).is_err());
        assert!(PlanHash::parse(&"A".repeat(64)).is_err());
        assert!(PlanHash::parse(&"0".repeat(64)).is_ok());
    }

    #[test]
    fn plan_state_names_round_trip() {
        for s in [
            PlanState::Proposed,
            PlanState::Approved,
            PlanState::Rehearsed,
            PlanState::Applying,
            PlanState::Applied,
            PlanState::Failed,
            PlanState::Rejected,
            PlanState::Superseded,
        ] {
            assert_eq!(PlanState::parse(s.as_str()), Some(s));
        }
        assert_eq!(PlanState::parse("x"), None);
    }

    #[test]
    fn anonymous_actor_is_refused() {
        use crate::context::{Actor, Channel};
        use spot_permissions::Principal;
        assert!(Actor::new(Principal::default(), Channel::Cli).is_err());
        let actor = Actor::new(Principal::with_roles("alice", &[]), Channel::Mgmt).unwrap();
        assert_eq!(actor.user_id(), "user:alice");
        assert_eq!(actor.channel().as_str(), "mgmt");
    }
}
