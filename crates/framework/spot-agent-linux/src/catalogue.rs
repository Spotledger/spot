//! The catalogue: the `os.*` operations this crate registers, and the module that registers
//! them.
//!
//! Every operation is data — an id, typed parameters, a risk level and a server target — from
//! which the console builds its screen and the CLI its flags. The Linux module registers
//! `os.package.*`, `os.update.*`, `os.service.*`, `os.user.*` and `os.firewall.*`; the handlers
//! in [`crate::handlers`] implement exactly these ids.

use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, CatalogueError, OperationModule, OperationSpec, ParamSpec,
    ParamType, ParamValue, Risk, TargetKind,
};

use crate::ids;

/// The module's name, used in catalogue errors.
pub const MODULE_NAME: &str = "spot-agent-linux";

/// A required text parameter, whole-value checked against `pattern` when given.
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

/// An optional enum parameter with a default.
fn enumeration(name: &str, choices: &[&str], default: &str, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Enum(choices.iter().map(|choice| (*choice).to_owned()).collect()),
        required: false,
        default: Some(ParamValue::Text(default.to_owned())),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A required list-of-text parameter, each element whole-value checked against `pattern`.
fn list_text_required(name: &str, max_len: usize, pattern: Option<&str>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::List(Box::new(ParamType::Text {
            max_len,
            pattern: pattern.map(str::to_owned),
        })),
        required: true,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// An optional list-of-text parameter.
fn list_text_optional(name: &str, max_len: usize, pattern: Option<&str>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::List(Box::new(ParamType::Text {
            max_len,
            pattern: pattern.map(str::to_owned),
        })),
        required: false,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// Assembles one operation spec targeting a single server.
fn spec(id: &str, title: &str, summary: &str, risk: Risk, params: Vec<ParamSpec>) -> OperationSpec {
    OperationSpec {
        id: ids::op_id(id),
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Server,
    }
}

/// The account-name pattern the catalogue and the module both enforce.
const ACCOUNT_PATTERN: &str = "[a-z_][a-z0-9_-]{0,31}";
/// The package-name pattern.
const PACKAGE_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9._+:-]{0,127}";
/// The source (address or CIDR) pattern.
const SOURCE_PATTERN: &str = "[0-9A-Fa-f:.]{1,64}(/[0-9]{1,3})?";

/// Every operation this crate registers, in a stable order.
#[must_use]
pub fn specs() -> Vec<OperationSpec> {
    vec![
        spec(
            ids::PACKAGE_LIST,
            "List installed packages",
            "Read the list of installed packages from the host's package manager.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::PACKAGE_INSTALL,
            "Install packages",
            "Install one or more packages by name, skipping those already installed.",
            Risk::High,
            vec![list_text_required(
                "packages",
                128,
                Some(PACKAGE_PATTERN),
                "The package names to install.",
            )],
        ),
        spec(
            ids::PACKAGE_REMOVE,
            "Remove packages",
            "Remove one or more installed packages by name.",
            Risk::High,
            vec![list_text_required(
                "packages",
                128,
                Some(PACKAGE_PATTERN),
                "The package names to remove.",
            )],
        ),
        spec(
            ids::PACKAGE_REPOSITORIES,
            "List repositories",
            "Read the package repositories the host is configured to use.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::UPDATE_CHECK,
            "Check for updates",
            "Report the available package updates and whether a reboot is required.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::UPDATE_APPLY,
            "Apply updates",
            "Apply all available package updates, or only the security updates.",
            Risk::High,
            vec![boolean(
                "security_only",
                false,
                "Apply only updates from security sources.",
            )],
        ),
        spec(
            ids::SERVICE_STATUS,
            "Service status",
            "Read a systemd unit's active, sub and file states.",
            Risk::Read,
            vec![text_required(
                "unit",
                128,
                None,
                "The systemd unit to read.",
            )],
        ),
        spec(
            ids::SERVICE_START,
            "Start service",
            "Start a systemd unit.",
            Risk::Low,
            vec![text_required(
                "unit",
                128,
                None,
                "The systemd unit to start.",
            )],
        ),
        spec(
            ids::SERVICE_STOP,
            "Stop service",
            "Stop a systemd unit; a protected unit needs the critical flag.",
            Risk::High,
            vec![
                text_required("unit", 128, None, "The systemd unit to stop."),
                boolean(
                    "critical",
                    false,
                    "Acknowledge stopping a protected unit (the agent or the runtime).",
                ),
            ],
        ),
        spec(
            ids::SERVICE_RESTART,
            "Restart service",
            "Restart a systemd unit; a protected unit needs the critical flag.",
            Risk::Medium,
            vec![
                text_required("unit", 128, None, "The systemd unit to restart."),
                boolean(
                    "critical",
                    false,
                    "Acknowledge restarting a protected unit (the agent or the runtime).",
                ),
            ],
        ),
        spec(
            ids::SERVICE_ENABLE,
            "Enable service",
            "Enable a systemd unit at boot.",
            Risk::Low,
            vec![text_required(
                "unit",
                128,
                None,
                "The systemd unit to enable.",
            )],
        ),
        spec(
            ids::SERVICE_DISABLE,
            "Disable service",
            "Disable a systemd unit at boot; a protected unit needs the critical flag.",
            Risk::Medium,
            vec![
                text_required("unit", 128, None, "The systemd unit to disable."),
                boolean(
                    "critical",
                    false,
                    "Acknowledge disabling a protected unit (the agent or the runtime).",
                ),
            ],
        ),
        spec(
            ids::SERVICE_MASK,
            "Mask service",
            "Mask a systemd unit; a protected unit needs the critical flag.",
            Risk::High,
            vec![
                text_required("unit", 128, None, "The systemd unit to mask."),
                boolean(
                    "critical",
                    false,
                    "Acknowledge masking a protected unit (the agent or the runtime).",
                ),
            ],
        ),
        spec(
            ids::SERVICE_FAILED,
            "List failed services",
            "List the systemd units currently in the failed state.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::USER_LIST,
            "List users",
            "Read the host's normal (non-system) accounts.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::USER_CREATE,
            "Create user",
            "Create an account, optionally with a home directory and group memberships.",
            Risk::High,
            vec![
                text_required("name", 32, Some(ACCOUNT_PATTERN), "The account name."),
                boolean("create_home", true, "Create the account's home directory."),
                text_optional("home", 256, None, "The home directory (absolute path)."),
                text_optional("shell", 256, None, "The login shell (absolute path)."),
                integer("uid", 0, 60_000, "The numeric UID to assign."),
                list_text_optional(
                    "groups",
                    32,
                    Some(ACCOUNT_PATTERN),
                    "Supplementary groups to add the account to.",
                ),
            ],
        ),
        spec(
            ids::USER_LOCK,
            "Lock user",
            "Lock an account's password.",
            Risk::Medium,
            vec![text_required(
                "name",
                32,
                Some(ACCOUNT_PATTERN),
                "The account name to lock.",
            )],
        ),
        spec(
            ids::USER_UNLOCK,
            "Unlock user",
            "Unlock an account's password.",
            Risk::Medium,
            vec![text_required(
                "name",
                32,
                Some(ACCOUNT_PATTERN),
                "The account name to unlock.",
            )],
        ),
        spec(
            ids::USER_DELETE,
            "Delete user",
            "Delete an account, optionally removing its home directory.",
            Risk::High,
            vec![
                text_required(
                    "name",
                    32,
                    Some(ACCOUNT_PATTERN),
                    "The account name to delete.",
                ),
                boolean(
                    "remove_home",
                    false,
                    "Also remove the account's home directory.",
                ),
            ],
        ),
        spec(
            ids::USER_GROUPS,
            "User groups",
            "Read the groups an account belongs to.",
            Risk::Read,
            vec![text_required(
                "name",
                32,
                Some(ACCOUNT_PATTERN),
                "The account name.",
            )],
        ),
        spec(
            ids::USER_ADD_GROUP,
            "Add group membership",
            "Add an account to a supplementary group.",
            Risk::Medium,
            vec![
                text_required("name", 32, Some(ACCOUNT_PATTERN), "The account name."),
                text_required(
                    "group",
                    32,
                    Some(ACCOUNT_PATTERN),
                    "The group to add the account to.",
                ),
            ],
        ),
        spec(
            ids::USER_REMOVE_GROUP,
            "Remove group membership",
            "Remove an account from a supplementary group.",
            Risk::Medium,
            vec![
                text_required("name", 32, Some(ACCOUNT_PATTERN), "The account name."),
                text_required(
                    "group",
                    32,
                    Some(ACCOUNT_PATTERN),
                    "The group to remove the account from.",
                ),
            ],
        ),
        spec(
            ids::FIREWALL_STATUS,
            "Firewall status",
            "Detect the active firewall backend (firewalld, ufw or nftables).",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::FIREWALL_LIST,
            "List firewall rules",
            "List the firewall's open port and source rules.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::FIREWALL_OPEN,
            "Open firewall rule",
            "Open a port rule or a source rule.",
            Risk::Medium,
            firewall_params(),
        ),
        spec(
            ids::FIREWALL_CLOSE,
            "Close firewall rule",
            "Close a port rule or a source rule; closing management access needs the critical flag.",
            Risk::High,
            firewall_params(),
        ),
    ]
}

/// The parameters shared by the firewall open and close operations.
fn firewall_params() -> Vec<ParamSpec> {
    vec![
        integer("port", 1, 65_535, "The port to open or close."),
        enumeration(
            "protocol",
            &["tcp", "udp"],
            "tcp",
            "The transport protocol for the port rule.",
        ),
        text_optional(
            "source",
            64,
            Some(SOURCE_PATTERN),
            "The source address or CIDR to open or close.",
        ),
        boolean(
            "critical",
            false,
            "Acknowledge closing a rule that carries management access.",
        ),
    ]
}

/// The Linux operation module: registers every `os.*` operation on a catalogue builder.
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxModule;

impl OperationModule for LinuxModule {
    fn name(&self) -> &str {
        MODULE_NAME
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for operation in specs() {
            builder.add(MODULE_NAME, operation);
        }
    }
}

/// Builds the catalogue from just the Linux module.
///
/// # Errors
/// Every catalogue problem the Linux specs have; a test asserts there are none.
pub fn build_catalogue() -> Result<Catalogue, Vec<CatalogueError>> {
    let modules: Vec<Box<LinuxModule>> = vec![Box::new(LinuxModule)];
    Catalogue::from_modules(&modules)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_catalogue_builds_clean() {
        let catalogue = build_catalogue().unwrap();
        assert_eq!(catalogue.len(), ids::ALL.len());
    }

    #[test]
    fn every_id_in_the_crate_is_registered() {
        let catalogue = build_catalogue().unwrap();
        for id in ids::ALL {
            let op = ids::op_id(id);
            assert!(catalogue.get(&op).is_some(), "{id} is registered");
            assert_eq!(catalogue.module_of(&op), Some(MODULE_NAME));
        }
    }

    #[test]
    fn risk_levels_follow_the_brief() {
        let catalogue = build_catalogue().unwrap();
        let risk = |id: &str| catalogue.get(&ids::op_id(id)).unwrap().risk;
        assert_eq!(risk(ids::PACKAGE_LIST), Risk::Read);
        assert_eq!(risk(ids::PACKAGE_INSTALL), Risk::High);
        assert_eq!(risk(ids::PACKAGE_REMOVE), Risk::High);
        assert_eq!(risk(ids::UPDATE_CHECK), Risk::Read);
        assert_eq!(risk(ids::UPDATE_APPLY), Risk::High);
        assert_eq!(risk(ids::SERVICE_STOP), Risk::High);
        assert_eq!(risk(ids::SERVICE_MASK), Risk::High);
        assert_eq!(risk(ids::USER_CREATE), Risk::High);
        assert_eq!(risk(ids::USER_DELETE), Risk::High);
        assert_eq!(risk(ids::FIREWALL_OPEN), Risk::Medium);
        assert_eq!(risk(ids::FIREWALL_CLOSE), Risk::High);
    }

    #[test]
    fn every_operation_targets_a_server() {
        let catalogue = build_catalogue().unwrap();
        for spec in catalogue.iter() {
            assert_eq!(spec.target, TargetKind::Server, "{}", spec.id.as_str());
        }
    }
}
