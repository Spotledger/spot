//! DocType metadata for the Spotledger Platform (WP-MET-01.T1; ADR-05, ADR-16, ADR-33).
//!
//! A DocType is defined in TOML, one file per DocType ([`Definition`], [`load_dir`]). A site may
//! add custom fields and override a few properties at runtime ([`Customisations`]), with no
//! rebuild. [`Registry::build`] merges the two into [`EffectiveMeta`], which carries a metadata
//! version and compiles to the two views the platform needs:
//!
//! - [`EffectiveMeta::catalog`]: the storage `Catalog` that backends create tables from;
//! - [`EffectiveMeta::doctype`]: the app API's `DocTypeDef`.
//!
//! Both are deterministic. Persisting metadata, versioned snapshots and cross-node invalidation
//! come later (WP-MET-01.T2, WP-MET-03).
//!
//! # Engine-managed fields
//!
//! Every posted DocType gets `doc_state` and `amended_from` in both views. Both names are
//! reserved on every DocType. `nullable` is always true in storage: writes are full and
//! required-ness is enforced by the engine.

mod compile;
mod custom;
mod definition;
pub mod derive;
pub mod descriptor;
mod error;
pub mod formats;
mod registry;
mod report;
mod validate;

pub use compile::{
    BLIND_INDEX_SUFFIX, ReverseIndex, SEALED_SUFFIX, blind_index_column,
    catalog_for_with_reverse_indexes, catalog_with_reverse_indexes, sealed_column,
};
pub use custom::{CUSTOM_PREFIX, CustomField, Customisations, Override, Property};
pub use definition::{
    CONTROL_HINTS, Definition, ExclusionSpec, FieldLookup, FieldSpec, FieldType, IndexSpec,
    LookupRule, LookupSpec, MaskSpec, PLATFORM_LINK_SEARCH, ProtectionGroupSpec, SearchFieldSpec,
    SearchSpec, load_dir,
};
pub use derive::{Derivation, DerivationSource, FieldRef, InheritanceMap, compute_inheritance};
pub use error::{MetaError, MetaErrorKind};
pub use registry::{BuildOptions, EffectiveMeta, FieldProtection, FieldRules, Registry};
pub use report::{CompileReport, Warning};
pub use validate::{AMENDED_FROM, DOC_STATE, ENGINE_FIELDS, MAX_SCALE};
