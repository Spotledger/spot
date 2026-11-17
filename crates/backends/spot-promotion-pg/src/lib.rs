//! The PostgreSQL [`PromotionStore`](spot_promotion::PromotionStore) (WP-ALM-04.T6, ADR-082
//! class C).
//!
//! `spot-promotion` drives a bundle promotion through request, plan, approve, rehearse, apply
//! and verify and keeps the promotion between steps in a
//! [`PromotionStore`](spot_promotion::PromotionStore). Its in-memory store lives and dies with
//! the process, so a second `spot promote` run could not resume. [`PgPromotionStore`] keeps each
//! promotion as a versioned [`Snapshot`](spot_promotion::Snapshot) in one row of
//! `spot_control.spot_promotion` ([`schema`]).
//!
//! # Tenant scope
//!
//! A store is built for one tenant, the **target environment's tenant** (a promotion is kept with
//! the environment it changes). The tenant leads the primary key and every statement filters on
//! it, so two tenants may use the same promotion id and never see each other's rows. A promotion
//! whose target is another tenant is refused with `promotion.environment_mismatch`.
//!
//! # Where it may be used
//!
//! The trait is synchronous with no async alternative, so the store reaches the database through
//! the shared [`spot_pg_bridge::Bridge`]. That is legitimate only off the request path: **use it
//! from a control-plane process, a CLI command or a job worker, never from a request handler**.
//! Called from a thread inside a tokio runtime the bridge refuses with `pg_bridge.wrong_thread`,
//! and the store returns `promotion.store_failed` naming that code.
//!
//! # Semantics (the same as `MemoryPromotionStore`)
//!
//! * `insert`: stores the promotion at its revision (0), or `promotion.duplicate`.
//! * `load`: `None` for an unknown id. A row that no longer restores to a valid promotion is
//!   refused with `promotion_pg.snapshot_invalid` or `promotion_pg.snapshot_version`, never
//!   repaired.
//! * `save`: a compare-and-swap on the revision, in one transaction. `promotion.not_found` and
//!   `promotion_pg.stale_save` are told apart exactly; a stale save writes nothing.
//! * `list`: the tenant's promotions by id, in pages in one snapshot. Above [`LIST_CEILING`] it
//!   fails with `promotion.store_failed` rather than truncating.
//! * Every database error or bridge refusal is `promotion.store_failed` with only the bridge
//!   code or the SQLSTATE in the text: never SQL, a connection string, an id or a value. A
//!   timed-out write may or may not have been applied; load the promotion again before retrying.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
pub mod schema;
mod store;

pub use store::{LIST_CEILING, LIST_PAGE, PgPromotionStore, READ_DEADLINE, WRITE_DEADLINE};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-promotion-pg";
