//! Image upcasters (WP-JRN-06.T1, ADR-061 s3 and s6).
//!
//! A rebuild applies images written under older catalog versions into the current schema. Each
//! image records the `entity_schema` it was written under; between that version and the target
//! one there is a chain of **upcasters**, one per shape change, and applying them in order is
//! the only way an old image becomes a current one.
//!
//! This crate is the mechanism, not the wiring:
//!
//! * [`Image`] is a document image — the entity, the catalog version it was written under, and
//!   the whole document body (its declared fields and its child rows).
//! * [`Registry::register`] stores one [`Upcaster`] per entity and `from` version, and refuses a
//!   duplicate or a step that does not move forward.
//! * [`upcast`] walks the chain from the image's version to the target, applying each step in
//!   turn. A version it cannot cross is [`UpcastError::NoPath`] naming the first missing step,
//!   an image already at the target is returned unchanged, and an image newer than the target
//!   is [`UpcastError::FromTheFuture`].
//! * [`steps`] holds the declarative builders patches are written with, so a step is data
//!   rather than a hand-written function: [`steps::rename_field`], [`steps::drop_field`],
//!   [`steps::add_field_with_default`], [`steps::move_field_to_child`],
//!   [`steps::retype_field`].
//! * [`check_patch`] takes the shape changes a patch declares and reports every one that has no
//!   upcaster covering its version range. CI calls it; an additive change (a new optional
//!   field, a new entity) needs none.
//!
//! # Purity
//!
//! A step is a function of the image alone. No step reads the wall clock, a random source, the
//! filesystem, the network or a key, and no value is ever a float that the crate does arithmetic
//! on: amounts and quantities stay the encoded values ADR-055 defines. Applying the same chain
//! to the same image twice therefore gives the same bytes, which is what a rebuild relies on.
//!
//! # Shape changes
//!
//! [`ShapeChangeKind`] uses the vocabulary of `ChangeKind` in `crates/framework/spot-migration-plan`
//! (identical variant names) rather than inventing a second set of words for the same events. The
//! two enums cannot be shared — this crate depends on `serde`, `serde_json` and `thiserror` only —
//! so the names are kept in step by hand and a test pins their snake-case spelling.

mod error;
mod image;
mod patch;
mod registry;
mod steps;

#[cfg(test)]
mod tests;

pub use error::UpcastError;
pub use image::{Body, Image};
pub use patch::{CheckFinding, ShapeChange, ShapeChangeKind, check_patch};
pub use registry::{Registry, Upcaster, upcast};
pub use steps::{
    Step, add_field_with_default, drop_field, move_field_to_child, rename_field, retype_field,
};
