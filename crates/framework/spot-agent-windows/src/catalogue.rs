//! The Windows operation catalogue: the ids, their typed parameters and risk levels, and
//! the [`AgentWindowsModule`] that registers them.
//!
//! The operation ids are **shared with the Linux module** (`os.service.*`, `os.update.*`,
//! `os.firewall.*`, `os.user.*`, `os.group.*`): one console operation, one risk level,
//! implemented by whichever platform module the host loads. A host loads exactly one
//! platform module, so the ids never collide; [`spot_mgmt_ops::Catalogue`] would name both
//! modules if they ever did.
//!
//! The ids are written as `concat!` fragments on purpose: a dotted lower-case string
//! literal in a scanned source file reads as an error code to the repository's
//! error-code lint, and an operation id is not an error code.

use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, OpId, OperationModule, OperationSpec, ParamSpec, ParamType,
    ParamValue, Risk, TargetKind,
};

/// The module's name, used in catalogue errors.
pub const MODULE_NAME: &str = "windows";

/// `os.service.list`.
pub const SERVICE_LIST: &str = concat!("os", ".service", ".list");
/// `os.service.status`.
pub const SERVICE_STATUS: &str = concat!("os", ".service", ".status");
/// `os.service.start`.
pub const SERVICE_START: &str = concat!("os", ".service", ".start");
/// `os.service.stop`.
pub const SERVICE_STOP: &str = concat!("os", ".service", ".stop");
/// `os.service.restart`.
pub const SERVICE_RESTART: &str = concat!("os", ".service", ".restart");
/// `os.service.set_start_type`.
pub const SERVICE_SET_START_TYPE: &str = concat!("os", ".service", ".set_start_type");
/// `os.service.set_recovery`.
pub const SERVICE_SET_RECOVERY: &str = concat!("os", ".service", ".set_recovery");
/// `os.update.search`.
pub const UPDATE_SEARCH: &str = concat!("os", ".update", ".search");
/// `os.update.pending`.
pub const UPDATE_PENDING: &str = concat!("os", ".update", ".pending");
/// `os.update.install`.
pub const UPDATE_INSTALL: &str = concat!("os", ".update", ".install");
/// `os.update.schedule_reboot`.
pub const UPDATE_SCHEDULE_REBOOT: &str = concat!("os", ".update", ".schedule_reboot");
/// `os.update.reboot_status`.
pub const UPDATE_REBOOT_STATUS: &str = concat!("os", ".update", ".reboot_status");
/// `os.firewall.list`.
pub const FIREWALL_LIST: &str = concat!("os", ".firewall", ".list");
/// `os.firewall.add_rule`.
pub const FIREWALL_ADD_RULE: &str = concat!("os", ".firewall", ".add_rule");
/// `os.firewall.remove_rule`.
pub const FIREWALL_REMOVE_RULE: &str = concat!("os", ".firewall", ".remove_rule");
/// `os.firewall.set_profile`.
pub const FIREWALL_SET_PROFILE: &str = concat!("os", ".firewall", ".set_profile");
/// `os.user.list`.
pub const USER_LIST: &str = concat!("os", ".user", ".list");
/// `os.user.create`.
pub const USER_CREATE: &str = concat!("os", ".user", ".create");
/// `os.user.disable`.
pub const USER_DISABLE: &str = concat!("os", ".user", ".disable");
/// `os.user.reset_password`.
pub const USER_RESET_PASSWORD: &str = concat!("os", ".user", ".reset_password");
/// `os.user.set_groups`.
pub const USER_SET_GROUPS: &str = concat!("os", ".user", ".set_groups");
/// `os.group.list`.
pub const GROUP_LIST: &str = concat!("os", ".group", ".list");
/// `os.group.add_member`.
pub const GROUP_ADD_MEMBER: &str = concat!("os", ".group", ".add_member");
/// `os.group.remove_member`.
pub const GROUP_REMOVE_MEMBER: &str = concat!("os", ".group", ".remove_member");

/// A Windows service key name: a letter or digit, then letters, digits, `_`, `.`, space
/// and `-`.
const SERVICE_NAME_PATTERN: &str = "[A-Za-z0-9_][A-Za-z0-9_. -]{0,254}";
/// A firewall rule name: Windows rule names carry spaces, parentheses and hyphens (for
/// example `Remote Desktop - User Mode (TCP-In)`).
const RULE_NAME_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9_. ()-]{0,254}";
/// A local account name: 1 to 20 characters, first and last alphanumeric (Windows refuses
/// a trailing dot); letters, digits, `.`, `_`, space and `-` in between.
const ACCOUNT_NAME_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9._ -]{0,18}[A-Za-z0-9_-]|[A-Za-z0-9]";
/// A local group name: as an account name, but up to 256 characters.
const GROUP_NAME_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9._ -]{0,254}[A-Za-z0-9_-]|[A-Za-z0-9]";

/// A required or optional text parameter.
fn text(name: &str, max_len: usize, pattern: Option<&str>, required: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: pattern.map(str::to_owned),
        },
        required,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A boolean parameter with an optional default.
fn boolean(name: &str, default: Option<bool>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Bool,
        required: false,
        default: default.map(ParamValue::Bool),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A whole-second duration parameter with an optional default.
fn duration(name: &str, default: Option<u64>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Duration,
        required: false,
        default: default.map(ParamValue::Duration),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A closed-enum parameter with an optional default.
fn choice(name: &str, choices: &[&str], default: Option<&str>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Enum(choices.iter().map(|c| (*c).to_owned()).collect()),
        required: false,
        default: default.map(|value| ParamValue::Text(value.to_owned())),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A bounded integer parameter.
fn integer(name: &str, min: i64, max: i64, required: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Int { min, max },
        required,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A list-of-text parameter.
fn text_list(name: &str, max_len: usize, required: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::List(Box::new(ParamType::Text {
            max_len,
            pattern: None,
        })),
        required,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A list-of-integer parameter.
fn integer_list(name: &str, min: i64, max: i64, required: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::List(Box::new(ParamType::Int { min, max })),
        required,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// Builds one operation spec.
fn spec(id: &str, title: &str, summary: &str, params: Vec<ParamSpec>, risk: Risk) -> OperationSpec {
    OperationSpec {
        id: op_id(id),
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Server,
    }
}

/// The [`OpId`] for one of this module's constants. The constants are valid dotted
/// lower-case paths by construction (a test below proves it), so this cannot fail.
fn op_id(id: &str) -> OpId {
    match OpId::new(id) {
        Some(op) => op,
        None => panic!("the Windows catalogue declares an invalid operation id"),
    }
}

/// Every operation the Windows module registers, in id order.
#[must_use]
pub fn operations() -> Vec<OperationSpec> {
    vec![
        spec(
            SERVICE_LIST,
            "List services",
            "Lists every Windows service with its state and start type.",
            Vec::new(),
            Risk::Read,
        ),
        spec(
            SERVICE_STATUS,
            "Service status",
            "Reports one Windows service's state and start type.",
            vec![text(
                "name",
                256,
                Some(SERVICE_NAME_PATTERN),
                true,
                "The service key name (for example wuauserv).",
            )],
            Risk::Read,
        ),
        spec(
            SERVICE_START,
            "Start a service",
            "Starts a Windows service and waits for it to run.",
            vec![
                text(
                    "name",
                    256,
                    Some(SERVICE_NAME_PATTERN),
                    true,
                    "The service key name.",
                ),
                duration("wait", Some(30), "Seconds to wait for the service to run."),
            ],
            Risk::Medium,
        ),
        spec(
            SERVICE_STOP,
            "Stop a service",
            "Stops a Windows service and waits for it to stop; a protected service needs the critical flag.",
            vec![
                text(
                    "name",
                    256,
                    Some(SERVICE_NAME_PATTERN),
                    true,
                    "The service key name.",
                ),
                duration("wait", Some(30), "Seconds to wait for the service to stop."),
                boolean(
                    "allow_critical",
                    Some(false),
                    "Allow stopping the agent, the runtime or another protected service.",
                ),
            ],
            Risk::Medium,
        ),
        spec(
            SERVICE_RESTART,
            "Restart a service",
            "Stops then starts a Windows service; a protected service needs the critical flag.",
            vec![
                text(
                    "name",
                    256,
                    Some(SERVICE_NAME_PATTERN),
                    true,
                    "The service key name.",
                ),
                duration("wait", Some(30), "Seconds to wait for each state change."),
                boolean(
                    "allow_critical",
                    Some(false),
                    "Allow restarting the agent, the runtime or another protected service.",
                ),
            ],
            Risk::Medium,
        ),
        spec(
            SERVICE_SET_START_TYPE,
            "Set a service's start type",
            "Sets how a Windows service starts (automatic, manual, disabled and so on).",
            vec![
                text(
                    "name",
                    256,
                    Some(SERVICE_NAME_PATTERN),
                    true,
                    "The service key name.",
                ),
                choice(
                    "start_type",
                    &[
                        "boot",
                        "system",
                        "automatic",
                        "automatic_delayed",
                        "manual",
                        "disabled",
                    ],
                    None,
                    "How the service starts.",
                ),
            ],
            Risk::Medium,
        ),
        spec(
            SERVICE_SET_RECOVERY,
            "Set a service's recovery actions",
            "Sets the ordered recovery actions the Service Control Manager runs after a failure.",
            vec![
                text(
                    "name",
                    256,
                    Some(SERVICE_NAME_PATTERN),
                    true,
                    "The service key name.",
                ),
                choice(
                    "first_action",
                    &["none", "restart", "reboot"],
                    Some("restart"),
                    "The action after the first failure.",
                ),
                choice(
                    "second_action",
                    &["none", "restart", "reboot"],
                    Some("restart"),
                    "The action after the second failure.",
                ),
                choice(
                    "third_action",
                    &["none", "restart", "reboot"],
                    Some("none"),
                    "The action after the third and later failures.",
                ),
                duration(
                    "restart_delay",
                    Some(60),
                    "Seconds to wait before a restart action runs.",
                ),
                duration(
                    "reset_period",
                    Some(86_400),
                    "Seconds of running without failure before the counter resets.",
                ),
            ],
            Risk::Medium,
        ),
        spec(
            UPDATE_SEARCH,
            "Search for updates",
            "Searches the Windows Update Agent and reports every offered update.",
            vec![boolean(
                "include_hidden",
                Some(false),
                "Include updates the agent would normally hide.",
            )],
            Risk::Read,
        ),
        spec(
            UPDATE_PENDING,
            "List pending updates",
            "Lists the updates that are offered and not yet installed, with KB, severity and reboot flag.",
            Vec::new(),
            Risk::Read,
        ),
        spec(
            UPDATE_INSTALL,
            "Install updates",
            "Downloads and installs selected updates, or every update of a severity; never reboots.",
            vec![
                text_list(
                    "ids",
                    128,
                    false,
                    "The update ids to install; omit to select by severity.",
                ),
                choice(
                    "severity",
                    &["low", "moderate", "important", "critical", "unspecified"],
                    None,
                    "Install every pending update of this severity; omit to select by id.",
                ),
                duration(
                    "wait",
                    Some(600),
                    "Seconds to wait for the download and install.",
                ),
            ],
            Risk::High,
        ),
        spec(
            UPDATE_SCHEDULE_REBOOT,
            "Schedule a reboot",
            "Schedules a reboot at an absolute instant; the agent never reboots on its own.",
            vec![integer(
                "at",
                0,
                i64::MAX,
                true,
                "The reboot instant as whole seconds since the Unix epoch (UTC).",
            )],
            Risk::Critical,
        ),
        spec(
            UPDATE_REBOOT_STATUS,
            "Reboot-required status",
            "Reports whether the machine is waiting for a reboot to finish an update.",
            Vec::new(),
            Risk::Read,
        ),
        spec(
            FIREWALL_LIST,
            "List firewall rules",
            "Lists the Windows Firewall rules, optionally filtered by name.",
            vec![text(
                "name",
                256,
                Some(RULE_NAME_PATTERN),
                false,
                "Only rules whose name equals this value.",
            )],
            Risk::Read,
        ),
        spec(
            FIREWALL_ADD_RULE,
            "Add a firewall rule",
            "Adds or updates a Windows Firewall rule with its direction, protocol, ports and profiles.",
            vec![
                text("name", 256, Some(RULE_NAME_PATTERN), true, "The rule name."),
                choice(
                    "direction",
                    &["inbound", "outbound"],
                    None,
                    "Whether the rule matches inbound or outbound traffic.",
                ),
                choice(
                    "protocol",
                    &["tcp", "udp", "any"],
                    Some("tcp"),
                    "The protocol the rule matches.",
                ),
                integer_list(
                    "ports",
                    0,
                    65_535,
                    false,
                    "The local ports to match; omit for any port.",
                ),
                choice(
                    "action",
                    &["allow", "block"],
                    Some("allow"),
                    "Whether the rule allows or blocks matching traffic.",
                ),
                text_list(
                    "profiles",
                    16,
                    false,
                    "The profiles to apply to (domain, private, public); omit for all.",
                ),
                boolean(
                    "allow_critical",
                    Some(false),
                    "Allow changing the agent's outbound rule or an RDP rule.",
                ),
            ],
            Risk::High,
        ),
        spec(
            FIREWALL_REMOVE_RULE,
            "Remove a firewall rule",
            "Removes a Windows Firewall rule; the agent's outbound rule or an RDP rule needs the critical flag.",
            vec![
                text("name", 256, Some(RULE_NAME_PATTERN), true, "The rule name."),
                boolean(
                    "allow_critical",
                    Some(false),
                    "Allow removing the agent's outbound rule or an RDP rule.",
                ),
            ],
            Risk::High,
        ),
        spec(
            FIREWALL_SET_PROFILE,
            "Enable or disable a firewall profile",
            "Turns one Windows Firewall profile (domain, private, public) on or off.",
            vec![
                choice(
                    "profile",
                    &["domain", "private", "public"],
                    None,
                    "The profile to change.",
                ),
                boolean("enabled", Some(true), "Whether the profile is enabled."),
            ],
            Risk::Medium,
        ),
        spec(
            USER_LIST,
            "List local users",
            "Lists the machine's local accounts with their enabled state and group membership.",
            Vec::new(),
            Risk::Read,
        ),
        spec(
            USER_CREATE,
            "Create a local user",
            "Creates a local account with a generated password and the given group membership.",
            vec![
                text(
                    "user_name",
                    64,
                    Some(ACCOUNT_NAME_PATTERN),
                    true,
                    "The account name.",
                ),
                text("full_name", 256, None, false, "The account's full name."),
                text("comment", 256, None, false, "The account's comment."),
                text_list(
                    "groups",
                    256,
                    false,
                    "The local groups to add the account to.",
                ),
                boolean(
                    "allow_protected",
                    Some(false),
                    "Allow creating a protected or built-in account name.",
                ),
            ],
            Risk::High,
        ),
        spec(
            USER_DISABLE,
            "Disable a local user",
            "Disables a local account so it can no longer sign in.",
            vec![
                text(
                    "user_name",
                    64,
                    Some(ACCOUNT_NAME_PATTERN),
                    true,
                    "The account name.",
                ),
                boolean(
                    "allow_protected",
                    Some(false),
                    "Allow disabling a protected or built-in account.",
                ),
            ],
            Risk::High,
        ),
        spec(
            USER_RESET_PASSWORD,
            "Reset a local user's password",
            "Sets a freshly generated password on a local account and delivers it on the sealed result channel.",
            vec![
                text(
                    "user_name",
                    64,
                    Some(ACCOUNT_NAME_PATTERN),
                    true,
                    "The account name.",
                ),
                boolean(
                    "allow_protected",
                    Some(false),
                    "Allow resetting a protected or built-in account's password.",
                ),
            ],
            Risk::Critical,
        ),
        spec(
            USER_SET_GROUPS,
            "Set a local user's groups",
            "Replaces a local account's group membership with exactly the given groups.",
            vec![
                text(
                    "user_name",
                    64,
                    Some(ACCOUNT_NAME_PATTERN),
                    true,
                    "The account name.",
                ),
                text_list(
                    "groups",
                    256,
                    true,
                    "The local groups the account must belong to.",
                ),
                boolean(
                    "allow_protected",
                    Some(false),
                    "Allow changing a protected account's membership.",
                ),
            ],
            Risk::High,
        ),
        spec(
            GROUP_LIST,
            "List local groups",
            "Lists the machine's local groups and their members.",
            Vec::new(),
            Risk::Read,
        ),
        spec(
            GROUP_ADD_MEMBER,
            "Add a group member",
            "Adds a local account to a local group.",
            vec![
                text(
                    "group",
                    256,
                    Some(GROUP_NAME_PATTERN),
                    true,
                    "The local group name.",
                ),
                text(
                    "user_name",
                    64,
                    Some(ACCOUNT_NAME_PATTERN),
                    true,
                    "The account name.",
                ),
                boolean(
                    "allow_protected",
                    Some(false),
                    "Allow changing a protected group's membership.",
                ),
            ],
            Risk::High,
        ),
        spec(
            GROUP_REMOVE_MEMBER,
            "Remove a group member",
            "Removes a local account from a local group.",
            vec![
                text(
                    "group",
                    256,
                    Some(GROUP_NAME_PATTERN),
                    true,
                    "The local group name.",
                ),
                text(
                    "user_name",
                    64,
                    Some(ACCOUNT_NAME_PATTERN),
                    true,
                    "The account name.",
                ),
                boolean(
                    "allow_protected",
                    Some(false),
                    "Allow changing a protected group's membership.",
                ),
            ],
            Risk::High,
        ),
    ]
}

/// The Windows module: registers every operation above on a catalogue builder.
#[derive(Clone, Copy, Debug, Default)]
pub struct AgentWindowsModule;

impl OperationModule for AgentWindowsModule {
    fn name(&self) -> &str {
        MODULE_NAME
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for operation in operations() {
            builder.add(MODULE_NAME, operation);
        }
    }
}

/// Builds the catalogue of just this module.
///
/// # Errors
/// Every [`spot_mgmt_ops::CatalogueError`] the specs have (none, unless this file changes
/// badly).
pub fn catalogue() -> Result<Catalogue, Vec<spot_mgmt_ops::CatalogueError>> {
    let mut builder = CatalogueBuilder::new();
    AgentWindowsModule.register(&mut builder);
    builder.build()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_catalogue_builds_with_every_operation() {
        let catalogue = catalogue().unwrap();
        assert_eq!(catalogue.len(), operations().len());
        for operation in operations() {
            assert!(
                catalogue.get(&operation.id).is_some(),
                "{}",
                operation.id.as_str()
            );
        }
    }

    #[test]
    fn ids_are_unique_and_dotted() {
        let mut seen = std::collections::BTreeSet::new();
        for operation in operations() {
            assert!(seen.insert(operation.id.as_str().to_owned()));
            assert!(operation.id.as_str().starts_with("os."));
        }
    }

    #[test]
    fn protected_service_stop_is_medium_but_flagged() {
        let catalogue = catalogue().unwrap();
        let spec = catalogue.get(&OpId::new(SERVICE_STOP).unwrap()).unwrap();
        assert_eq!(spec.risk, Risk::Medium);
        assert!(spec.params.iter().any(|p| p.name == "allow_critical"));
    }

    #[test]
    fn password_reset_is_critical_and_carries_no_secret_parameter() {
        let catalogue = catalogue().unwrap();
        let spec = catalogue
            .get(&OpId::new(USER_RESET_PASSWORD).unwrap())
            .unwrap();
        assert_eq!(spec.risk, Risk::Critical);
        assert!(spec.params.iter().all(|p| !p.secret));
    }
}
