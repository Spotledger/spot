//! The real adapters behind the control plane's ports (WP-CTL-01.T3, ADR-34, ADR-058).
//!
//! `spot-tenancy` (WP-CTL-01.T1) drives provisioning and deprovisioning through eight ports:
//! [`SchemaPort`](spot_tenancy::ports::SchemaPort), [`SiteSeeder`](spot_tenancy::ports::SiteSeeder),
//! [`IamPort`](spot_tenancy::ports::IamPort), [`LicencePort`](spot_tenancy::ports::LicencePort),
//! [`TelemetryPort`](spot_tenancy::ports::TelemetryPort), [`Archiver`](spot_tenancy::ports::Archiver),
//! [`ApprovalHook`](spot_tenancy::ports::ApprovalHook) and
//! [`AuditSink`](spot_tenancy::ports::AuditSink). This crate provides one real adapter per port:
//!
//! | Port | Adapter | Over |
//! | --- | --- | --- |
//! | schema, seeder | [`SchemaAdapter`], [`SeedAdapter`] | a [`MigrationRunner`], implemented by `spot-tenancy-pg`'s executor over `spot-pg-migrate` |
//! | licence | [`LicenceIssuer`] | `spot-license`'s `Issuer` plus a [`LicenceStore`] |
//! | telemetry | [`TelemetryAdapter`] | a [`TelemetryRegistry`] |
//! | IAM | [`IamAdapter`] | a [`UserProvisioning`] |
//! | archive | [`SnapshotArchiver`] | `spot-snapshot`'s [`SnapshotProvider`](spot_snapshot::SnapshotProvider) |
//! | approval | [`RoleApprovals`] | an [`ApprovalPolicy`] over the control plane's roles |
//! | audit | [`ChainAuditSink`] | `spot-audit-chain`'s `AuditChain` over an `AuditStore` |
//!
//! # Where the database lives
//!
//! ADR-45 keeps hand-written SQL inside the storage crates. The baseline migration and the
//! tenant schema therefore stay where they already are: the [`MigrationRunner`] seam is
//! implemented in `spot-tenancy-pg`, next to the executor that runs `spot-pg-migrate` against a
//! tenant's schema. This crate holds the port semantics (idempotency, the drop guard, the
//! failure codes); the backend crate holds the statements.
//!
//! # Fail closed
//!
//! A port the platform cannot satisfy yet never reports success. [`UnconfiguredIam`] and
//! [`UnconfiguredTelemetry`] are the defaults until their work packages land, and both fail
//! with [`AdapterError::NotConfigured`]. An approval hook that cannot resolve the tenant's
//! approvers returns an empty list — the control plane treats that as "nobody approved" — never
//! "anyone".
//!
//! # Idempotency
//!
//! The workflow re-runs a step after a crash without knowing whether its effect landed, so
//! every apply/undo here tolerates a replay: creating a schema or applying the baseline is
//! idempotent in the runner, an issued licence is reused rather than re-signed, a final backup
//! into the tenant's deterministic directory is discovered rather than taken twice, and a
//! revocation of an absent licence is a success. The audit sink deliberately appends every
//! attempt: the trail records attempts, and re-running a step must add a record, not erase one.
//!
//! # Time and secrets
//!
//! Time is always the injected [`Clock`](spot_core::Clock); nothing here reads the system clock
//! or the environment. Failure codes are fixed `&'static str`s ([`AdapterError::code`]) that
//! become a step's `reason`, so an error can never echo a licence key, a credential or SQL.

#![warn(missing_docs)]

mod approval;
mod archive;
mod audit;
mod block;
mod error;
mod iam;
mod licence;
mod migrate;
mod telemetry;

pub use approval::{ApprovalPolicy, RoleApprovals};
pub use archive::SnapshotArchiver;
pub use audit::ChainAuditSink;
pub use error::AdapterError;
pub use iam::{IamAdapter, UnconfiguredIam, UserProvisioning};
pub use licence::{LicenceIssuer, LicenceStore, LicenceWindow, MemoryLicenceStore};
pub use migrate::{MigrationRunner, SchemaAdapter, SeedAdapter};
pub use telemetry::{TelemetryAdapter, TelemetryRegistry, UnconfiguredTelemetry};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-tenancy-adapters";
