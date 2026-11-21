//! The registry of condition definitions.

use std::collections::BTreeMap;

use crate::def::ConditionDef;
use crate::error::ConditionsError;
use crate::id::ConditionId;

/// Every condition this deployment knows, keyed by id.
///
/// Registration is the only way in, and it refuses two things (ADR-067 section 1): an id that is
/// not upper snake case ([`ConditionsError::UnknownId`]) and an id that is already registered
/// ([`ConditionsError::DuplicateId`]). An app registers its own conditions through the same
/// [`Registry::register`] the platform's catalogue uses, so an app condition gets the same
/// lifecycle, display and notification as a platform one.
///
/// The registry is a definition store, not state: whether a condition is open belongs to the
/// engine ([`ConditionEngine`](crate::ConditionEngine)). Definitions are immutable once
/// registered; changing a severity or an escalation schedule in place would silently rewrite the
/// meaning of conditions that are already open.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    defs: BTreeMap<ConditionId, ConditionDef>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a definition.
    ///
    /// The escalation schedule is put in `after_millis` order here, once, so the schedule a
    /// caller reads back is the one that applies: the engine walks it from the first step and
    /// stops at the first one that is not due yet.
    ///
    /// # Errors
    ///
    /// [`ConditionsError::UnknownId`] when the id is not upper snake case (so it can never be a
    /// registered condition), and [`ConditionsError::DuplicateId`] when it is already registered.
    pub fn register(&mut self, mut def: ConditionDef) -> Result<(), ConditionsError> {
        let id = ConditionId::parse(&def.id)?;
        if self.defs.contains_key(&id) {
            return Err(ConditionsError::DuplicateId { id: def.id });
        }
        def.escalation.sort_by_key(|step| step.after_millis);
        self.defs.insert(id, def);
        Ok(())
    }

    /// The definition registered under `id`, if any.
    ///
    /// An id that is not upper snake case is simply not found: nothing can ever be registered
    /// under it.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ConditionDef> {
        self.defs.get(id)
    }

    /// Every definition, in id order.
    pub fn iter(&self) -> impl Iterator<Item = (&ConditionId, &ConditionDef)> + '_ {
        self.defs.iter()
    }

    /// How many definitions are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Whether no definition is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}
