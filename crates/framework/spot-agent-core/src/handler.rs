//! The handler registry: no ambient authority.
//!
//! The executor runs **only** the handlers registered here, keyed by catalogue operation
//! id. An operation the catalogue knows but that has no registered handler is refused
//! ([`Refusal::UnknownHandler`]); there is no default handler and no way to run anything
//! else. A handler is an injected trait object, so the executor itself never reaches for
//! a shell, a path or an environment variable.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_mgmt_ops::OpId;

use crate::plan::ExecutionPlan;

/// What running one operation produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandlerOutcome {
    /// The operation completed.
    Succeeded,
    /// The operation failed; the detail is a short, secret-free description.
    Failed {
        /// What went wrong, in the handler's words.
        detail: String,
    },
}

/// One operation implementation. Handlers do the privileged work; the executor only
/// decides *whether* they run.
pub trait OperationHandler: Send + Sync {
    /// Runs the validated plan and reports the outcome.
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome;
}

/// The registered handlers, keyed by the catalogue operation id.
#[derive(Default)]
pub struct HandlerRegistry {
    handlers: BTreeMap<OpId, Arc<dyn OperationHandler>>,
}

impl HandlerRegistry {
    /// An empty registry: every operation is refused.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `handler` for `op`, replacing any previous handler for that id.
    pub fn register(&mut self, op: OpId, handler: Arc<dyn OperationHandler>) {
        self.handlers.insert(op, handler);
    }

    /// The handler for `op`, when one is registered.
    #[must_use]
    pub fn get(&self, op: &OpId) -> Option<&Arc<dyn OperationHandler>> {
        self.handlers.get(op)
    }

    /// Whether a handler is registered for `op`.
    #[must_use]
    pub fn contains(&self, op: &OpId) -> bool {
        self.handlers.contains_key(op)
    }

    /// How many handlers are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

impl std::fmt::Debug for HandlerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerRegistry")
            .field("registered", &self.handlers.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn registry_lookup_is_by_operation_id() {
        let mut registry = HandlerRegistry::new();
        assert!(registry.is_empty());
        registry.register(OpId::new("server.service.restart").unwrap(), Arc::new(Echo));
        assert_eq!(registry.len(), 1);
        assert!(registry.contains(&OpId::new("server.service.restart").unwrap()));
        assert!(!registry.contains(&OpId::new("server.service.stop").unwrap()));
    }

    struct Echo;

    impl OperationHandler for Echo {
        fn handle(&self, _plan: &ExecutionPlan) -> HandlerOutcome {
            HandlerOutcome::Succeeded
        }
    }
}
