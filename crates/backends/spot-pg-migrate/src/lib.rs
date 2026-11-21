//! Online schema migration for the PostgreSQL backend (WP-STO-11.T1).
//!
//! [`LiveSchema::read`] reads the database, [`plan`] diffs it against a catalogue built from
//! effective metadata and orders the safe changes, and [`apply`] runs them online: each step in
//! a short transaction with lock and statement timeouts, index builds concurrently, a required
//! field added to a populated table filled in batches, a session advisory lock so two nodes
//! cannot migrate at once, and a re-plan after the lock so a rerun does only what is left.
//!
//! A destructive or lossy change (dropping, renaming, narrowing, changing a type family,
//! tightening a constraint, changing the collation) is **refused**, with a named reason, and a
//! plan with any refusal is never applied. There is no force flag. Those changes belong to the
//! expand and contract path of the migration lifecycle (WP-MIG).
//!
//! Spatial, range and search constructs (ADR-087) add two things: a plan **requires** an
//! extension and refuses, naming the `extension.install` operation, when the database lacks it
//! (it never creates one); and an exclusion constraint is planned after the columns and indexes,
//! as `Risk::NeedsValidation` on a table that holds rows. See the `plan` module docs.
//!
//! A table that is not a document (the edge tables, the audit chain tables, a mail log) reaches
//! the database only one way: the owning crate hands its idempotent `CREATE` statements to
//! [`PlanOptions::extra_tables`] as [`ExtraTable`]s, and they run as plan steps after every
//! document step, under the same advisory lock. The rules are in the `extra` module docs (see
//! [`ExtraTable`]); a `prepare()` side door is not a supported way to create such a table.

mod apply;
mod conn;
mod ddl;
mod error;
mod extension;
mod extra;
mod live;
mod names;
mod plan;

pub use apply::{ApplyOptions, ApplyReport, apply, dry_run};
pub use error::{MigrateError, Refusal, codes};
pub use extension::{
    ALLOWED_EXTENSIONS, AuditSink, EXTENSION_INSTALL, ExtensionAuditEvent, ExtensionAuditOp,
    ExtensionAuditResult, ExtensionContext, ExtensionOutcome, ExtensionPlan, ExtensionRequest,
    OperationDescriptor, OperationRisk, ParameterDescriptor, apply_extension_install,
    plan_extension_install,
};
pub use extra::ExtraTable;
pub use live::{
    LiveColumn, LiveConstraint, LiveExtension, LiveIndex, LiveSchema, LiveTable, MANAGED_EXTENSIONS,
};
pub use plan::{
    BackfillSpec, ExclusionConstraint, ExclusionElement, ExtensionNeed, MigrationPlan, PlanOptions,
    Risk, Run, Step, check_extension, default_min_version, extension_step, plan, plan_hash_of,
    plan_with, required_extensions, version_at_least,
};
