//! Sender-side dependency closure and bundles (ADR-064 sections 1 to 5, WP-SYN-03.T1).
//!
//! Given the in-scope state events of one source transaction (roots and satellites) and what a
//! destination is known to have, [`build`] returns the bundle, or the ordered bundles, that never
//! leave a record without its dependencies. It is a pure library: the catalog
//! ([`References`]), the destination's knowledge ([`DestinationKnows`]), the source's records
//! ([`ImageSource`]) and the `excluded_fields.down` filter ([`Redactor`]) are traits, so there is
//! no clock, randomness, network, file or database here.
//!
//! Rule for doubt: include. A target is skipped only when the destination is known to have it;
//! an unknown acknowledged position, or a target out of the destination's scope, includes it.
//! The up direction runs the same code: hub-owned and mirrored targets are `owns`, so nothing is
//! added in the normal case.
//!
//! A record the destination holds — it cannot decode the record's schema major yet (ADR-066
//! section 5) — is never an extra, whatever its role, and a record whose closure needs one is
//! withheld with it ([`BundlePlan::withheld_by_hold`]). Sending the dependency anyway would hand
//! the node exactly the image the hold exists to withhold.

mod closure;
mod error;
mod extras;
mod model;
mod plan;
mod split;
mod static_refs;
mod traits;

pub use error::{
    ClosureError, INVALID_LIMITS, REFERENCE_UNREADABLE, SOURCE_FAILED, TARGET_MISSING, TOO_LARGE,
};
pub use extras::{ExtrasGrowth, ExtrasSet};
pub use model::{
    Bundle, BundleKind, BundlePlan, BundleRecord, Limits, Position, RecordKey, Role, SourceImage,
};
pub use plan::build;
pub use static_refs::{LinkSpec, LinkTarget, StaticRedactor, StaticReferences};
pub use traits::{DestinationKnows, ImageSource, NoRedaction, Redactor, References, element_id};

/// A record's closure needs a record the destination holds, so the dependent is withheld with it
/// instead of being sent without its dependency ([`BundlePlan::withheld_by_hold`]). Catalogue
/// code of a routing decision, never an error.
pub const HELD_DEPENDENCY: &str = "sync_closure.held_dependency";
