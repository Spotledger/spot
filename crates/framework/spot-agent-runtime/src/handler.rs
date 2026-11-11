//! The runtime module: the ports it composes, the one entry point the executor calls, and the
//! [`OperationHandler`] that plugs it into the host agent.
//!
//! [`RuntimeModule::run`] takes a catalogue operation id, its validated arguments and the
//! command instant, and performs the operation through the injected ports. It is the single
//! place the console, the CLI and the privileged executor all reach the same behaviour. The
//! [`RuntimeHandler`] is a thin adapter over it, so the executor runs exactly the catalogue's
//! operations and nothing else.

use std::sync::Arc;

use spot_agent_core::{ExecutionPlan, HandlerOutcome, HandlerRegistry, OperationHandler};
use spot_mgmt_ops::{OpId, ParamValue, Validated};
use spot_settings::{MemoryStore, ScopeRef, SettingsRegistry, ValueStore};

use crate::backup::{
    BackupKind, BackupPort, BackupRequest, BackupStarted, BackupVerification, list_backups,
    start_backup, verify_backup,
};
use crate::catalogue::{RuntimeCatalogue, specs};
use crate::error::{RuntimeError, RuntimeResult};
use crate::ids;
use crate::params::{ParamReading, ParamSetReport, PgAdminPort, read_parameter, set_parameter};
use crate::profiles::{Profile, ProfileReport, apply_profile};
use crate::replica::{
    PromoteReport, ReplicaLag, ReplicaPort, ReplicaStatus, StandbyOutcome, StandbyRequest,
    create_standby, lag, promote, status,
};
use crate::roles::{RoleReport, read_roles, set_roles};
use crate::runtime::{RuntimeControl, StartReport, StopReport, start_runtime, stop_runtime};
use crate::settings::runtime_registry;
use crate::upgrade::{
    ApplyPort, ApprovalPort, ReleasePlanner, UpgradeOutcome, UpgradeReport, resume_upgrade,
    start_upgrade,
};
use crate::value;

/// The settings registry and store the module reads and writes through.
pub struct SettingsPorts {
    /// The typed schema every key must be registered in.
    pub registry: SettingsRegistry,
    /// The value store diffs are written to.
    pub store: Arc<dyn ValueStore>,
}

impl SettingsPorts {
    /// The runtime registry over a fresh in-memory store.
    ///
    /// # Errors
    /// [`RuntimeError::Settings`] when the compiled-in schema breaks a rule (a programming
    /// mistake proven absent by the crate's tests).
    pub fn runtime() -> RuntimeResult<Self> {
        let registry = runtime_registry().map_err(|error| RuntimeError::Settings {
            detail: format!("{error}"),
        })?;
        Ok(Self {
            registry,
            store: Arc::new(MemoryStore::new()),
        })
    }

    /// The runtime registry over a caller-supplied store.
    ///
    /// # Errors
    /// [`RuntimeError::Settings`] when the compiled-in schema breaks a rule.
    pub fn with_store(store: Arc<dyn ValueStore>) -> RuntimeResult<Self> {
        let registry = runtime_registry().map_err(|error| RuntimeError::Settings {
            detail: format!("{error}"),
        })?;
        Ok(Self { registry, store })
    }
}

/// Every port the runtime module acts through.
pub struct RuntimePorts {
    /// PostgreSQL parameter administration.
    pub pg: Arc<dyn PgAdminPort>,
    /// Replica administration.
    pub replica: Arc<dyn ReplicaPort>,
    /// Backup administration.
    pub backup: Arc<dyn BackupPort>,
    /// Runtime service control.
    pub runtime: Arc<dyn RuntimeControl>,
    /// The release planner.
    pub planner: Arc<dyn ReleasePlanner>,
    /// The approval flow.
    pub approval: Arc<dyn ApprovalPort>,
    /// The apply engine.
    pub apply: Arc<dyn ApplyPort>,
}

/// The database and runtime operation module.
pub struct RuntimeModule {
    ports: RuntimePorts,
    settings: SettingsPorts,
    node_id: String,
}

impl RuntimeModule {
    /// A module over `ports` and `settings` for the node named `node_id`.
    #[must_use]
    pub fn new(ports: RuntimePorts, settings: SettingsPorts, node_id: impl Into<String>) -> Self {
        Self {
            ports,
            settings,
            node_id: node_id.into(),
        }
    }

    /// The scope this node's settings are read and written at.
    #[must_use]
    pub fn scope(&self) -> ScopeRef {
        ScopeRef::Server(self.node_id.clone())
    }

    /// Reads a PostgreSQL parameter.
    ///
    /// # Errors
    /// [`RuntimeError::UnknownParameter`], the port's error, or a missing argument.
    pub fn read_parameter(&self, args: &Validated) -> RuntimeResult<ParamReading> {
        let name = value::text(args, ids::DB_PARAM_GET, "name")?;
        read_parameter(self.ports.pg.as_ref(), &name)
    }

    /// Sets a PostgreSQL parameter.
    ///
    /// # Errors
    /// [`RuntimeError::UnknownParameter`], [`RuntimeError::ValueOutOfRange`], the port's
    /// reload/restart error, or a missing argument.
    pub fn set_parameter(&self, args: &Validated) -> RuntimeResult<ParamSetReport> {
        let name = value::text(args, ids::DB_PARAM_SET, "name")?;
        let raw = value::text(args, ids::DB_PARAM_SET, "value")?;
        set_parameter(self.ports.pg.as_ref(), &name, &raw)
    }

    /// Reads replica status.
    ///
    /// # Errors
    /// The port's error.
    pub fn replica_status(&self) -> RuntimeResult<ReplicaStatus> {
        status(self.ports.replica.as_ref())
    }

    /// Reads replica lag.
    ///
    /// # Errors
    /// The port's error.
    pub fn replica_lag(&self) -> RuntimeResult<ReplicaLag> {
        lag(self.ports.replica.as_ref())
    }

    /// Creates a standby.
    ///
    /// # Errors
    /// [`RuntimeError::InvalidArgument`] or the port's error.
    pub fn replica_create(&self, args: &Validated) -> RuntimeResult<StandbyOutcome> {
        let request = StandbyRequest {
            host: value::text(args, ids::DB_REPLICA_CREATE, "host")?,
            port: u16::try_from(value::integer(args, ids::DB_REPLICA_CREATE, "port")?).map_err(
                |_| RuntimeError::InvalidArgument {
                    op: ids::DB_REPLICA_CREATE.to_owned(),
                    param: "port".to_owned(),
                    detail: "must be between 1 and 65535".to_owned(),
                },
            )?,
            data_dir: value::text(args, ids::DB_REPLICA_CREATE, "data_dir")?,
            slot: value::text_opt(args, "slot"),
        };
        create_standby(self.ports.replica.as_ref(), &request)
    }

    /// Promotes this node, with the guard.
    ///
    /// # Errors
    /// [`RuntimeError::PromotePrimaryReachable`] or the port's error.
    pub fn replica_promote(&self, args: &Validated) -> RuntimeResult<PromoteReport> {
        promote(
            self.ports.replica.as_ref(),
            value::boolean(args, "critical"),
        )
    }

    /// Starts a backup.
    ///
    /// # Errors
    /// [`RuntimeError::InvalidArgument`] or the port's error.
    pub fn backup_start(&self, args: &Validated, now: i64) -> RuntimeResult<BackupStarted> {
        let kind_name = value::text(args, ids::DB_BACKUP_START, "kind")?;
        let kind =
            BackupKind::from_name(&kind_name).ok_or_else(|| RuntimeError::InvalidArgument {
                op: ids::DB_BACKUP_START.to_owned(),
                param: "kind".to_owned(),
                detail: "must be base or logical".to_owned(),
            })?;
        let request = BackupRequest {
            kind,
            label: value::text(args, ids::DB_BACKUP_START, "label")?,
        };
        start_backup(self.ports.backup.as_ref(), &request, now)
    }

    /// Verifies a backup.
    ///
    /// # Errors
    /// [`RuntimeError::BackupVerifyFailed`] or the port's error.
    pub fn backup_verify(&self, args: &Validated) -> RuntimeResult<BackupVerification> {
        let id = value::text(args, ids::DB_BACKUP_VERIFY, "id")?;
        verify_backup(self.ports.backup.as_ref(), &id)
    }

    /// Applies a profile.
    ///
    /// # Errors
    /// [`RuntimeError::UnknownProfile`], [`RuntimeError::RoleCombination`] or
    /// [`RuntimeError::Settings`].
    pub fn profile_apply(&self, args: &Validated) -> RuntimeResult<ProfileReport> {
        let name = value::text(args, ids::RUNTIME_PROFILE_APPLY, "profile")?;
        let profile = Profile::from_name(&name)
            .ok_or_else(|| RuntimeError::UnknownProfile { name: name.clone() })?;
        apply_profile(
            self.settings.store.as_ref(),
            &self.settings.registry,
            &self.scope(),
            profile,
        )
    }

    /// Starts an upgrade.
    ///
    /// # Errors
    /// The planner, approval or apply port's error.
    pub fn upgrade_start(&self, args: &Validated, now: i64) -> RuntimeResult<UpgradeReport> {
        let version = value::text(args, ids::RUNTIME_UPGRADE_START, "version")?;
        start_upgrade(
            self.ports.planner.as_ref(),
            self.ports.approval.as_ref(),
            self.ports.apply.as_ref(),
            &version,
            now,
        )
    }

    /// Resumes an interrupted upgrade.
    ///
    /// # Errors
    /// The planner or apply port's error.
    pub fn upgrade_resume(&self, args: &Validated, now: i64) -> RuntimeResult<UpgradeReport> {
        let version = value::text(args, ids::RUNTIME_UPGRADE_RESUME, "version")?;
        resume_upgrade(
            self.ports.planner.as_ref(),
            self.ports.apply.as_ref(),
            &version,
            now,
        )
    }

    /// Runs one operation and returns a short, secret-free note on success.
    ///
    /// A rejected or rolled-back upgrade is an [`Err`]: that is how the executor records the
    /// operation as failed while the evidence (cause and mechanism) travels in the error.
    ///
    /// # Errors
    /// Any [`RuntimeError`] the operation produced.
    pub fn run(&self, op: &OpId, args: &Validated, now: i64) -> RuntimeResult<String> {
        let segments: Vec<&str> = op.segments().collect();
        match segments.as_slice() {
            ["db", "param", "get"] => {
                let reading = self.read_parameter(args)?;
                Ok(format!(
                    "{} = {} {} (restart needed: {}, pending: {})",
                    reading.name,
                    reading.value,
                    reading.unit.label(),
                    reading.restart_needed,
                    reading.pending_restart
                ))
            }
            ["db", "param", "set"] => {
                let report = self.set_parameter(args)?;
                Ok(format!(
                    "{} set to {} and applied with a {}",
                    report.name,
                    report.value,
                    report.method.label()
                ))
            }
            ["db", "replica", "status"] => {
                let status = self.replica_status()?;
                Ok(format!(
                    "role {}, streaming {}",
                    status.role.label(),
                    status.streaming
                ))
            }
            ["db", "replica", "lag"] => {
                let lag = self.replica_lag()?;
                Ok(format!(
                    "lag {} bytes, streaming {}",
                    lag.bytes, lag.streaming
                ))
            }
            ["db", "replica", "create"] => {
                let outcome = self.replica_create(args)?;
                Ok(format!("standby created in {}", outcome.data_dir))
            }
            ["db", "replica", "promote"] => {
                let report = self.replica_promote(args)?;
                Ok(report.note)
            }
            ["db", "backup", "start"] => {
                let started = self.backup_start(args, now)?;
                Ok(format!(
                    "{} backup {} written to {}",
                    started.kind.name(),
                    started.id,
                    started.location
                ))
            }
            ["db", "backup", "list"] => {
                let entries = list_backups(self.ports.backup.as_ref())?;
                Ok(format!("{} backups", entries.len()))
            }
            ["db", "backup", "verify"] => {
                let report = self.backup_verify(args)?;
                Ok(format!(
                    "{} checked: {} files, {} bytes",
                    report.id, report.files_checked, report.bytes_checked
                ))
            }
            ["node", "role", "get"] => {
                let roles = read_roles(
                    self.settings.store.as_ref(),
                    &self.settings.registry,
                    &self.scope(),
                )?;
                Ok(format!("roles: {}", roles.names().join(", ")))
            }
            ["node", "role", "set"] => {
                let roles = value::list_text(args, ids::NODE_ROLE_SET, "roles")?;
                let report = self.set_roles(&roles)?;
                Ok(format!(
                    "roles now {}; restarts: {}",
                    report
                        .preview
                        .after
                        .iter()
                        .map(|role| role.name())
                        .collect::<Vec<_>>()
                        .join(", "),
                    report
                        .preview
                        .restarts
                        .iter()
                        .map(|restart| restart.service.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            ["runtime", "profile", "apply"] => {
                let report = self.profile_apply(args)?;
                Ok(format!(
                    "profile {} applied ({} settings)",
                    report.profile.name(),
                    report.changes.len()
                ))
            }
            ["runtime", "upgrade", "start"] => {
                let report = self.upgrade_start(args, now)?;
                self.upgrade_note(report)
            }
            ["runtime", "upgrade", "resume"] => {
                let report = self.upgrade_resume(args, now)?;
                self.upgrade_note(report)
            }
            ["runtime", "service", "stop"] => {
                let drain = value::duration_secs(args, "drain_secs");
                let report = stop_runtime(self.ports.runtime.as_ref(), drain, now)?;
                Ok(format!(
                    "runtime stopped after {}s drain",
                    report.drain.waited_secs
                ))
            }
            ["runtime", "service", "start"] => {
                let report = start_runtime(self.ports.runtime.as_ref())?;
                Ok(format!("runtime started: {}", report.started))
            }
            other => Err(RuntimeError::Unsupported {
                what: format!("operation {other:?}"),
            }),
        }
    }

    /// Replaces the node's role set.
    ///
    /// # Errors
    /// [`RuntimeError::RoleCombination`] or [`RuntimeError::Settings`].
    pub fn set_roles(&self, roles: &[String]) -> RuntimeResult<RoleReport> {
        set_roles(
            self.settings.store.as_ref(),
            &self.settings.registry,
            &self.scope(),
            roles,
        )
    }

    /// Turns an upgrade report into a success note or the failure error that carries the
    /// rollback evidence.
    fn upgrade_note(&self, report: UpgradeReport) -> RuntimeResult<String> {
        let digest = report.plan_digest.clone();
        match report.outcome {
            UpgradeOutcome::Rejected { by } => Err(RuntimeError::UpgradeRejected {
                digest,
                by: by.join(", "),
            }),
            UpgradeOutcome::RolledBack { evidence } => Err(RuntimeError::UpgradeRolledBack {
                cause: evidence.cause,
                mechanism: evidence.mechanism,
            }),
            outcome => Ok(format!("{} (plan {})", outcome.describe(), digest)),
        }
    }

    /// Stops the runtime with a drain.
    ///
    /// # Errors
    /// [`RuntimeError::DrainTimeout`] or the port's error.
    pub fn stop_runtime(&self, drain_secs: u64, now: i64) -> RuntimeResult<StopReport> {
        stop_runtime(self.ports.runtime.as_ref(), drain_secs, now)
    }

    /// Starts the runtime.
    ///
    /// # Errors
    /// The port's error.
    pub fn start_runtime(&self) -> RuntimeResult<StartReport> {
        start_runtime(self.ports.runtime.as_ref())
    }
}

/// The [`OperationHandler`] that runs the runtime module's operations.
pub struct RuntimeHandler {
    module: Arc<RuntimeModule>,
}

impl RuntimeHandler {
    /// A handler over `module`.
    #[must_use]
    pub fn new(module: Arc<RuntimeModule>) -> Self {
        Self { module }
    }

    /// The module behind the handler.
    #[must_use]
    pub fn module(&self) -> &Arc<RuntimeModule> {
        &self.module
    }

    /// Registers this handler for every operation in this crate's catalogue.
    pub fn register(handler: &Arc<Self>, registry: &mut HandlerRegistry) {
        for spec in specs() {
            let registered: Arc<dyn OperationHandler> = handler.clone();
            registry.register(spec.id.clone(), registered);
        }
    }
}

impl OperationHandler for RuntimeHandler {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        match self.module.run(plan.op(), plan.args(), plan.issued_at()) {
            Ok(_note) => HandlerOutcome::Succeeded,
            Err(error) => HandlerOutcome::Failed {
                detail: error.detail(),
            },
        }
    }
}

impl std::fmt::Debug for RuntimeHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeHandler")
            .field("module", &crate::CRATE_NAME)
            .finish()
    }
}

/// The `ParamValue` a role-list argument carries, for callers that build one.
#[must_use]
pub fn role_list_value(roles: &[String]) -> ParamValue {
    ParamValue::List(roles.iter().cloned().map(ParamValue::Text).collect())
}

/// The catalogue this module registers.
#[must_use]
pub fn catalogue_module() -> RuntimeCatalogue {
    RuntimeCatalogue
}
