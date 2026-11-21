//! `spot-agent-runtime` — the host agent's PostgreSQL and Spotledger runtime modules
//! (blueprint WS22, WP-ADM-06, ADR-44).
//!
//! This is the database-and-runtime half of the host agent's operation suite. Every operation
//! is a typed management operation (WP-ADM-01), implemented as an
//! [`OperationHandler`](spot_agent_core::OperationHandler) so the privileged executor can run
//! exactly the catalogue's operations and nothing else.
//!
//! # The operations
//!
//! - [`params`]: `db.param.get` and `db.param.set` over a **typed allow-list** of PostgreSQL
//!   parameters, each with a unit and a restart-needed flag. A set writes `ALTER SYSTEM`
//!   through [`PgAdminPort`](params::PgAdminPort) and applies it with a reload or a restart,
//!   and the choice is reported. A parameter that is not on the list is refused.
//! - [`replica`]: `db.replica.status`, `db.replica.lag`, `db.replica.create` (a `pg_basebackup`
//!   standby) and `db.replica.promote`, with a guard that refuses to promote while the old
//!   primary is still reachable unless the caller carries the `critical` flag.
//! - [`backup`]: `db.backup.start` (a base backup or a logical dump), `db.backup.list` and
//!   `db.backup.verify`, returning the snapshot crate's own verification report.
//! - [`roles`]: `node.role.get` and `node.role.set` for the `web`, `jobs`, `realtime`,
//!   `database` and `branch-edge` roles. A role change is a settings diff (never a file write)
//!   plus a restart plan, and it previews what moves. `database` with `branch-edge` is refused.
//! - [`profiles`]: `runtime.profile.apply` for the `standalone`, `cluster-basic` and
//!   `cluster-redis` bundles, applied as one settings diff.
//! - [`upgrade`]: `runtime.upgrade.start` and `runtime.upgrade.resume`. The module builds a
//!   release plan with the migration planner, submits it for approval and — only once approved
//!   — asks the apply engine to run it. **On a failed verify it returns the engine's restore
//!   cause and mechanism as rollback evidence and marks the operation failed.** It contains no
//!   upgrade logic of its own.
//! - [`runtime`]: the [`RuntimeControl`](runtime::RuntimeControl) seam (stop with a drain, start)
//!   the host's service modules implement, exposed as `runtime.service.stop`/`.start`.
//!
//! # How it touches the machine
//!
//! It does not. Every database, service, snapshot and migration action goes through an injected
//! port ([`PgAdminPort`](params::PgAdminPort), [`ReplicaPort`](replica::ReplicaPort),
//! [`BackupPort`](backup::BackupPort), [`RuntimeControl`](runtime::RuntimeControl),
//! [`ReleasePlanner`](upgrade::ReleasePlanner), [`ApprovalPort`](upgrade::ApprovalPort),
//! [`ApplyPort`](upgrade::ApplyPort)); node roles and profiles go through the shared typed
//! settings store. That is what makes the module deterministic and testable without a database.
//!
//! # Purity
//!
//! No wall clock (time is an argument), no floats, no `unsafe` (the project coding rules), no
//! hand-built SQL outside the injected adapter, and no secret is ever logged or returned in a
//! `Debug`.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod backup;
pub mod catalogue;
pub mod error;
pub mod handler;
pub mod ids;
pub mod params;
pub mod profiles;
pub mod replica;
pub mod roles;
pub mod runtime;
pub mod settings;
pub mod upgrade;
pub mod value;

pub use backup::{
    BackupEntry, BackupKind, BackupPort, BackupRequest, BackupStarted, BackupVerification,
    list_backups, start_backup, verify_backup,
};
pub use catalogue::{RuntimeCatalogue, build_catalogue, specs};
pub use error::{RuntimeError, RuntimeResult};
pub use handler::{RuntimeHandler, RuntimeModule, RuntimePorts, SettingsPorts};
pub use params::{
    ApplyMethod, PG_PARAMETERS, ParamKind, ParamReading, ParamSetReport, PgAdminPort, PgParameter,
    PgValue, Unit, parse_value, read_parameter, set_parameter,
};
pub use profiles::{Profile, ProfileReport, apply_profile};
pub use replica::{
    PromoteReport, ReplicaLag, ReplicaPort, ReplicaRole, ReplicaStatus, StandbyOutcome,
    StandbyRequest, create_standby, lag, promote, status,
};
pub use roles::{
    NodeRole, RolePreview, RoleReport, RoleSet, ServiceRestart, read_roles, set_roles,
};
pub use runtime::{
    DrainReport, RuntimeControl, StartReport, StopReport, start_runtime, stop_runtime,
};
pub use settings::{NODE_PROFILE_KEY, NODE_ROLES_KEY, runtime_registry, runtime_specs};
pub use upgrade::{
    ApplyPort, Approval, ApprovalDecision, ApprovalPort, DescriptionPlanner, ReleasePlanner,
    RollbackEvidence, UpgradeOutcome, UpgradeReport, resume_upgrade, start_upgrade,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-agent-runtime";
