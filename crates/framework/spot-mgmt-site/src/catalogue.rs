//! The eight non-migration site operations of the management API contract 0.2.0
//! (`contracts/management-api/openapi.yaml`, `x-spot-operations`): `site.status`,
//! `site.backup.run|list|verify`, `site.restore.preflight|apply` and `site.maintenance.enter|leave`.
//! The ten `site.migrate.*` entries come from `spot-migration-ops`. A test reads the contract file
//! and checks every id, risk, summary and parameter against this table.

use spot_mgmt_ops::{
    CatalogueBuilder, OpId, OperationModule, OperationSpec, ParamSpec, ParamType, Risk, TargetKind,
};

/// The module name used in catalogue errors.
pub const SITE_OPS_MODULE: &str = "site-ops";

/// The ids this crate serves itself, in contract order.
pub const SITE_OPERATIONS: [&str; 8] = [
    "site.backup.run",
    "site.backup.list",
    "site.backup.verify",
    "site.restore.preflight",
    "site.restore.apply",
    "site.maintenance.enter",
    "site.maintenance.leave",
    "site.status",
];

/// A registered site id (never a path).
pub const SITE_PATTERN: &str = "^[a-z][a-z0-9-]{1,62}$";
/// A configured backup target name (never a path).
pub const TARGET_PATTERN: &str = "^[a-z][a-z0-9_-]{0,99}$";
/// A backup id of this site.
pub const BACKUP_ID_PATTERN: &str = "^[A-Za-z0-9_-]{1,64}$";

#[derive(Clone, Copy)]
enum Shape {
    Site,
    SiteTarget,
    SiteBackup,
    SiteBackupTarget,
    SiteBackupTargetConfirm,
    SiteReason,
}

/// `(id, title, summary, risk, parameters, requires)`.
type Row = (
    &'static str,
    &'static str,
    &'static str,
    Risk,
    Shape,
    &'static [&'static str],
);

const ROWS: [Row; 8] = [
    (
        "site.backup.run",
        "Run a backup",
        "Run a backup of the site to a configured target.",
        Risk::Medium,
        Shape::SiteTarget,
        &["signature"],
    ),
    (
        "site.backup.list",
        "List backups",
        "List the backups of the site.",
        Risk::Read,
        Shape::Site,
        &[],
    ),
    (
        "site.backup.verify",
        "Verify a backup",
        "Verify a backup (manifest and digests) without restoring.",
        Risk::Low,
        Shape::SiteBackup,
        &["signature"],
    ),
    (
        "site.restore.preflight",
        "Check a restore",
        "Check that a restore is possible and issue the single-use confirm token.",
        Risk::Low,
        Shape::SiteBackupTarget,
        &["signature"],
    ),
    (
        "site.restore.apply",
        "Restore a backup",
        "Restore the site from a backup. A restore cannot be started by one call: it needs the confirm token of the preflight.",
        Risk::Critical,
        Shape::SiteBackupTargetConfirm,
        &["signature", "approval:restore"],
    ),
    (
        "site.maintenance.enter",
        "Enter maintenance",
        "Put the site into maintenance mode (it refuses writes).",
        Risk::Critical,
        Shape::SiteReason,
        &["signature"],
    ),
    (
        "site.maintenance.leave",
        "Leave maintenance",
        "Take the site out of maintenance mode.",
        Risk::Medium,
        Shape::Site,
        &["signature"],
    ),
    (
        "site.status",
        "Site status",
        "The site's status: version, state, last backup, any held migration.",
        Risk::Read,
        Shape::Site,
        &[],
    ),
];

fn text(name: &str, max_len: usize, pattern: Option<&str>, secret: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: pattern.map(str::to_owned),
        },
        required: true,
        default: None,
        doc: doc.to_owned(),
        secret,
    }
}

fn site() -> ParamSpec {
    text(
        "site",
        63,
        Some(SITE_PATTERN),
        false,
        "A registered site id (never a path).",
    )
}

fn target() -> ParamSpec {
    text(
        "target",
        100,
        Some(TARGET_PATTERN),
        false,
        "A configured backup target name, never a free path.",
    )
}

fn backup_id() -> ParamSpec {
    text(
        "backup_id",
        64,
        Some(BACKUP_ID_PATTERN),
        false,
        "A backup id of this site.",
    )
}

fn confirm() -> ParamSpec {
    text(
        "confirm",
        200,
        None,
        true,
        "The single-use token produced by `site.restore.preflight`, bound to the backup id, the target and the clock.",
    )
}

fn reason() -> ParamSpec {
    text("reason", 500, None, false, "Why (recorded in the audit).")
}

fn params(shape: Shape) -> Vec<ParamSpec> {
    match shape {
        Shape::Site => vec![site()],
        Shape::SiteTarget => vec![site(), target()],
        Shape::SiteBackup => vec![site(), backup_id()],
        Shape::SiteBackupTarget => vec![site(), backup_id(), target()],
        Shape::SiteBackupTargetConfirm => vec![site(), backup_id(), target(), confirm()],
        Shape::SiteReason => vec![site(), reason()],
    }
}

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

/// The `requires` list of an operation (`signature`, `approval:restore`), empty when unknown.
#[must_use]
pub fn requires(id: &str) -> &'static [&'static str] {
    ROWS.iter().find(|row| row.0 == id).map_or(&[], |row| row.5)
}

/// The module that registers the eight entries on the catalogue builder.
#[derive(Clone, Copy, Debug, Default)]
pub struct SiteOpsModule;

impl OperationModule for SiteOpsModule {
    fn name(&self) -> &str {
        SITE_OPS_MODULE
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for spec in specs() {
            builder.add(SITE_OPS_MODULE, spec);
        }
    }
}
