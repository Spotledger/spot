//! Curated, typed, versioned PostgreSQL reporting views for direct BI access (WP-PRD-07.T1).
//!
//! People point Power BI, Excel and other tools straight at the database. They must see stable,
//! typed views and never the transactional tables or a column the permission model protects.
//! This crate generates those views from the storage catalogue and a [`BiPolicy`], creates a
//! `NOLOGIN` read-only role for them, and checks, with real statements, that the role reaches
//! nothing else.
//!
//! # The pieces
//!
//! - [`BiPolicy`]: which entities are exposed (nothing by default), their columns, `posted_only`
//!   (default true), child tables, title fields and versions. Loaded from TOML with every problem
//!   reported at once.
//! - [`Protection`]: the fields that are never shown: protection-group members, secret fields.
//!   The storage catalogue does not carry this, so the caller supplies it. A policy cannot
//!   include a protected field.
//! - [`generate`]: the pure step. Catalogue, policy and protection in; a [`BiPlan`] out, with the
//!   SQL, the columns, and the [`Exclusion`]s an administrator reads.
//! - [`apply`]: makes the `bi` schema match the plan, in one transaction. Views are replaced in
//!   place only when their columns are the plan's, plus more at the end. A removed or retyped
//!   column needs a new `version`; the old view is kept.
//! - [`retire`]: drops one version explicitly. Nothing else is ever dropped.
//! - [`verify`]: as the BI role, checks what it can and cannot read or write.
//!
//! # Rules the views follow
//!
//! - Names are quoted; values never reach SQL except as quoted literals. Storage names are
//!   validated by `Name`, and the quoting here is defence in depth.
//! - Views run with the owner's rights, so the role needs no grant on base tables. Each view has
//!   `security_barrier`, so a caller's filter cannot push past the view's own predicate.
//! - A document view shows rows with `docstatus = 1` only, unless the policy says otherwise.
//!   A child view inherits its parent's filter. A link name joins the target with the same filter.
//! - Money and quantities are `numeric`, instants `timestamptz`, dates `date`, booleans `boolean`
//!   (ADR-12, ADR-73: typed values, never formatted strings).
//! - Nothing here moves data. Read replicas and analytics mirrors are later tasks (ADR-058).
//!
//! # Not in this crate
//!
//! BI user creation (the admin console owns credentials; [`grant_role_sql`] only adds the
//! membership), the GL and stock views (ERP stage), the read replica and the analytics mirror.

mod db;
mod error;
mod plan;
mod policy;
mod protect;
mod sql;
mod verify;

pub use db::{ApplyReport, apply, grant_role_sql, retire};
pub use error::{BiError, BiErrors};
pub use plan::{BiPlan, Exclusion, ExclusionReason, PlannedColumn, ViewPlan, generate};
pub use policy::{BiPolicy, DEFAULT_BASE_SCHEMA, EntityPolicy};
pub use protect::Protection;
pub use sql::{
    BI_SCHEMA, CONNECTION_LIMIT, IDLE_TRANSACTION_TIMEOUT, MAX_IDENT_BYTES, POSTED_FIELD,
    READER_ROLE, STATEMENT_TIMEOUT,
};
pub use verify::{VerifyCheck, VerifyReport, verify};
