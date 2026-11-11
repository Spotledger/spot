//! Permission compilation to the storage query IR (WP-PLT-04.T1; ADR-070 sections 6 and 12).
//!
//! A list, a report or an export must return exactly the rows the principal may read, in one
//! query, with no per-row permission call. This crate turns the decision core's rules
//! (`spot-permissions`) into a filter on the storage query IR ([`PermissionFilter::for_read`]),
//! refuses the queries that would leak through filtering, sorting or aggregating
//! ([`PermissionFilter::check_query`]), reduces a select list to the columns the principal may
//! see ([`PermissionFilter::restrict_columns`]), and says which values a restricted field may
//! take ([`PermissionFilter::lookup_filter`]).
//!
//! It is pure: no storage access, no clock. `tests/equivalence.rs` proves, over a seeded
//! corpus run through the in-memory backend, that list results equal the decision core's
//! `can(Read)` document by document.
//!
//! Both paths fail closed: a field name the permission snapshot does not know is not readable
//! by any principal (it is refused where a query filters, sorts, groups, has or joins on it,
//! and dropped from the select list), and a system principal reads a known field at the plain
//! level — reveal is a separate grant, not a default.
//!
//! The owner rules compare the user with a creator column of the DocType; its name and the
//! source alias come in a [`ReadTarget`].

mod analysis;
mod check;
mod columns;
mod error;
mod lookup;
mod pred;
mod read;

pub use columns::{MaskPlan, MaskedColumn, Restricted};
pub use error::PermQueryError;
pub use lookup::LookupFilter;
pub use read::{MAX_FILTER_NODES, MAX_IN_LIST, PermissionFilter, ReadFilter, ReadTarget};
