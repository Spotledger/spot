//! Typed errors with stable `image_upcast.*` codes.
//!
//! A message names an entity, a schema version, a field or a step. It never carries a field
//! value: an image holds row data, and an error message is logged and travel to clients.

/// Every error the registry, a step and [`crate::upcast`] return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpcastError {
    /// No registered step continues the chain at `from`, so the image cannot reach `to`.
    ///
    /// `from` is the version the chain stops at (the first missing step's lower end) and `to`
    /// is the schema that was asked for.
    #[error("entity {entity}: no upcaster from schema {from} towards schema {to}")]
    NoPath {
        /// The entity the image belongs to.
        entity: String,
        /// The schema the chain stops at.
        from: u64,
        /// The schema that was asked for.
        to: u64,
    },
    /// The image was written under a newer catalog version than the target.
    #[error("entity {entity}: image schema {schema} is newer than the target schema {target}")]
    FromTheFuture {
        /// The entity the image belongs to.
        entity: String,
        /// The schema the image was written under.
        schema: u64,
        /// The schema that was asked for.
        target: u64,
    },
    /// A step is already registered for this entity and `from` version.
    #[error(
        "entity {entity}: an upcaster from schema {from} is already registered (this one goes to {to})"
    )]
    DuplicateStep {
        /// The entity.
        entity: String,
        /// The version the offered step starts at.
        from: u64,
        /// The version the offered step ends at.
        to: u64,
    },
    /// A step does not move forward: its `to` is not above its `from`.
    #[error(
        "entity {entity}: the upcaster from schema {from} to schema {to} does not move forward"
    )]
    BackwardsStep {
        /// The entity.
        entity: String,
        /// The version the offered step starts at.
        from: u64,
        /// The version the offered step ends at.
        to: u64,
    },
    /// A step met a document shape it cannot rewrite without losing data.
    #[error("step {step}: field {field} {reason}")]
    StepShape {
        /// The step builder that refused.
        step: &'static str,
        /// The field the step was working on.
        field: String,
        /// What is wrong with the shape, as a phrase that follows the field name.
        reason: &'static str,
    },
    /// A `retype_field` converter refused a value.
    #[error("step retype_field: field {field} cannot be converted: {reason}")]
    ConvertFailed {
        /// The field being retyped.
        field: String,
        /// Why the value was refused. A converter names the shape, never the value.
        reason: String,
    },
    /// The value handed in is not a well-formed document image.
    #[error("the image is not a well-formed document image: {0}")]
    ImageInvalid(String),
}

impl UpcastError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoPath { .. } => "image_upcast.no_path",
            Self::FromTheFuture { .. } => "image_upcast.from_the_future",
            Self::DuplicateStep { .. } => "image_upcast.duplicate_step",
            Self::BackwardsStep { .. } => "image_upcast.backwards_step",
            Self::StepShape { .. } => "image_upcast.step_shape",
            Self::ConvertFailed { .. } => "image_upcast.convert_failed",
            Self::ImageInvalid(_) => "image_upcast.image_invalid",
        }
    }
}
