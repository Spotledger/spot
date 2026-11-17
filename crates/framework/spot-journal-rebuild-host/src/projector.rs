//! Projectors: the derived entities a rebuild recomputes after the images.
//!
//! The journal holds what happened; derived entities (graph edges, search projections, balance
//! tables) are computed from it. An owner that flags an entity derived must register a projector
//! for it; a derived entity with none is a **start-up error**
//! ([`crate::HostError::ProjectorMissing`], ADR-061 s6), found before anything is written.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::checkpoint::Fut;
use crate::error::HostError;

/// Recomputes derived entities over the whole rebuilt set.
pub trait Projector: Send + Sync {
    /// The projector's stable name (for the report and the audit).
    fn name(&self) -> &str;
    /// The derived entities it covers.
    fn entities(&self) -> Vec<String>;
    /// Recomputes them.
    fn rebuild(&self) -> Fut<'_, ()>;
}

/// The registered projectors.
#[derive(Default, Clone)]
pub struct Projectors(Vec<Arc<dyn Projector>>);

impl Projectors {
    /// None registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one.
    pub fn register(&mut self, projector: Arc<dyn Projector>) {
        self.0.push(projector);
    }

    /// Every registered projector, in registration order.
    #[must_use]
    pub fn all(&self) -> &[Arc<dyn Projector>] {
        &self.0
    }

    /// Checks that every entity in `derived` has a projector.
    ///
    /// # Errors
    /// [`HostError::ProjectorMissing`] naming the first (in name order) entity without one.
    pub fn check(&self, derived: &[String]) -> Result<(), HostError> {
        let covered: BTreeSet<String> = self.0.iter().flat_map(|p| p.entities()).collect();
        let wanted: BTreeSet<&String> = derived.iter().collect();
        match wanted.into_iter().find(|entity| !covered.contains(*entity)) {
            Some(entity) => Err(HostError::ProjectorMissing {
                entity: entity.clone(),
            }),
            None => Ok(()),
        }
    }
}
