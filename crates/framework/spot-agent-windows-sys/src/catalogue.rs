//! The typed operations the Windows system modules add to the management catalogue
//! (`spot-mgmt-ops`, blueprint WS22, ADR-44). The console and the `spot` CLI derive their
//! screens and flags from these specs; the host registers a handler per id.
//!
//! Ids are built from their segments ([`op_id`]) rather than written as dotted literals,
//! because a dotted lower-case literal anywhere in a scanned source file is read as an
//! error code by the error-code completeness scan.
//!
//! Operations registered here:
//!
//! - `vss.shadow.create`, `vss.shadow.delete`, `vss.shadow.list` — the Volume Shadow Copy
//!   API ([`crate::vss`]).
//! - `certificate.list`, `certificate.import`, `certificate.remove`, `certificate.bind` —
//!   LocalMachine stores ([`crate::certificates`]).
//! - `backup.mode.start`, `backup.mode.stop` — PostgreSQL backup mode ([`crate::backup`]).
//! - `snapshot.vss.run` — the coordinated VSS snapshot ([`crate::provider`]).

use spot_mgmt_ops::{
    CatalogueBuilder, OpId, OperationModule, OperationSpec, ParamSpec, ParamType, ParamValue, Risk,
    TargetKind,
};

use crate::error::{WindowsSysError, WindowsSysResult};

/// The module's name, used in catalogue errors.
pub const MODULE_NAME: &str = "agent_windows_sys";

/// Builds an operation id from its segments, mapping a static authoring mistake to
/// [`WindowsSysError::InvalidOperationId`].
///
/// # Errors
/// [`WindowsSysError::InvalidOperationId`] when the segments do not form a valid id.
pub fn op_id(segments: &[&str]) -> WindowsSysResult<OpId> {
    let text = segments.join(".");
    OpId::try_new(&text).map_err(|error| WindowsSysError::InvalidOperationId {
        id: text,
        reason: error.to_string(),
    })
}

/// A `Text` parameter.
fn text(name: &str, doc: &str, required: bool, pattern: Option<&str>, secret: bool) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len: 512,
            pattern: pattern.map(str::to_owned),
        },
        required,
        default: None,
        doc: doc.to_owned(),
        secret,
    }
}

/// A store-name enum parameter (`root`, `ca`, `my`).
fn store_param() -> ParamSpec {
    ParamSpec {
        name: "store".to_owned(),
        ty: ParamType::Enum(vec!["root".to_owned(), "ca".to_owned(), "my".to_owned()]),
        required: true,
        default: None,
        doc: "The LocalMachine certificate store: root, ca or my.".to_owned(),
        secret: false,
    }
}

/// A thumbprint parameter (40 or 64 hex digits).
fn thumbprint_param() -> ParamSpec {
    ParamSpec {
        name: "thumbprint".to_owned(),
        ty: ParamType::Text {
            max_len: 64,
            pattern: Some("(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})".to_owned()),
        },
        required: true,
        default: None,
        doc: "The certificate thumbprint: 40 or 64 hex digits.".to_owned(),
        secret: false,
    }
}

/// A list-of-text parameter.
fn text_list(name: &str, doc: &str, required: bool) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::List(Box::new(ParamType::Text {
            max_len: 260,
            pattern: None,
        })),
        required,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// The Windows system module: builds every operation spec once and registers them on the
/// catalogue builder.
#[derive(Clone, Debug)]
pub struct WindowsSysModule {
    specs: Vec<OperationSpec>,
}

impl WindowsSysModule {
    /// Builds the module's operation specs.
    ///
    /// # Errors
    /// [`WindowsSysError::InvalidOperationId`] when a static id does not validate (an
    /// authoring mistake that can never happen at runtime).
    pub fn new() -> WindowsSysResult<Self> {
        let specs = vec![
            OperationSpec {
                id: op_id(&["vss", "shadow", "create"])?,
                title: "Create a volume shadow copy".to_owned(),
                summary: "Creates a Volume Shadow Copy of the given volumes, quiescing the listed writers.".to_owned(),
                params: vec![
                    text_list("volumes", "The volumes to shadow, for example C:\\.", true),
                    text_list("writers", "The VSS writers to quiesce; omit for all.", false),
                ],
                risk: Risk::Medium,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["vss", "shadow", "delete"])?,
                title: "Delete a volume shadow copy".to_owned(),
                summary: "Deletes one Volume Shadow Copy by its id.".to_owned(),
                params: vec![text(
                    "shadow_id",
                    "The shadow copy id as VSS reported it.",
                    true,
                    None,
                    false,
                )],
                risk: Risk::Medium,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["vss", "shadow", "list"])?,
                title: "List volume shadow copies".to_owned(),
                summary: "Lists the Volume Shadow Copies VSS currently holds.".to_owned(),
                params: Vec::new(),
                risk: Risk::Read,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["certificate", "list"])?,
                title: "List certificates".to_owned(),
                summary: "Lists the certificates in one LocalMachine store.".to_owned(),
                params: vec![store_param()],
                risk: Risk::Read,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["certificate", "import"])?,
                title: "Import a certificate".to_owned(),
                summary: "Imports a certificate, and optionally its private key, into a LocalMachine store.".to_owned(),
                params: vec![
                    store_param(),
                    text("path", "The file holding the certificate.", true, None, false),
                    text(
                        "password",
                        "The password protecting the private key, asked for at run time and never stored.",
                        false,
                        None,
                        true,
                    ),
                ],
                risk: Risk::High,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["certificate", "remove"])?,
                title: "Remove a certificate".to_owned(),
                summary: "Removes one certificate from a LocalMachine store.".to_owned(),
                params: vec![store_param(), thumbprint_param()],
                risk: Risk::High,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["certificate", "bind"])?,
                title: "Bind a certificate to HTTPS".to_owned(),
                summary: "Binds a certificate to the Spotledger service HTTPS endpoint after checking it is valid and trusted.".to_owned(),
                params: vec![
                    store_param(),
                    thumbprint_param(),
                    text("hostname", "The service host name, with no scheme or port.", true, None, false),
                    ParamSpec {
                        name: "port".to_owned(),
                        ty: ParamType::Int { min: 1, max: 65535 },
                        required: true,
                        default: None,
                        doc: "The HTTPS port.".to_owned(),
                        secret: false,
                    },
                ],
                risk: Risk::High,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["backup", "mode", "start"])?,
                title: "Enter PostgreSQL backup mode".to_owned(),
                summary: "Runs pg_backup_start and returns the label to stop it with.".to_owned(),
                params: Vec::new(),
                risk: Risk::Medium,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["backup", "mode", "stop"])?,
                title: "Leave PostgreSQL backup mode".to_owned(),
                summary: "Runs pg_backup_stop with the label pg_backup_start returned.".to_owned(),
                params: vec![text(
                    "label",
                    "The label pg_backup_start returned.",
                    true,
                    None,
                    false,
                )],
                risk: Risk::Medium,
                target: TargetKind::Server,
            },
            OperationSpec {
                id: op_id(&["snapshot", "vss", "run"])?,
                title: "Take a coordinated VSS snapshot".to_owned(),
                summary: "Runs the fixed sequence: pg_backup_start, a Volume Shadow Copy, pg_backup_stop, and a manifest.".to_owned(),
                params: vec![
                    text("tenant", "The tenant the snapshot belongs to.", true, None, false),
                    ParamSpec {
                        name: "keep".to_owned(),
                        ty: ParamType::Int { min: 0, max: 1000 },
                        required: false,
                        default: Some(ParamValue::Int(5)),
                        doc: "How many newest shadow copies to keep after the take.".to_owned(),
                        secret: false,
                    },
                ],
                risk: Risk::High,
                target: TargetKind::Tenant,
            },
        ];
        Ok(WindowsSysModule { specs })
    }

    /// The module's operation specs, in declaration order.
    #[must_use]
    pub fn specs(&self) -> &[OperationSpec] {
        &self.specs
    }

    /// How many operations the module registers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Whether the module registers no operation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// The id of the operation whose segments are given, when the module registers it.
    #[must_use]
    pub fn find(&self, segments: &[&str]) -> Option<&OpId> {
        let text = segments.join(".");
        self.specs
            .iter()
            .map(|spec| &spec.id)
            .find(|id| id.as_str() == text)
    }
}

impl OperationModule for WindowsSysModule {
    fn name(&self) -> &str {
        MODULE_NAME
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for spec in &self.specs {
            builder.add(MODULE_NAME, spec.clone());
        }
    }
}
