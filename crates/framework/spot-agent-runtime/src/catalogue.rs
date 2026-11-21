//! The catalogue: the `db.*`, `node.*` and `runtime.*` operations this crate registers.
//!
//! Every operation is data — an id, typed parameters, a risk level and a server target — from
//! which the console builds its screen and the CLI its flags. The parameter choices the console
//! offers are derived from the same tables the modules enforce: the PostgreSQL allow-list, the
//! role set, the backup kinds and the profiles. The handlers in [`crate::handler`] implement
//! exactly these ids and nothing else.

use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, CatalogueError, OpId, OperationModule, OperationSpec, ParamSpec,
    ParamType, ParamValue, Risk, TargetKind,
};

use crate::backup::BackupKind;
use crate::ids;
use crate::params::PG_PARAMETERS;
use crate::profiles::Profile;
use crate::roles::NodeRole;

/// The module's name, used in catalogue errors.
pub const MODULE_NAME: &str = "spot-agent-runtime";

/// A required text parameter.
fn text_required(name: &str, max_len: usize, pattern: Option<&str>, doc: &str) -> ParamSpec {
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

/// An optional text parameter.
fn text_optional(name: &str, max_len: usize, pattern: Option<&str>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: pattern.map(str::to_owned),
        },
        required: false,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// An optional boolean parameter with a default.
fn boolean(name: &str, default: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Bool,
        required: false,
        default: Some(ParamValue::Bool(default)),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// An optional bounded integer parameter.
fn integer(name: &str, min: i64, max: i64, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Int { min, max },
        required: false,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A required enum parameter with no default.
fn enum_required(name: &str, choices: Vec<String>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Enum(choices),
        required: true,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A required list-of-enum parameter.
fn list_enum_required(name: &str, choices: Vec<String>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::List(Box::new(ParamType::Enum(choices))),
        required: true,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// The allow-listed PostgreSQL parameter names, in name order.
#[must_use]
pub fn parameter_choices() -> Vec<String> {
    PG_PARAMETERS
        .iter()
        .map(|parameter| parameter.name.to_owned())
        .collect()
}

/// The role names, in role order.
#[must_use]
pub fn role_choices() -> Vec<String> {
    NodeRole::ALL
        .iter()
        .map(|role| role.name().to_owned())
        .collect()
}

/// The profile names, in profile order.
#[must_use]
pub fn profile_choices() -> Vec<String> {
    Profile::ALL
        .iter()
        .map(|profile| profile.name().to_owned())
        .collect()
}

/// The backup kind names.
#[must_use]
pub fn backup_choices() -> Vec<String> {
    [BackupKind::BaseBackup, BackupKind::LogicalDump]
        .iter()
        .map(|kind| kind.name().to_owned())
        .collect()
}

/// The accepted version spellings.
const VERSION_PATTERN: &str = "[0-9A-Za-z][0-9A-Za-z._+-]{0,63}";
/// The accepted backup label spelling.
const LABEL_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9._-]{0,63}";
/// The accepted absolute data-directory spelling.
const DATA_DIR_PATTERN: &str = "/[A-Za-z0-9._/-]{0,255}";
/// The accepted host spelling.
const HOST_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9.:-]{0,253}";

/// Assembles one operation spec targeting a single server.
fn spec(id: &str, title: &str, summary: &str, risk: Risk, params: Vec<ParamSpec>) -> OperationSpec {
    OperationSpec {
        id: op_id(id),
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Server,
    }
}

/// The operation id, unchecked: the constants are proven valid by [`crate::ids`]'s tests.
fn op_id(text: &str) -> OpId {
    match ids::op_id(text) {
        Ok(id) => id,
        // Unreachable: every constant is validated by `ids::tests::every_id_is_valid`.
        Err(error) => panic!("invalid operation id {text}: {error}"),
    }
}

/// Every operation this crate registers, in a stable order.
#[must_use]
pub fn specs() -> Vec<OperationSpec> {
    vec![
        spec(
            ids::DB_PARAM_GET,
            "Read a PostgreSQL parameter",
            "Read one allow-listed PostgreSQL parameter and whether a restart is pending.",
            Risk::Read,
            vec![enum_required(
                "name",
                parameter_choices(),
                "The allow-listed parameter to read.",
            )],
        ),
        spec(
            ids::DB_PARAM_SET,
            "Set a PostgreSQL parameter",
            "Set one allow-listed PostgreSQL parameter with ALTER SYSTEM, then reload or restart.",
            Risk::High,
            vec![
                enum_required(
                    "name",
                    parameter_choices(),
                    "The allow-listed parameter to set.",
                ),
                text_required(
                    "value",
                    256,
                    None,
                    "The value, in the parameter's unit (for example 64MB or on).",
                ),
            ],
        ),
        spec(
            ids::DB_REPLICA_STATUS,
            "Replica status",
            "Read this node's replication role and streaming state.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::DB_REPLICA_LAG,
            "Replica lag",
            "Read how far this standby is behind its primary.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::DB_REPLICA_CREATE,
            "Create a standby",
            "Build a standby from the primary with pg_basebackup.",
            Risk::High,
            vec![
                text_required("host", 255, Some(HOST_PATTERN), "The primary's host."),
                integer("port", 1, 65535, "The primary's port."),
                text_required(
                    "data_dir",
                    256,
                    Some(DATA_DIR_PATTERN),
                    "The standby's absolute data directory.",
                ),
                text_optional("slot", 64, None, "The replication slot to use, if any."),
            ],
        ),
        spec(
            ids::DB_REPLICA_PROMOTE,
            "Promote a standby",
            "Promote this node to primary; refused while the old primary is reachable unless the critical flag is set.",
            Risk::Critical,
            vec![boolean(
                "critical",
                false,
                "Acknowledge promoting while the old primary is still reachable.",
            )],
        ),
        spec(
            ids::DB_BACKUP_START,
            "Start a backup",
            "Start a base backup or a logical dump and record it in the snapshot manifest.",
            Risk::Medium,
            vec![
                enum_required("kind", backup_choices(), "base or logical."),
                text_required(
                    "label",
                    64,
                    Some(LABEL_PATTERN),
                    "A short label the operator recognises.",
                ),
            ],
        ),
        spec(
            ids::DB_BACKUP_LIST,
            "List backups",
            "List the backups this node holds, newest first.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::DB_BACKUP_VERIFY,
            "Verify a backup",
            "Check a backup's manifest and file checksums.",
            Risk::Low,
            vec![text_required("id", 128, None, "The backup's snapshot id.")],
        ),
        spec(
            ids::NODE_ROLE_GET,
            "Read node roles",
            "Read the Spotledger roles this node serves.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::NODE_ROLE_SET,
            "Set node roles",
            "Replace the node's role set as a settings diff and report the restart plan.",
            Risk::High,
            vec![list_enum_required(
                "roles",
                role_choices(),
                "The full role set (web, jobs, realtime, database, branch-edge).",
            )],
        ),
        spec(
            ids::RUNTIME_PROFILE_APPLY,
            "Apply a runtime profile",
            "Apply a named settings bundle (standalone, cluster-basic, cluster-redis).",
            Risk::High,
            vec![enum_required(
                "profile",
                profile_choices(),
                "The profile to apply.",
            )],
        ),
        spec(
            ids::RUNTIME_UPGRADE_START,
            "Start an upgrade",
            "Plan an upgrade, submit it for approval and apply it once approved.",
            Risk::Critical,
            vec![text_required(
                "version",
                64,
                Some(VERSION_PATTERN),
                "The target version.",
            )],
        ),
        spec(
            ids::RUNTIME_UPGRADE_RESUME,
            "Resume an upgrade",
            "Recover an interrupted upgrade from the apply engine's journal.",
            Risk::Critical,
            vec![text_required(
                "version",
                64,
                Some(VERSION_PATTERN),
                "The target version of the interrupted upgrade.",
            )],
        ),
        spec(
            ids::RUNTIME_SERVICE_STOP,
            "Stop the runtime",
            "Stop the runtime service, draining in-flight work for the given window.",
            Risk::High,
            vec![ParamSpec {
                name: "drain_secs".to_owned(),
                ty: ParamType::Duration,
                required: false,
                default: Some(ParamValue::Duration(30)),
                doc: "How long to let in-flight work finish before stopping.".to_owned(),
                secret: false,
            }],
        ),
        spec(
            ids::RUNTIME_SERVICE_START,
            "Start the runtime",
            "Start the runtime service.",
            Risk::Medium,
            Vec::new(),
        ),
    ]
}

/// The module that registers this crate's operations on the catalogue.
#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeCatalogue;

impl OperationModule for RuntimeCatalogue {
    fn name(&self) -> &str {
        MODULE_NAME
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for spec in specs() {
            builder.add(MODULE_NAME, spec);
        }
    }
}

/// Builds this crate's catalogue alone (the host merges it with the other modules').
///
/// # Errors
/// Every problem the specs have; there are none, which the tests prove.
pub fn build_catalogue() -> Result<Catalogue, Vec<CatalogueError>> {
    let mut builder = CatalogueBuilder::new();
    RuntimeCatalogue.register(&mut builder);
    builder.build()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_catalogue_builds_clean() {
        let catalogue = build_catalogue().unwrap();
        assert_eq!(catalogue.len(), 16);
    }

    #[test]
    fn every_operation_id_is_registered_once() {
        let catalogue = build_catalogue().unwrap();
        for expected in [
            ids::DB_PARAM_GET,
            ids::DB_PARAM_SET,
            ids::DB_REPLICA_STATUS,
            ids::DB_REPLICA_LAG,
            ids::DB_REPLICA_CREATE,
            ids::DB_REPLICA_PROMOTE,
            ids::DB_BACKUP_START,
            ids::DB_BACKUP_LIST,
            ids::DB_BACKUP_VERIFY,
            ids::NODE_ROLE_GET,
            ids::NODE_ROLE_SET,
            ids::RUNTIME_PROFILE_APPLY,
            ids::RUNTIME_UPGRADE_START,
            ids::RUNTIME_UPGRADE_RESUME,
            ids::RUNTIME_SERVICE_STOP,
            ids::RUNTIME_SERVICE_START,
        ] {
            assert!(
                catalogue.get(&ids::op_id(expected).unwrap()).is_some(),
                "{expected} is registered"
            );
        }
    }
}
