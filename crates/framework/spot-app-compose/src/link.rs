//! Runtime linking: which component instances link to which, how they pool, and how a composed
//! call's fuel is bounded by — and charged to — the caller.
//!
//! A [`ComposedInstance`] is a **description** for the `spot-wasm` host: it says which app answers
//! which interface for which importer, how many instances each app pools, and the per-app per-call
//! fuel. It runs nothing. The host links the described components and enforces the limits.
//!
//! The accounting rule is the one the brief asks for: an extension cannot make its provider run
//! unbounded. A composed call is a stack of nested calls; the call may use at most the **smallest**
//! per-call fuel budget on the stack, and whatever it uses is charged to the **outermost** caller,
//! never to the provider. A provider with a generous budget cannot widen an extension's call.

use std::collections::BTreeMap;

use semver::Version;
use spot_wasm::Limits;

use crate::interface::Capability;
use crate::problem::{Problem, Problems, codes};
use crate::resolve::Resolution;

/// One app's share of the instance pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceSlot {
    /// The app.
    pub app: String,
    /// How many component instances of it may run at once.
    pub slots: u32,
}

/// How the composed apps share the host's instance pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolPlan {
    /// The pool size: how many composed calls may run at once.
    pub max_concurrent_calls: u32,
    /// One slot count per app.
    pub slots: Vec<InstanceSlot>,
}

impl PoolPlan {
    /// An even split of `max_concurrent_calls` across `apps`: each gets at least one slot, and the
    /// remainder goes to the first apps in name order, so the plan is deterministic.
    #[must_use]
    pub fn even(apps: &[String], max_concurrent_calls: u32) -> Self {
        let max = max_concurrent_calls.max(1);
        let mut sorted: Vec<String> = apps.to_vec();
        sorted.sort();
        sorted.dedup();
        let count = u32::try_from(sorted.len()).unwrap_or(u32::MAX).max(1);
        let base = max / count;
        let remainder = max % count;
        let slots = sorted
            .into_iter()
            .enumerate()
            .map(|(i, app)| InstanceSlot {
                app,
                slots: base + u32::from((i as u32) < remainder),
            })
            .collect();
        Self {
            max_concurrent_calls: max,
            slots,
        }
    }

    /// The slots `app` may use; at least one, so an app is never starved to zero.
    #[must_use]
    pub fn slots_for(&self, app: &str) -> u32 {
        self.slots
            .iter()
            .find(|slot| slot.app == app)
            .map_or(1, |slot| slot.slots.max(1))
    }
}

/// One link the host must make: `importer` calls `interface`, answered by `provider` at `version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The app that imports.
    pub importer: String,
    /// The interface it calls.
    pub interface: String,
    /// The app that answers.
    pub provider: String,
    /// The exact interface version linked.
    pub version: Version,
    /// The capabilities the importer obtains through this link, sorted.
    pub grants: Vec<Capability>,
}

/// One step of a composed call: an app and the interface it was reached through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallStep {
    /// The app running at this step.
    pub app: String,
    /// The interface it was reached through; empty for the entry app.
    pub interface: String,
}

/// The stack of apps a composed call runs through, the entry app first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallChain {
    /// The steps, outermost first.
    pub steps: Vec<CallStep>,
}

impl CallChain {
    /// The app the call is charged to: the outermost caller.
    #[must_use]
    pub fn caller(&self) -> &str {
        self.steps.first().map_or("", |step| step.app.as_str())
    }
}

/// What one composed call may use, and who pays for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuelCharge {
    /// The app the fuel is charged to: the outermost caller.
    pub caller: String,
    /// The fuel the call used.
    pub fuel: u64,
    /// The budget it ran under: the smallest per-call fuel on the stack.
    pub limit: u64,
}

/// A composed instance: the links, the pool and the per-app limits, for the `spot-wasm` host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposedInstance {
    /// The links to make, sorted by (importer, interface).
    pub links: Vec<Link>,
    /// The pool plan.
    pub pool: PoolPlan,
    /// The default per-call limits for an app with no explicit fuel.
    pub limits: Limits,
    /// The per-app per-call fuel budget; an app not listed uses [`ComposedInstance::limits`].
    pub fuel_by_app: BTreeMap<String, u64>,
}

impl ComposedInstance {
    /// Builds a composed instance from a resolution, a pool plan, the default limits and the
    /// per-app fuel budgets.
    #[must_use]
    pub fn from_resolution(
        resolution: &Resolution,
        pool: PoolPlan,
        limits: Limits,
        fuel_by_app: BTreeMap<String, u64>,
    ) -> Self {
        let links = resolution
            .bindings
            .iter()
            .map(|binding| Link {
                importer: binding.importer.clone(),
                interface: binding.interface.clone(),
                provider: binding.provider.clone(),
                version: binding.version.clone(),
                grants: resolution.obtained_by(&binding.importer).to_vec(),
            })
            .collect();
        Self {
            links,
            pool,
            limits,
            fuel_by_app,
        }
    }

    /// The per-call fuel budget for `app`: its explicit budget, or the default.
    #[must_use]
    pub fn fuel_for(&self, app: &str) -> u64 {
        self.fuel_by_app
            .get(app)
            .copied()
            .unwrap_or(self.limits.fuel)
    }

    /// The link that answers `interface` for `importer`, when there is one.
    #[must_use]
    pub fn link(&self, importer: &str, interface: &str) -> Option<&Link> {
        self.links
            .iter()
            .find(|link| link.importer == importer && link.interface == interface)
    }

    /// The call chain a call from `entry` into `interface` runs through: `entry` first, then every
    /// app reachable through links, in name order, each once.
    ///
    /// # Errors
    ///
    /// `app_compose.unknown_interface` when `entry` has no link for `interface`.
    pub fn call_chain(&self, entry: &str, interface: &str) -> Result<CallChain, Problems> {
        if self.link(entry, interface).is_none() {
            return Err(Problems::new(vec![Problem::new(
                codes::UNKNOWN_INTERFACE,
                format!("app `{entry}` does not link interface `{interface}`"),
                &[entry],
            )]));
        }
        let mut steps = vec![CallStep {
            app: entry.to_owned(),
            interface: interface.to_owned(),
        }];
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        seen.insert(entry.to_owned());
        let mut frontier = vec![entry.to_owned()];
        while let Some(current) = frontier.pop() {
            let mut providers: Vec<&Link> = self
                .links
                .iter()
                .filter(|link| link.importer == current)
                .collect();
            providers.sort_by(|a, b| (&a.provider, &a.interface).cmp(&(&b.provider, &b.interface)));
            for link in providers {
                if seen.insert(link.provider.clone()) {
                    steps.push(CallStep {
                        app: link.provider.clone(),
                        interface: link.interface.clone(),
                    });
                    frontier.push(link.provider.clone());
                }
            }
        }
        Ok(CallChain { steps })
    }

    /// Charges `used` fuel to the chain's outermost caller, under the smallest per-call fuel on
    /// the chain.
    ///
    /// # Errors
    ///
    /// `app_compose.fuel_exhausted` when `used` exceeds the effective budget; the charge still
    /// names the caller and the budget, so the report says who tried to run too long.
    pub fn attribute_fuel(&self, chain: &CallChain, used: u64) -> Result<FuelCharge, Problems> {
        let limit = chain
            .steps
            .iter()
            .map(|step| self.fuel_for(&step.app))
            .min()
            .unwrap_or(0);
        let charge = FuelCharge {
            caller: chain.caller().to_owned(),
            fuel: used,
            limit,
        };
        if used > limit {
            return Err(Problems::new(vec![Problem::new(
                codes::FUEL_EXHAUSTED,
                format!(
                    "the call from `{}` used {used} fuel; the caller's budget is {limit}",
                    charge.caller
                ),
                &[charge.caller.as_str()],
            )]));
        }
        Ok(charge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_even_split_is_deterministic() {
        let apps = vec!["b".to_owned(), "a".to_owned(), "c".to_owned()];
        let pool = PoolPlan::even(&apps, 8);
        assert_eq!(pool.max_concurrent_calls, 8);
        assert_eq!(pool.slots_for("a"), 3);
        assert_eq!(pool.slots_for("b"), 3);
        assert_eq!(pool.slots_for("c"), 2);
        assert_eq!(pool.slots_for("missing"), 1);
    }
}
