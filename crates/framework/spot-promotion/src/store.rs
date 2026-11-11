//! Where promotions live between steps.
//!
//! The store holds the whole [`Promotion`]: its state, its journal, the plan, the captured
//! pre-image and the evidence. Persisting the journal with the state is what makes a resume
//! possible: a process that dies after apply finds the promotion in [`PromotionState::Applied`]
//! and continues with verification instead of applying twice.

use std::collections::BTreeMap;
use std::sync::Mutex;

use spot_migration_audit::RunId;

use crate::error::PromotionError;
use crate::state::Promotion;

/// A promotion store.
///
/// [`MemoryPromotionStore`] is the in-process implementation the tests and a single-node host
/// use. A durable host implements this trait over its control store; nothing in the pipeline
/// assumes the promotions live in memory.
pub trait PromotionStore: Send + Sync {
    /// Adds a new promotion.
    ///
    /// # Errors
    /// [`PromotionError::Duplicate`] when the id is taken, [`PromotionError::StoreFailed`] when
    /// the backend fails.
    fn insert(&self, promotion: Promotion) -> Result<(), PromotionError>;

    /// Replaces an existing promotion.
    ///
    /// # Errors
    /// [`PromotionError::NotFound`] when it does not exist, [`PromotionError::StoreFailed`] when
    /// the backend fails.
    fn save(&self, promotion: Promotion) -> Result<(), PromotionError>;

    /// Loads one promotion.
    ///
    /// # Errors
    /// [`PromotionError::StoreFailed`] when the backend fails.
    fn load(&self, id: &RunId) -> Result<Option<Promotion>, PromotionError>;

    /// Every promotion, in id order.
    ///
    /// # Errors
    /// [`PromotionError::StoreFailed`] when the backend fails.
    fn list(&self) -> Result<Vec<Promotion>, PromotionError>;
}

/// An in-memory [`PromotionStore`].
#[derive(Debug, Default)]
pub struct MemoryPromotionStore {
    promotions: Mutex<BTreeMap<String, Promotion>>,
}

impl MemoryPromotionStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The store's lock, or a store failure.
    fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Promotion>>, PromotionError> {
        self.promotions
            .lock()
            .map_err(|_| PromotionError::StoreFailed {
                message: "the promotion store lock is poisoned".to_owned(),
            })
    }
}

impl PromotionStore for MemoryPromotionStore {
    fn insert(&self, promotion: Promotion) -> Result<(), PromotionError> {
        let id = promotion.id().to_string();
        let mut promotions = self.lock()?;
        if promotions.contains_key(&id) {
            return Err(PromotionError::Duplicate { id });
        }
        promotions.insert(id, promotion);
        Ok(())
    }

    fn save(&self, promotion: Promotion) -> Result<(), PromotionError> {
        let id = promotion.id().to_string();
        let mut promotions = self.lock()?;
        if !promotions.contains_key(&id) {
            return Err(PromotionError::NotFound { id });
        }
        promotions.insert(id, promotion);
        Ok(())
    }

    fn load(&self, id: &RunId) -> Result<Option<Promotion>, PromotionError> {
        Ok(self.lock()?.get(id.as_str()).cloned())
    }

    fn list(&self) -> Result<Vec<Promotion>, PromotionError> {
        Ok(self.lock()?.values().cloned().collect())
    }
}
