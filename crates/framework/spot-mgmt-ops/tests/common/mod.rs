//! The three toy operations every test file uses, defined in the tests only: a service
//! restart with a name and a wait duration, a backup run with a secret passphrase, and a
//! user-create with a list of groups. No real operation belongs in this crate.

#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, Invocation, OpId, OperationSpec, ParamSpec, ParamType, ParamValue,
    Risk, TargetKind,
};

/// `server.service.echo` — a free-text operation the shell round-trip tests drive: its
/// one parameter accepts ANY text (no pattern), so every awkward value is legal.
pub fn service_echo() -> OperationSpec {
    OperationSpec {
        id: OpId::new("server.service.echo").unwrap(),
        title: "Echo a text".to_owned(),
        summary: "Echoes one text back through the agent, for shell-quoting checks.".to_owned(),
        params: vec![ParamSpec {
            name: "message".to_owned(),
            ty: ParamType::Text {
                max_len: 4_096,
                pattern: None,
            },
            required: true,
            default: None,
            doc: "Any text; the shell round-trip tests drive awkward values through it.".to_owned(),
            secret: false,
        }],
        risk: Risk::Read,
        target: TargetKind::Server,
    }
}

/// `server.service.restart` — a required name, an optional wait duration with default.
pub fn service_restart() -> OperationSpec {
    OperationSpec {
        id: OpId::new("server.service.restart").unwrap(),
        title: "Restart a service".to_owned(),
        summary: "Restarts one systemd service and waits for it to come back.".to_owned(),
        params: vec![
            ParamSpec {
                name: "name".to_owned(),
                ty: ParamType::Text {
                    max_len: 128,
                    pattern: Some("[a-z][a-z0-9-@.]*".to_owned()),
                },
                required: true,
                default: None,
                doc: "The service unit name.".to_owned(),
                secret: false,
            },
            ParamSpec {
                name: "wait".to_owned(),
                ty: ParamType::Duration,
                required: false,
                default: Some(ParamValue::Duration(30)),
                doc: "How long to wait for the service to come back.".to_owned(),
                secret: false,
            },
            ParamSpec {
                name: "force".to_owned(),
                ty: ParamType::Bool,
                required: false,
                default: None,
                doc: "Force the restart even when the service is healthy.".to_owned(),
                secret: false,
            },
        ],
        risk: Risk::Medium,
        target: TargetKind::Server,
    }
}

/// `tenant.backup.run` — a secret passphrase (no default) and a level enum.
pub fn backup_run() -> OperationSpec {
    OperationSpec {
        id: OpId::new("tenant.backup.run").unwrap(),
        title: "Run a backup".to_owned(),
        summary: "Runs one backup of the tenant's application data.".to_owned(),
        params: vec![
            ParamSpec {
                name: "passphrase".to_owned(),
                ty: ParamType::Text {
                    max_len: 128,
                    pattern: None,
                },
                required: true,
                default: None,
                doc: "The backup passphrase; asked for at run time, never stored.".to_owned(),
                secret: true,
            },
            ParamSpec {
                name: "level".to_owned(),
                ty: ParamType::Enum(vec![
                    "full".to_owned(),
                    "incremental".to_owned(),
                    "differential".to_owned(),
                ]),
                required: false,
                default: Some(ParamValue::Text("incremental".to_owned())),
                doc: "The backup level.".to_owned(),
                secret: false,
            },
        ],
        risk: Risk::High,
        target: TargetKind::Tenant,
    }
}

/// `tenant.user.create` — a list of groups and a bounded integer.
pub fn user_create() -> OperationSpec {
    OperationSpec {
        id: OpId::new("tenant.user.create").unwrap(),
        title: "Create a user".to_owned(),
        summary: "Creates one user and assigns the given groups.".to_owned(),
        params: vec![
            ParamSpec {
                name: "user_name".to_owned(),
                ty: ParamType::Text {
                    max_len: 64,
                    pattern: None,
                },
                required: true,
                default: None,
                doc: "The user's name.".to_owned(),
                secret: false,
            },
            ParamSpec {
                name: "groups".to_owned(),
                ty: ParamType::List(Box::new(ParamType::Text {
                    max_len: 64,
                    pattern: None,
                })),
                required: false,
                default: Some(ParamValue::List(vec![ParamValue::Text(
                    "observers".to_owned(),
                )])),
                doc: "The groups to assign.".to_owned(),
                secret: false,
            },
            ParamSpec {
                name: "quota_mb".to_owned(),
                ty: ParamType::Int {
                    min: 100,
                    max: 10_000,
                },
                required: false,
                default: Some(ParamValue::Int(1_000)),
                doc: "The storage quota in megabytes.".to_owned(),
                secret: false,
            },
        ],
        risk: Risk::Low,
        target: TargetKind::Tenant,
    }
}

/// The catalogue of all toy operations, including `server.service.echo` (the free-text
/// operation the shell round-trip tests drive).
pub fn catalogue() -> Catalogue {
    catalogue_builder()
        .build()
        .expect("the toy specs are valid")
}

/// The builder of all toy operations, for tests that register more (or expect the build
/// to fail).
pub fn catalogue_builder() -> CatalogueBuilder {
    let mut builder = CatalogueBuilder::new();
    builder.add("test.core", service_restart());
    builder.add("test.core", backup_run());
    builder.add("test.core", user_create());
    builder.add("test.core", service_echo());
    builder
}

/// An empty invocation of `op`.
pub fn invocation(op: &str) -> Invocation {
    Invocation::new(OpId::new(op).unwrap())
}

/// A `Text` parameter value.
pub fn text(value: &str) -> ParamValue {
    ParamValue::Text(value.to_owned())
}

/// A duration value in seconds.
pub fn duration(seconds: u64) -> ParamValue {
    ParamValue::Duration(seconds)
}

/// An integer value.
pub fn int(value: i64) -> ParamValue {
    ParamValue::Int(value)
}

/// A bool value.
pub fn boolean(value: bool) -> ParamValue {
    ParamValue::Bool(value)
}

/// A list value.
pub fn list(values: Vec<ParamValue>) -> ParamValue {
    ParamValue::List(values)
}

/// An invocation's params as a plain map literal.
pub fn params(pairs: Vec<(&str, ParamValue)>) -> BTreeMap<String, ParamValue> {
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}
