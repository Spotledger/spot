//! The plan cache: the current [`PlanSet`] behind a lock-free pointer.
//!
//! Readers never block and never see a half-built set (arc-swap): `current()` is a single
//! atomic pointer read. Refreshes are serialised between themselves with a mutex, so the
//! compare-then-store of a refresh is atomic without ever blocking a reader; a failed rebuild
//! simply never stores, leaving the previous set in place.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use spot_meta::EffectiveMeta;

use crate::error::PlanError;
use crate::set::PlanSet;

/// The cache of compiled plans; the plan set version-keyed on the metadata.
#[derive(Debug)]
pub struct PlanCache {
    current: ArcSwap<PlanSet>,
    /// Serialises refreshes among themselves; readers never take it.
    refresh_gate: Mutex<()>,
}

impl PlanCache {
    /// A cache holding `initial` as the current plan set.
    #[must_use]
    pub fn new(initial: PlanSet) -> Self {
        Self {
            current: ArcSwap::from(Arc::new(initial)),
            refresh_gate: Mutex::new(()),
        }
    }

    /// The current plan set, lock-free. An operation keeps the `Arc` it took: it always sees
    /// one consistent set, whatever refreshes happen meanwhile.
    #[must_use]
    pub fn current(&self) -> Arc<PlanSet> {
        self.current.load_full()
    }

    /// Swaps in the plans of `meta`, rebuilding them on every call.
    ///
    /// The metadata version is **not** a cache key: two independently built
    /// [`EffectiveMeta`] values can both report the same version while describing different
    /// fields, rules or links. What makes a refresh cheap is the plan hash, not the version —
    /// every DocType whose plan hash did not change **reuses its old `Arc`**, so a refresh of
    /// unchanged metadata returns an empty list and a held plan stays valid for a DocType the
    /// metadata did not really change. The return value lists (in name order) every DocType
    /// whose plan changed: new ones and rebuilt ones. A DocType removed from the metadata is
    /// simply gone from the new set. The new set carries `meta.version()` as information
    /// ([`PlanSet::meta_version`]).
    ///
    /// A failed rebuild leaves the current set in place and returns the errors.
    ///
    /// # Errors
    ///
    /// The errors [`PlanSet::build`] reports, when the new metadata does not compile.
    pub fn refresh(&self, meta: &EffectiveMeta) -> Result<Vec<String>, Vec<PlanError>> {
        // A poisoned gate means a refresh panicked mid-build; proceeding is still safe: the
        // current set was never changed and the rebuild below is the same work.
        let gate = match self.refresh_gate.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let old = self.current.load_full();
        let fresh = PlanSet::build(meta)?;
        let mut plans = BTreeMap::new();
        let mut changed: Vec<String> = Vec::new();
        for (name, new_plan) in fresh.plans() {
            let reuse = old
                .get(name)
                .is_some_and(|old_plan| old_plan.plan_hash() == new_plan.plan_hash());
            if reuse && let Some(old_plan) = old.get(name) {
                plans.insert(name.clone(), old_plan);
            } else {
                plans.insert(name.clone(), Arc::clone(new_plan));
                changed.push(name.clone());
            }
        }
        self.current
            .store(Arc::new(PlanSet::from_parts(meta.version(), plans)));
        drop(gate);
        Ok(changed)
    }
}
