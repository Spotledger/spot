//! The `QUARANTINED` condition: raised for every item a person must decide, cleared when the item
//! leaves that state. The catalogue (`spot-sync-conditions`) has it Red, subject kind `record`,
//! parameters `node`, `record` and `cause`.
//!
//! [`QuarantinedSync::reconcile`] compares the open set it raised last time with the items now
//! held and calls the port for the difference only, so a quiet queue costs nothing and a restart
//! (which forgets the set) re-raises, which the conditions engine debounces. Parked dependents and
//! items under an automatic retry are not raised: the retry schedule raises a condition itself
//! when its cap is reached, and a parked dependent waits behind its parent.

use std::collections::BTreeSet;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{Item, State};

/// The condition id.
pub const CONDITION_ID: &str = "QUARANTINED";

/// Where the condition is raised and cleared (the conditions service, or a fake in tests).
#[async_trait]
pub trait ConditionPort: Send + Sync {
    /// Raises `QUARANTINED` on `subject` (the item id) with its parameters.
    ///
    /// # Errors
    ///
    /// A reason when the engine refused or has stopped.
    async fn raise(
        &self,
        subject: &str,
        node: &str,
        record: &str,
        cause: &str,
    ) -> Result<(), String>;

    /// Records that `subject` is no longer abnormal.
    ///
    /// # Errors
    ///
    /// A reason when the engine refused or has stopped.
    async fn clear(&self, subject: &str) -> Result<(), String>;
}

/// What one reconcile did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Subjects raised now.
    pub raised: Vec<String>,
    /// Subjects cleared now.
    pub cleared: Vec<String>,
    /// Subjects the port refused (they stay unraised and are tried again next time).
    pub failed: Vec<String>,
}

/// The set of items this module has raised the condition for.
#[derive(Default)]
pub struct QuarantinedSync {
    open: Mutex<BTreeSet<String>>,
}

impl QuarantinedSync {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many subjects are currently raised.
    pub fn raised(&self) -> usize {
        self.open.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Raises for every `Held` item not yet raised and clears every subject that is no longer a
    /// `Held` item. `items` is the whole summary of the tenant.
    pub async fn reconcile(&self, items: &[Item], port: &dyn ConditionPort) -> Reconciled {
        let wanted: BTreeSet<String> = items
            .iter()
            .filter(|item| item.state == State::Held)
            .map(|item| item.id.clone())
            .collect();
        let before = self.open.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut done = Reconciled::default();
        let mut now = before.clone();
        for item in items.iter().filter(|i| wanted.contains(&i.id)) {
            if before.contains(&item.id) {
                continue;
            }
            match port
                .raise(&item.id, &item.node, &item.record, &item.reason)
                .await
            {
                Ok(()) => {
                    now.insert(item.id.clone());
                    done.raised.push(item.id.clone());
                }
                Err(_) => done.failed.push(item.id.clone()),
            }
        }
        for subject in before.difference(&wanted) {
            match port.clear(subject).await {
                Ok(()) => {
                    now.remove(subject);
                    done.cleared.push(subject.clone());
                }
                Err(_) => done.failed.push(subject.clone()),
            }
        }
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = now;
        done
    }
}

/// [`ConditionPort`] over the site's conditions service.
pub struct HandleConditions {
    handle: crate::services::conditions::ConditionsHandle,
    clock: std::sync::Arc<dyn spot_core::Clock>,
}

impl HandleConditions {
    /// A port over `handle`, stamping with `clock`.
    pub fn new(
        handle: crate::services::conditions::ConditionsHandle,
        clock: std::sync::Arc<dyn spot_core::Clock>,
    ) -> Self {
        Self { handle, clock }
    }

    fn now(&self) -> crate::services::conditions::Millis {
        let millis = self.clock.now_utc().unix_timestamp_nanos() / 1_000_000;
        crate::services::conditions::Millis::new(u64::try_from(millis.max(0)).unwrap_or(0))
    }
}

#[async_trait]
impl ConditionPort for HandleConditions {
    async fn raise(
        &self,
        subject: &str,
        node: &str,
        record: &str,
        cause: &str,
    ) -> Result<(), String> {
        let params = crate::services::conditions::ConditionParams::new()
            .with("node", node)
            .with("record", record)
            .with("cause", cause);
        self.handle
            .raise(CONDITION_ID, subject, params, self.now())
            .await
            .map_err(|e| e.to_string())?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn clear(&self, subject: &str) -> Result<(), String> {
        self.handle
            .observe_cleared(CONDITION_ID, subject, self.now())
            .await
            .map_err(|e| e.to_string())?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
