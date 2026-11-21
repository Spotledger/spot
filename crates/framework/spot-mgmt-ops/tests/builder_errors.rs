//! The builder's error set: every way a catalogue build can fail, all reported together,
//! each with its stable `ops.*` code.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_mgmt_ops::{
    CatalogueBuilder, CatalogueError, OpId, OperationSpec, ParamSpec, ParamType, ParamValue, Risk,
    TargetKind,
};

/// A minimal valid spec, for tests that break one aspect at a time.
fn minimal(id: &str) -> OperationSpec {
    OperationSpec {
        id: OpId::new(id).unwrap(),
        title: "Minimal".to_owned(),
        summary: "A minimal operation.".to_owned(),
        params: vec![],
        risk: Risk::Read,
        target: TargetKind::Server,
    }
}

#[test]
fn duplicate_ids_error_names_both_modules() {
    let mut builder = CatalogueBuilder::new();
    builder.add("linux", minimal("server.service.restart"));
    builder.add("windows", minimal("server.service.restart"));
    builder.add("database", minimal("tenant.backup.run"));
    let errors = builder.build().expect_err("duplicate ids are refused");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].code(), "ops.duplicate_id");
    assert!(
        errors[0].to_string().contains("linux")
            && errors[0].to_string().contains("windows")
            && errors[0].to_string().contains("server.service.restart"),
        "the error names both modules and the id: {}",
        errors[0]
    );
    // The same module registering an id twice names itself on both sides.
    let mut builder = CatalogueBuilder::new();
    builder.add("linux", minimal("server.service.restart"));
    builder.add("linux", minimal("server.service.restart"));
    let errors = builder.build().expect_err("duplicates refused");
    assert_eq!(errors[0].code(), "ops.duplicate_id");
    assert!(errors[0].to_string().contains("linux"));
}

#[test]
fn duplicate_parameter_names_inside_one_operation() {
    let mut spec = minimal("server.service.restart");
    spec.params = vec![
        ParamSpec {
            name: "wait".to_owned(),
            ty: ParamType::Duration,
            required: false,
            default: None,
            doc: "Wait one.".to_owned(),
            secret: false,
        },
        ParamSpec {
            name: "wait".to_owned(),
            ty: ParamType::Duration,
            required: false,
            default: None,
            doc: "Wait two.".to_owned(),
            secret: false,
        },
    ];
    let mut builder = CatalogueBuilder::new();
    builder.add("linux", spec);
    let errors = builder.build().expect_err("duplicate parameters refused");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].code(), "ops.duplicate_param");
    assert_eq!(errors[0].op(), "server.service.restart");
}

#[test]
fn a_default_that_breaks_its_own_type_is_refused() {
    // Text default on an Int parameter: shape.
    let mut spec = minimal("tenant.backup.run");
    spec.params = vec![ParamSpec {
        name: "keep".to_owned(),
        ty: ParamType::Int { min: 1, max: 10 },
        required: false,
        default: Some(ParamValue::Text("weekly".to_owned())),
        doc: "Keep.".to_owned(),
        secret: false,
    }];
    let mut builder = CatalogueBuilder::new();
    builder.add("database", spec);
    let errors = builder.build().expect_err("wrong-shaped default refused");
    assert_eq!(errors[0].code(), "ops.default_invalid");
    assert!(errors[0].to_string().contains("shape"), "{}", errors[0]);

    // Out-of-range default.
    let mut spec = minimal("tenant.backup.run");
    spec.params = vec![ParamSpec {
        name: "keep".to_owned(),
        ty: ParamType::Int { min: 1, max: 10 },
        required: false,
        default: Some(ParamValue::Int(11)),
        doc: "Keep.".to_owned(),
        secret: false,
    }];
    let mut builder = CatalogueBuilder::new();
    builder.add("database", spec);
    let errors = builder.build().expect_err("out-of-range default refused");
    assert!(
        errors[0].to_string().contains("out of range 1..10"),
        "{}",
        errors[0]
    );
}

#[test]
fn a_secret_parameter_may_not_have_a_default() {
    let mut spec = minimal("tenant.backup.run");
    spec.params = vec![ParamSpec {
        name: "passphrase".to_owned(),
        ty: ParamType::Text {
            max_len: 128,
            pattern: None,
        },
        required: true,
        default: Some(ParamValue::Text("hunter2".to_owned())),
        doc: "The passphrase.".to_owned(),
        secret: true,
    }];
    let mut builder = CatalogueBuilder::new();
    builder.add("database", spec);
    let errors = builder.build().expect_err("secret with default refused");
    assert_eq!(errors[0].code(), "ops.secret_with_default");
    // The message must not echo the secret value.
    assert!(!errors[0].to_string().contains("hunter2"), "{}", errors[0]);
}

#[test]
fn an_invalid_pattern_is_refused_at_build_time() {
    let mut spec = minimal("server.service.restart");
    spec.params = vec![ParamSpec {
        name: "name".to_owned(),
        ty: ParamType::Text {
            max_len: 64,
            pattern: Some("[a-z".to_owned()),
        },
        required: true,
        default: None,
        doc: "Name.".to_owned(),
        secret: false,
    }];
    let mut builder = CatalogueBuilder::new();
    builder.add("linux", spec);
    let errors = builder.build().expect_err("broken regex refused");
    assert_eq!(errors[0].code(), "ops.pattern_invalid");
}

#[test]
fn an_int_range_with_min_above_max_is_refused() {
    let mut spec = minimal("tenant.backup.run");
    spec.params = vec![ParamSpec {
        name: "keep".to_owned(),
        ty: ParamType::Int { min: 10, max: 1 },
        required: false,
        default: None,
        doc: "Keep.".to_owned(),
        secret: false,
    }];
    let mut builder = CatalogueBuilder::new();
    builder.add("database", spec);
    let errors = builder.build().expect_err("inverted range refused");
    assert_eq!(errors[0].code(), "ops.int_range_empty");
}

#[test]
fn an_empty_enum_is_refused() {
    let mut spec = minimal("tenant.backup.run");
    spec.params = vec![ParamSpec {
        name: "level".to_owned(),
        ty: ParamType::Enum(vec![]),
        required: false,
        default: None,
        doc: "Level.".to_owned(),
        secret: false,
    }];
    let mut builder = CatalogueBuilder::new();
    builder.add("database", spec);
    let errors = builder.build().expect_err("empty enum refused");
    assert_eq!(errors[0].code(), "ops.enum_empty");
}

#[test]
fn bad_parameter_names_and_flag_collisions_are_refused() {
    // Upper-case and hyphenated names can never become flags.
    let mut spec = minimal("server.service.restart");
    spec.params = vec![ParamSpec {
        name: "Max_Wait".to_owned(),
        ty: ParamType::Duration,
        required: false,
        default: None,
        doc: "Wait.".to_owned(),
        secret: false,
    }];
    let mut builder = CatalogueBuilder::new();
    builder.add("linux", spec);
    let errors = builder.build().expect_err("bad name refused");
    assert_eq!(errors[0].code(), "ops.param_name");

    // A boolean `cleanup` collides with the flag `--no-cleanup` of a boolean `no_cleanup`.
    let mut spec = minimal("server.service.restart");
    spec.params = vec![
        ParamSpec {
            name: "no_cleanup".to_owned(),
            ty: ParamType::Bool,
            required: false,
            default: None,
            doc: "No cleanup.".to_owned(),
            secret: false,
        },
        ParamSpec {
            name: "cleanup".to_owned(),
            ty: ParamType::Bool,
            required: false,
            default: None,
            doc: "Cleanup.".to_owned(),
            secret: false,
        },
    ];
    let mut builder = CatalogueBuilder::new();
    builder.add("linux", spec);
    let errors = builder.build().expect_err("flag collision refused");
    assert_eq!(errors[0].code(), "ops.flag_collision");
    assert!(
        errors[0].to_string().contains("--no-cleanup"),
        "{}",
        errors[0]
    );
}

#[test]
fn every_problem_is_reported_together_in_one_build() {
    let mut spec_a = minimal("server.service.restart");
    spec_a.params = vec![
        // Broken pattern.
        ParamSpec {
            name: "name".to_owned(),
            ty: ParamType::Text {
                max_len: 64,
                pattern: Some("([a-z".to_owned()),
            },
            required: true,
            default: None,
            doc: "Name.".to_owned(),
            secret: false,
        },
        // Inverted int range.
        ParamSpec {
            name: "wait_count".to_owned(),
            ty: ParamType::Int { min: 5, max: 1 },
            required: false,
            default: None,
            doc: "Count.".to_owned(),
            secret: false,
        },
    ];
    let mut builder = CatalogueBuilder::new();
    builder.add("linux", spec_a);
    builder.add("windows", minimal("server.service.restart")); // duplicate id
    let errors = builder.build().expect_err("three problems at once");
    let mut codes: Vec<&str> = errors.iter().map(CatalogueError::code).collect();
    codes.sort_unstable();
    assert_eq!(
        codes,
        vec![
            "ops.duplicate_id",
            "ops.int_range_empty",
            "ops.pattern_invalid"
        ],
        "{errors:#?}"
    );
}

#[test]
fn serde_round_trips_the_toy_specs() {
    for spec in [
        common::service_restart(),
        common::backup_run(),
        common::user_create(),
    ] {
        let json = serde_json::to_string(&spec).expect("toy spec serialises");
        let back: OperationSpec = serde_json::from_str(&json).expect("toy spec deserialises");
        assert_eq!(back, spec);
    }
}
