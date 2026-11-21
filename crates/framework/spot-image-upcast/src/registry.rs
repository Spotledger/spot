//! Registration and the chaining walk.
//!
//! A registry holds at most one step per entity and `from` version. Registering the second one,
//! or a step whose `to` is not above its `from`, is refused at registration time, so the walk
//! never has to choose between two steps for the same version and never runs backwards.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::UpcastError;
use crate::image::Image;
use crate::steps::Step;

/// One registered rewrite: the images of `entity` at `from` become images at `to`.
pub struct Upcaster {
    /// The entity the step applies to.
    entity: String,
    /// The schema version the step starts at.
    from: u64,
    /// The schema version the step produces.
    to: u64,
    /// The rewrite itself.
    f: Step,
}

impl Upcaster {
    /// A step for `entity` from `from` to `to`.
    ///
    /// Nothing is checked here: [`Registry::register`] is the gate, and it refuses a duplicate
    /// and a step that does not move forward.
    #[must_use]
    pub fn new(entity: impl Into<String>, from: u64, to: u64, f: Step) -> Self {
        Self {
            entity: entity.into(),
            from,
            to,
            f,
        }
    }

    /// The entity the step applies to.
    #[must_use]
    pub fn entity(&self) -> &str {
        &self.entity
    }

    /// The schema version the step starts at.
    #[must_use]
    pub fn from(&self) -> u64 {
        self.from
    }

    /// The schema version the step produces.
    #[must_use]
    pub fn to(&self) -> u64 {
        self.to
    }

    /// Runs the rewrite. The walk calls this once the step is chosen.
    pub(crate) fn run(&self, image: &mut Image) -> Result<(), UpcastError> {
        (self.f)(image)
    }
}

impl fmt::Debug for Upcaster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upcaster")
            .field("entity", &self.entity)
            .field("from", &self.from)
            .field("to", &self.to)
            .finish_non_exhaustive()
    }
}

/// Every registered step, keyed by entity and then by the version it starts at.
#[derive(Debug, Default)]
pub struct Registry {
    /// entity -> from -> step.
    steps: BTreeMap<String, BTreeMap<u64, Upcaster>>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a step.
    ///
    /// Refused with [`UpcastError::DuplicateStep`] when the entity already has a step from the
    /// same version (whatever that step's `to` is: a version has exactly one successor), and with
    /// [`UpcastError::BackwardsStep`] when `to <= from`.
    pub fn register(&mut self, upcaster: Upcaster) -> Result<(), UpcastError> {
        if upcaster.to <= upcaster.from {
            return Err(UpcastError::BackwardsStep {
                entity: upcaster.entity,
                from: upcaster.from,
                to: upcaster.to,
            });
        }
        let per_entity = self.steps.entry(upcaster.entity.clone()).or_default();
        if per_entity.contains_key(&upcaster.from) {
            return Err(UpcastError::DuplicateStep {
                entity: upcaster.entity,
                from: upcaster.from,
                to: upcaster.to,
            });
        }
        per_entity.insert(upcaster.from, upcaster);
        Ok(())
    }

    /// The step that continues the chain at `from` for `entity`, when one is registered.
    #[must_use]
    pub fn next(&self, entity: &str, from: u64) -> Option<&Upcaster> {
        self.steps.get(entity).and_then(|steps| steps.get(&from))
    }

    /// The version the chain stops at on its way from `from` to `to`: the lower end of the first
    /// step that is missing or that would jump past `to`.
    ///
    /// `None` when registered steps carry the range, and when `from >= to` (a range that asks for
    /// no rewrite).
    #[must_use]
    pub fn missing_step(&self, entity: &str, from: u64, to: u64) -> Option<u64> {
        let mut at = from;
        while at < to {
            let Some(step) = self.next(entity, at) else {
                return Some(at);
            };
            if step.to > to {
                return Some(at);
            }
            at = step.to;
        }
        None
    }

    /// How many steps are registered, over every entity.
    #[must_use]
    pub fn step_count(&self) -> usize {
        self.steps.values().map(BTreeMap::len).sum()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Rewrites `image` into the shape of `target`.
    ///
    /// The chain starts at the image's own `entity_schema` and applies the registered step for
    /// each version in turn, recording the new version after each one.
    ///
    /// * An image already at `target` is returned unchanged.
    /// * An image newer than `target` is [`UpcastError::FromTheFuture`]: a rebuild never
    ///   downgrades a document.
    /// * A missing step (or one that would jump past `target`) is [`UpcastError::NoPath`],
    ///   naming the entity, the version the chain stopped at and the target.
    /// * A step's own error is returned as it was raised; the walk does not re-wrap it, so a
    ///   `StepShape` still names the field that stopped it.
    pub fn upcast(&self, mut image: Image, target: u64) -> Result<Image, UpcastError> {
        let entity = image.entity().to_owned();
        let mut at = image.entity_schema();
        if at > target {
            return Err(UpcastError::FromTheFuture {
                entity,
                schema: at,
                target,
            });
        }
        while at < target {
            let Some(step) = self.next(&entity, at) else {
                return Err(UpcastError::NoPath {
                    entity,
                    from: at,
                    to: target,
                });
            };
            if step.to > target {
                return Err(UpcastError::NoPath {
                    entity,
                    from: at,
                    to: target,
                });
            }
            step.run(&mut image)?;
            at = step.to;
            image.set_schema(at);
        }
        Ok(image)
    }
}

/// Rewrites `image` into the shape of `target_schema` through the steps `registry` holds.
///
/// The function form of [`Registry::upcast`], for callers that hold the registry by reference.
pub fn upcast(registry: &Registry, image: Image, target_schema: u64) -> Result<Image, UpcastError> {
    registry.upcast(image, target_schema)
}
