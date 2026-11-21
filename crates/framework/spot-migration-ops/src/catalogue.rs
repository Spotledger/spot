//! The `site.migrate.*` operation entries and the fail-closed handler registry.
//!
//! The entries follow the operation set of the management API contract 0.2.0
//! (`contracts/management-api/openapi.yaml`, `x-spot-operations`); a test reads that file and
//! checks every id, risk and parameter name against it.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_mgmt_ops::{
    CatalogueBuilder, OpId, OperationModule, OperationSpec, ParamSpec, ParamType, Risk, TargetKind,
    Validated,
};
use spot_migration_apply::BoxFuture;

use crate::context::LifecycleContext;
use crate::error::LifecycleError;

/// The module name used in catalogue errors.
pub const MIGRATION_OPS_MODULE: &str = "migration-ops";

const SITE_PATTERN: &str = "^[a-z][a-z0-9-]{1,62}$";
const HASH_PATTERN: &str = "^[0-9a-f]{64}$";

/// What a handler returns on success: a short message and plain key/value fields (no secrets).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandlerOutput {
    /// A one-line human summary.
    pub message: String,
    /// Structured fields for `--json` and the API.
    pub fields: BTreeMap<String, String>,
}

/// One operation implementation.
pub trait OperationHandler: Send + Sync {
    /// Runs the validated invocation.
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>>;
}

/// Handlers keyed by operation id. An id with no handler is refused by the caller.
#[derive(Default)]
pub struct HandlerRegistry {
    handlers: BTreeMap<OpId, Arc<dyn OperationHandler>>,
}

impl HandlerRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or replaces) the handler for `op`.
    pub fn register(&mut self, op: OpId, handler: Arc<dyn OperationHandler>) {
        self.handlers.insert(op, handler);
    }

    /// The handler for `op`.
    #[must_use]
    pub fn get(&self, op: &OpId) -> Option<&Arc<dyn OperationHandler>> {
        self.handlers.get(op)
    }

    /// The registered ids, sorted.
    pub fn ids(&self) -> impl Iterator<Item = &OpId> {
        self.handlers.keys()
    }

    /// How many handlers are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

impl std::fmt::Debug for HandlerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerRegistry")
            .field("registered", &self.handlers.len())
            .finish()
    }
}

/// The fail-closed stub: refuses with `migration.not_implemented`.
#[derive(Clone, Debug)]
pub struct NotImplemented {
    op: String,
}

impl NotImplemented {
    /// A stub for `op`.
    #[must_use]
    pub fn new(op: &OpId) -> Self {
        Self {
            op: op.as_str().to_owned(),
        }
    }
}

impl OperationHandler for NotImplemented {
    fn handle<'a>(
        &'a self,
        _call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        let op = self.op.clone();
        Box::pin(async move { Err(LifecycleError::NotImplemented { op }) })
    }
}

fn text(name: &str, max_len: usize, pattern: Option<&str>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: pattern.map(str::to_owned),
        },
        required: true,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

fn site_param() -> ParamSpec {
    text(
        "site",
        63,
        Some(SITE_PATTERN),
        "A registered site id (never a path).",
    )
}

fn hash_param() -> ParamSpec {
    text(
        "plan_hash",
        64,
        Some(HASH_PATTERN),
        "The hex digest of `spot-migration-approval::PlanDigest` of the approved plan; apply runs exactly that plan.",
    )
}

fn reason_param() -> ParamSpec {
    text("reason", 500, None, "Why (recorded in the audit).")
}

/// A parameter set of the operations.
#[derive(Clone, Copy)]
enum Params {
    Site,
    SiteHash,
    SiteHashReason,
    SiteReason,
}

fn params(shape: Params) -> Vec<ParamSpec> {
    match shape {
        Params::Site => vec![site_param()],
        Params::SiteHash => vec![site_param(), hash_param()],
        Params::SiteHashReason => vec![site_param(), hash_param(), reason_param()],
        Params::SiteReason => vec![site_param(), reason_param()],
    }
}

/// `(id, title, summary, risk, parameters, requires)` for the ten `site.migrate.*` operations.
/// `requires` mirrors the contract (`signature`, `approval:migration`); the executor enforces it.
type Row = (
    &'static str,
    &'static str,
    &'static str,
    Risk,
    Params,
    &'static [&'static str],
);

const ROWS: [Row; 10] = [
    (
        "site.migrate.plan",
        "Plan a migration",
        "Create a migration plan and its digest.",
        Risk::Low,
        Params::Site,
        &["signature"],
    ),
    (
        "site.migrate.show",
        "Show a migration plan",
        "Show a migration plan: risk class, estimated lock time, steps.",
        Risk::Read,
        Params::SiteHash,
        &[],
    ),
    (
        "site.migrate.approve",
        "Approve a migration plan",
        "Approve a plan. The approver must differ from the plan author.",
        Risk::Medium,
        Params::SiteHash,
        &["signature"],
    ),
    (
        "site.migrate.reject",
        "Reject a migration plan",
        "Reject a plan. The rejecter must differ from the plan author.",
        Risk::Medium,
        Params::SiteHashReason,
        &["signature"],
    ),
    (
        "site.migrate.rehearse",
        "Rehearse a migration plan",
        "Rehearse the approved plan on a copy and report invariants, fingerprint and replay differences.",
        Risk::Medium,
        Params::SiteHash,
        &["signature", "approval:migration"],
    ),
    (
        "site.migrate.apply",
        "Apply a migration plan",
        "Apply exactly the approved plan.",
        Risk::Critical,
        Params::SiteHash,
        &["signature", "approval:migration"],
    ),
    (
        "site.migrate.hold",
        "Hold a migration",
        "Hold a running migration at its next checkpoint.",
        Risk::Medium,
        Params::SiteReason,
        &["signature"],
    ),
    (
        "site.migrate.release",
        "Release a held migration",
        "Release a held migration.",
        Risk::Medium,
        Params::Site,
        &["signature"],
    ),
    (
        "site.migrate.rescue",
        "Rescue a failed migration",
        "Roll a failed migration back to its last safe state.",
        Risk::Critical,
        Params::SiteHash,
        &["signature", "approval:migration"],
    ),
    (
        "site.migrate.audit",
        "Show the migration audit trail",
        "Show the audit trail of migrations of the site.",
        Risk::Read,
        Params::Site,
        &[],
    ),
];

/// The operation specs, in contract order.
#[must_use]
pub fn specs() -> Vec<OperationSpec> {
    ROWS.iter()
        .filter_map(|(id, title, summary, risk, shape, _)| {
            Some(OperationSpec {
                id: OpId::new(id)?,
                title: (*title).to_owned(),
                summary: (*summary).to_owned(),
                params: params(*shape),
                risk: *risk,
                target: TargetKind::Tenant,
            })
        })
        .collect()
}

/// The `requires` list of an operation (`signature`, `approval:migration`), empty when unknown.
#[must_use]
pub fn requires(id: &str) -> &'static [&'static str] {
    ROWS.iter().find(|row| row.0 == id).map_or(&[], |row| row.5)
}

/// The module that registers the entries on the catalogue builder.
#[derive(Clone, Copy, Debug, Default)]
pub struct MigrationOpsModule;

impl OperationModule for MigrationOpsModule {
    fn name(&self) -> &str {
        MIGRATION_OPS_MODULE
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for spec in specs() {
            builder.add(MIGRATION_OPS_MODULE, spec);
        }
    }
}

/// A registry with a fail-closed stub for every operation.
#[must_use]
pub fn stub_registry() -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    for spec in specs() {
        let stub = Arc::new(NotImplemented::new(&spec.id));
        registry.register(spec.id, stub);
    }
    registry
}

/// The handlers for `ctx`: every operation starts as a fail-closed stub and each verb module
/// replaces the stubs of the operations it implements.
#[must_use]
pub fn handlers(ctx: &Arc<LifecycleContext>) -> HandlerRegistry {
    let mut registry = stub_registry();
    crate::plan::register(ctx, &mut registry);
    crate::approve::register(ctx, &mut registry);
    crate::rehearse::register(ctx, &mut registry);
    crate::apply::register(ctx, &mut registry);
    crate::hold::register(ctx, &mut registry);
    crate::rescue::register(ctx, &mut registry);
    crate::audit::register(ctx, &mut registry);
    registry
}
