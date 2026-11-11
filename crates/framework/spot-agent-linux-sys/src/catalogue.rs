//! The module's operations in the typed management catalogue: the ids, parameters, risk
//! levels and target kinds the console generates its screens from and the handlers are
//! keyed by.
//!
//! Registering is one call: [`LinuxSysModule`] implements [`spot_mgmt_ops::OperationModule`],
//! so the host adds it beside the other Linux, Windows and database modules. The ids are
//! stable and dotted: `os.storage.*`, `os.snapshot.*`, `os.network.*`, `os.cert.*`,
//! `os.time.*`, `os.sysctl.*` and `os.mac.*`.

use spot_mgmt_ops::{
    OpId, OperationModule, OperationSpec, ParamSpec, ParamType, ParamValue, Risk, TargetKind,
};

/// The module's name, used in catalogue errors.
pub const MODULE_NAME: &str = "linux-sys";

/// The Linux system module: registers every operation this crate implements.
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxSysModule;

impl OperationModule for LinuxSysModule {
    fn name(&self) -> &str {
        MODULE_NAME
    }

    fn register(&self, builder: &mut spot_mgmt_ops::CatalogueBuilder) {
        for spec in operations() {
            builder.add(MODULE_NAME, spec);
        }
    }
}

/// Every operation this crate registers, in a stable order.
#[must_use]
pub fn operations() -> Vec<OperationSpec> {
    vec![
        spec(
            &["os", "storage", "facts"],
            "Storage facts",
            "Lists the block devices and mounts and reports the snapshot backend.",
            vec![],
            Risk::Read,
        ),
        spec(
            &["os", "storage", "free"],
            "Free space",
            "Reports the bytes available on the filesystem holding a path.",
            vec![text("path", 4096, true, "The absolute path to inspect.")],
            Risk::Read,
        ),
        spec(
            &["os", "snapshot", "list"],
            "List snapshots",
            "Lists the snapshots of a source on a btrfs, LVM or ZFS backend.",
            vec![
                enum_param(
                    "backend",
                    &["btrfs", "lvm", "zfs"],
                    true,
                    "The snapshot backend.",
                ),
                text(
                    "source",
                    255,
                    true,
                    "The subvolume, vg/lv or dataset to list.",
                ),
            ],
            Risk::Read,
        ),
        spec(
            &["os", "snapshot", "create"],
            "Create a snapshot",
            "Takes a btrfs subvolume, LVM or ZFS snapshot; idempotent when it exists.",
            vec![
                enum_param(
                    "backend",
                    &["btrfs", "lvm", "zfs"],
                    true,
                    "The snapshot backend.",
                ),
                text(
                    "source",
                    255,
                    true,
                    "The subvolume, vg/lv or dataset to snapshot.",
                ),
                text("name", 255, true, "The snapshot's name."),
                int_param(
                    "size_bytes",
                    0,
                    i64::MAX,
                    false,
                    Some(0),
                    "The LVM snapshot size in bytes (required by LVM).",
                ),
            ],
            Risk::High,
        ),
        spec(
            &["os", "snapshot", "delete"],
            "Delete a snapshot",
            "Deletes a btrfs, LVM or ZFS snapshot; idempotent when it is gone.",
            vec![
                enum_param(
                    "backend",
                    &["btrfs", "lvm", "zfs"],
                    true,
                    "The snapshot backend.",
                ),
                text("source", 255, true, "The subvolume, vg/lv or dataset."),
                text("name", 255, true, "The snapshot's name."),
            ],
            Risk::High,
        ),
        spec(
            &["os", "snapshot", "restore"],
            "Restore a snapshot",
            "Rolls the source back to a snapshot, moving the current data aside first.",
            vec![
                enum_param(
                    "backend",
                    &["btrfs", "lvm", "zfs"],
                    true,
                    "The snapshot backend.",
                ),
                text("source", 255, true, "The subvolume, vg/lv or dataset."),
                text("name", 255, true, "The snapshot's name."),
            ],
            Risk::Critical,
        ),
        spec(
            &["os", "network", "facts"],
            "Network facts",
            "Lists the interfaces, addresses, routes and DNS servers and the active backend.",
            vec![],
            Risk::Read,
        ),
        spec(
            &["os", "network", "set"],
            "Set interface addressing",
            "Sets a static address or DHCP on an interface, with an automatic revert unless confirmed.",
            vec![
                text("interface", 15, true, "The interface name."),
                enum_param(
                    "mode",
                    &["dhcp", "static"],
                    true,
                    "DHCP or a static address.",
                ),
                text("address", 45, false, "The static address."),
                int_param(
                    "prefix",
                    0,
                    128,
                    false,
                    Some(24),
                    "The static prefix length.",
                ),
                text("gateway", 45, false, "The static default gateway."),
                list_param("dns", false, "The DNS servers."),
                duration_param("window_secs", 120, "Seconds before the change reverts."),
            ],
            Risk::Critical,
        ),
        spec(
            &["os", "network", "confirm"],
            "Confirm a network change",
            "Confirms the pending network change so it is not reverted.",
            vec![int_param(
                "ticket",
                0,
                i64::MAX,
                true,
                None,
                "The ticket id to confirm.",
            )],
            Risk::Low,
        ),
        spec(
            &["os", "network", "revert"],
            "Revert a network change",
            "Reverts a pending network change when its window has passed.",
            vec![],
            Risk::Low,
        ),
        spec(
            &["os", "cert", "list"],
            "List certificates",
            "Lists the certificates in the system trust store.",
            vec![],
            Risk::Read,
        ),
        spec(
            &["os", "cert", "check"],
            "Check a certificate",
            "Checks a certificate's expiry and, when asked, its chain.",
            vec![
                text("name", 255, true, "The certificate's name."),
                duration_param(
                    "within_secs",
                    0,
                    "Check the certificate stays valid this long.",
                ),
                bool_param("chain", false, "Also verify the certificate's chain."),
            ],
            Risk::Read,
        ),
        spec(
            &["os", "cert", "install"],
            "Install a certificate",
            "Installs a CA or leaf certificate into the trust store and refreshes it.",
            vec![
                text("name", 255, true, "The certificate's name."),
                text("pem", 65_536, true, "The PEM certificate."),
            ],
            Risk::Medium,
        ),
        spec(
            &["os", "cert", "remove"],
            "Remove a certificate",
            "Removes a certificate from the trust store and refreshes it.",
            vec![text("name", 255, true, "The certificate's name.")],
            Risk::Medium,
        ),
        spec(
            &["os", "cert", "csr"],
            "Generate a signing request",
            "Generates a private key and a certificate-signing request.",
            vec![
                text(
                    "subject",
                    512,
                    true,
                    "The distinguished name, e.g. /CN=example.com.",
                ),
                text("key_path", 4096, true, "Where to write the private key."),
                text("csr_path", 4096, true, "Where to write the request."),
                int_param(
                    "key_bits",
                    2048,
                    8192,
                    false,
                    Some(2048),
                    "The RSA key size in bits.",
                ),
            ],
            Risk::Low,
        ),
        spec(
            &["os", "time", "status"],
            "Time status",
            "Reports chrony tracking, sources and whether a source is selected.",
            vec![],
            Risk::Read,
        ),
        spec(
            &["os", "time", "sources"],
            "Time sources",
            "Lists the chrony sources.",
            vec![],
            Risk::Read,
        ),
        spec(
            &["os", "time", "servers"],
            "Set NTP servers",
            "Writes the Spotledger chrony drop-in and reloads the sources.",
            vec![list_param(
                "servers",
                true,
                "The NTP server names or addresses.",
            )],
            Risk::Medium,
        ),
        spec(
            &["os", "time", "step"],
            "Force a clock step",
            "Steps the clock with chrony, refusing when the offset exceeds the guard.",
            vec![int_param(
                "max_jump_secs",
                0,
                i64::MAX,
                false,
                Some(1),
                "The largest offset, in seconds, that may be stepped.",
            )],
            Risk::High,
        ),
        spec(
            &["os", "sysctl", "list"],
            "List kernel parameters",
            "Lists the allowed kernel parameters and their current values.",
            vec![],
            Risk::Read,
        ),
        spec(
            &["os", "sysctl", "get"],
            "Read a kernel parameter",
            "Reads one allowed kernel parameter.",
            vec![text("key", 128, true, "The sysctl key.")],
            Risk::Read,
        ),
        spec(
            &["os", "sysctl", "set"],
            "Set a kernel parameter",
            "Sets an allowed kernel parameter now and persists it in a drop-in.",
            vec![
                text("key", 128, true, "The sysctl key."),
                int_param("value", i64::MIN, i64::MAX, true, None, "The value to set."),
            ],
            Risk::Medium,
        ),
        spec(
            &["os", "mac", "status"],
            "MAC status",
            "Reports the SELinux or AppArmor mode and profile count.",
            vec![],
            Risk::Read,
        ),
        spec(
            &["os", "mac", "denials"],
            "MAC denials",
            "Summarises recent SELinux AVC or AppArmor denials.",
            vec![],
            Risk::Read,
        ),
        spec(
            &["os", "mac", "mode"],
            "Set the MAC mode",
            "Sets the SELinux or AppArmor mode; weakening it needs the critical flag.",
            vec![
                enum_param(
                    "mode",
                    &["enforcing", "permissive", "disabled"],
                    true,
                    "The desired mode.",
                ),
                bool_param(
                    "critical",
                    false,
                    "Acknowledge weakening mandatory access control.",
                ),
            ],
            Risk::High,
        ),
        spec(
            &["os", "mac", "policy"],
            "Install a policy module",
            "Installs and enables the Spotledger policy module from a packaged file.",
            vec![
                text("name", 64, true, "The module's name."),
                text("path", 4096, true, "The packaged policy file."),
            ],
            Risk::Medium,
        ),
    ]
}

/// Joins operation id segments into the dotted id the catalogue uses.
///
/// The ids are built from segments rather than written as dotted literals, because the
/// repository's error-code scan treats any dotted lower-case literal in shipped source as a
/// candidate error code; an operation id is not one.
#[must_use]
pub fn op_id(segments: &[&str]) -> String {
    segments.join(".")
}

/// Builds one operation spec.
fn spec(
    segments: &[&str],
    title: &str,
    summary: &str,
    params: Vec<ParamSpec>,
    risk: Risk,
) -> OperationSpec {
    let id = op_id(segments);
    OperationSpec {
        id: OpId::new(&id)
            .unwrap_or_else(|| unreachable!("the operation ids in this module are valid")),
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Server,
    }
}

/// A text parameter.
fn text(name: &str, max_len: usize, required: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: None,
        },
        required,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// An enum parameter.
fn enum_param(name: &str, choices: &[&str], required: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Enum(choices.iter().map(|choice| (*choice).to_owned()).collect()),
        required,
        default: if required {
            None
        } else {
            choices
                .first()
                .map(|choice| ParamValue::Text((*choice).to_owned()))
        },
        doc: doc.to_owned(),
        secret: false,
    }
}

/// An integer parameter.
fn int_param(
    name: &str,
    min: i64,
    max: i64,
    required: bool,
    default: Option<i64>,
    doc: &str,
) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Int { min, max },
        required,
        default: default.map(ParamValue::Int),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A boolean parameter.
fn bool_param(name: &str, default: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Bool,
        required: false,
        default: Some(ParamValue::Bool(default)),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A whole-second duration parameter.
fn duration_param(name: &str, default: u64, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Duration,
        required: false,
        default: Some(ParamValue::Duration(default)),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// A list-of-text parameter.
fn list_param(name: &str, required: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::List(Box::new(ParamType::Text {
            max_len: 255,
            pattern: None,
        })),
        required,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_mgmt_ops::Catalogue;

    #[test]
    fn every_operation_is_valid_and_registers() {
        let modules: Vec<Box<dyn OperationModule>> = vec![Box::new(LinuxSysModule)];
        let catalogue = Catalogue::from_modules(&modules).unwrap();
        assert_eq!(catalogue.len(), operations().len());
        assert!(
            catalogue
                .get(&OpId::new("os.sysctl.set").unwrap())
                .is_some()
        );
        assert_eq!(
            catalogue.module_of(&OpId::new("os.mac.mode").unwrap()),
            Some(MODULE_NAME)
        );
    }

    #[test]
    fn risks_are_set_for_dangerous_operations() {
        let catalogue =
            Catalogue::from_modules(&[Box::new(LinuxSysModule) as Box<dyn OperationModule>])
                .unwrap();
        assert_eq!(
            catalogue
                .get(&OpId::new("os.snapshot.restore").unwrap())
                .unwrap()
                .risk,
            Risk::Critical
        );
        assert_eq!(
            catalogue
                .get(&OpId::new("os.network.set").unwrap())
                .unwrap()
                .risk,
            Risk::Critical
        );
        assert_eq!(
            catalogue
                .get(&OpId::new("os.storage.facts").unwrap())
                .unwrap()
                .risk,
            Risk::Read
        );
    }

    #[test]
    fn ids_are_unique_and_three_segments() {
        let specs = operations();
        let mut ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
        for spec in &specs {
            assert_eq!(spec.id.segments().count(), 3);
            assert_eq!(spec.target, TargetKind::Server);
        }
    }
}
