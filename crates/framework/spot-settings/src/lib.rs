//! The typed settings schema (WP-ADM-07, blueprint WS22, ADR-44).
//!
//! Every setting, from kernel parameters to company preferences, is described in one typed
//! schema (type, default, scope, restart needed, risk level, documentation). The console
//! generates its screens from that schema, with inheritance from fleet to company, "differs
//! from default" views and search.
//!
//! Pure library. No storage, no network, no clock, no HTTP, no filesystem. Values are read
//! and written through a trait.
//!
//! # Shape
//!
//! - [`SettingSpec`] — the schema for one setting: key, title, documentation, a
//!   [`ParamType`](spot_mgmt_ops::ParamType) value type, default, allowed scopes, restart
//!   hint, risk, and the secret/read-only and visibility hints. Serde and TOML-loadable.
//! - [`SettingsRegistry`] built by [`RegistryBuilder`] — validates every spec at once
//!   (duplicate keys, defaults, scope order, secret defaults, `depends_on`, deprecations) with
//!   stable `settings.*` codes. [`RegistryBuilder::register_all`] is the acceptance line: a
//!   spec an app registers appears with no further work.
//! - [`ValueStore`] — the object-safe, synchronous trait values are read and written through;
//!   [`MemoryStore`] is provided and applies a batch atomically.
//! - [`resolve`] — the effective value for a [`Chain`] of scopes: the narrowest set value
//!   wins, [`Origin`] says where it came from and [`Considered`] lists every scope checked.
//! - [`ChangeSet`] — a batch of changes validated as a whole, summarised for approval, with a
//!   stable order-independent [`ChangeSet::hash`].
//! - [`differs`] and [`effective_hash`] — the values that differ from their default, and the
//!   hash of every explicitly set effective value (the basis for ADM-08 drift detection).
//! - [`search`] — tokenised, case-insensitive, accent-folded search over key, title, summary,
//!   description and tags, with filters and stable ranking.
//! - [`screen_for`] — the generated screen model (groups and controls) the console renders,
//!   derived from the schema rather than written by hand.
//! - [`load_seed_catalogue`] — ten real platform settings compiled in from `settings/*.toml`.
//!
//! A value's typed validation is never reimplemented here: it calls
//! [`check_param_value`](spot_mgmt_ops::value_check::check_param_value) from `spot-mgmt-ops`
//! and maps every problem to a `settings.*` code with the same message.

#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod builder;
pub mod catalogue;
pub mod changes;
pub mod diff;
pub mod error;
pub mod model;
pub mod resolution;
pub mod screen;
pub mod search;
pub mod spec;
pub mod store;

mod hash;

#[cfg(test)]
mod tests;

pub use builder::{RegistryBuilder, SettingsRegistry};
pub use catalogue::{SEED_FILE_COUNT, load_seed_catalogue, parse_spec};
pub use changes::{
    AppliedChange, Change, ChangeSet, ChangeSummary, apply_changes, summarise, validate_changes,
};
pub use diff::{differs, effective_hash};
pub use error::{SettingsError, SettingsErrorKind, SettingsErrorReport};
pub use model::{
    Considered, DependsOn, Deprecated, Difference, EffectiveHash, EffectiveValue, Origin, Resolved,
    Restart, Scope, ScopeRef, SecretValue, StoredValue,
};
pub use resolution::{Chain, resolve};
pub use screen::{Control, Group, Row, Screen, ScreenModel, screen_for};
pub use search::{Filter as SearchFilter, Hit, MatchedField, search, search_counted};
pub use spec::SettingSpec;
/// The value model shared with management operations (re-exported for callers).
pub use spot_mgmt_ops::{ParamType, ParamValue, Risk};
pub use store::{AppliedWrite, MemoryStore, SettingWrite, ValueStore};
