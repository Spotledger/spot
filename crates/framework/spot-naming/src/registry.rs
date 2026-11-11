//! Where the namer finds a DocType's compiled naming plan.

use std::collections::BTreeMap;

use spot_plan::{NamingPlan, PlanSet};

/// The naming plans the namer works from, keyed by DocType name.
pub trait PlanRegistry: Send + Sync {
    /// The compiled naming plan of `doctype`, or `None` when the DocType has no naming
    /// property.
    fn naming_plan(&self, doctype: &str) -> Option<NamingPlan>;
}

impl PlanRegistry for PlanSet {
    fn naming_plan(&self, doctype: &str) -> Option<NamingPlan> {
        self.get(doctype).and_then(|plan| plan.naming().cloned())
    }
}

/// A fixed registry of naming plans, for tests and for hosts that hold plans themselves.
#[derive(Clone, Debug, Default)]
pub struct StaticPlans {
    plans: BTreeMap<String, NamingPlan>,
}

impl StaticPlans {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry with one more plan (builder form).
    pub fn with(mut self, doctype: &str, plan: NamingPlan) -> Self {
        self.insert(doctype, plan);
        self
    }

    /// Registers a DocType's plan.
    pub fn insert(&mut self, doctype: &str, plan: NamingPlan) {
        self.plans.insert(doctype.to_owned(), plan);
    }
}

impl PlanRegistry for StaticPlans {
    fn naming_plan(&self, doctype: &str) -> Option<NamingPlan> {
        self.plans.get(doctype).cloned()
    }
}
