//! The invocation error set: every validation problem, reported together, each with its
//! stable `ops.*` code; defaults filled into `Validated`; the catalogue API surface.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_mgmt_ops::{InvocationError, ParamValue, Risk, Shell, TargetKind, render};

#[test]
fn unknown_operation_short_circuits_validation() {
    let catalogue = common::catalogue();
    let errors = catalogue
        .validate(&common::invocation("cluster.node.eject"))
        .expect_err("unknown op refused");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].code(), "ops.unknown_op");
}

#[test]
fn every_problem_is_reported_together() {
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("server.service.restart");
    // `wrong` is not a declared parameter; `name` is missing; `force` holds an Int where
    // a Bool belongs. Three distinct problems, one error vector.
    invocation.params = common::params(vec![
        ("wrong", ParamValue::Int(1)),
        ("force", ParamValue::Int(3)),     // wrong type
        ("wait", ParamValue::Duration(0)), // valid
    ]);
    let errors = catalogue.validate(&invocation).expect_err("three problems");
    let mut codes: Vec<&str> = errors.iter().map(InvocationError::code).collect();
    codes.sort_unstable();
    assert_eq!(
        codes,
        vec!["ops.missing_param", "ops.unknown_param", "ops.wrong_type",],
        "{errors:#?}"
    );
    // Both unknown parameters are reported (two of the same kind, too).
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![
        ("wrong", ParamValue::Int(1)),
        ("also_wrong", ParamValue::Int(2)),
    ]);
    let errors = catalogue
        .validate(&invocation)
        .expect_err("two unknowns plus missing");
    let mut codes: Vec<&str> = errors.iter().map(InvocationError::code).collect();
    codes.sort_unstable();
    assert_eq!(
        codes,
        vec![
            "ops.missing_param",
            "ops.unknown_param",
            "ops.unknown_param",
        ],
        "{errors:#?}"
    );
}

#[test]
fn missing_required_and_filled_defaults_shape_the_result() {
    let catalogue = common::catalogue();
    // restart without `name`: missing (no default to fill).
    let errors = catalogue
        .validate(&common::invocation("server.service.restart"))
        .expect_err("name is required");
    assert_eq!(
        errors,
        vec![InvocationError::MissingParam {
            op: "server.service.restart".to_owned(),
            param: "name".to_owned(),
        }]
    );

    // restart with name only: `wait` default 30 and `force` default-less (absent) fill in.
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![("name", common::text("nginx"))]);
    let validated = catalogue.validate(&invocation).expect("valid");
    assert_eq!(validated.op.as_str(), "server.service.restart");
    assert_eq!(validated.params.len(), 2, "name and the defaulted wait");
    assert_eq!(validated.params[0].name, "name");
    assert!(!validated.params[0].from_default);
    assert_eq!(validated.params[1].name, "wait");
    assert_eq!(validated.params[1].value, ParamValue::Duration(30));
    assert!(validated.params[1].from_default);

    // backup without passphrase: missing; without level: default incremental.
    let errors = catalogue
        .validate(&common::invocation("tenant.backup.run"))
        .expect_err("passphrase is required");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code(), "ops.missing_param");
}

#[test]
fn out_of_range_pattern_and_enum_errors_with_codes() {
    let catalogue = common::catalogue();

    // quota out of range on user create.
    let mut invocation = common::invocation("tenant.user.create");
    invocation.params = common::params(vec![
        ("user_name", common::text("ana")),
        ("quota_mb", common::int(99)),
    ]);
    let errors = catalogue.validate(&invocation).expect_err("below min");
    assert_eq!(errors[0].code(), "ops.out_of_range");

    // name pattern mismatch on restart.
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![("name", common::text("NGINX"))]);
    let errors = catalogue
        .validate(&invocation)
        .expect_err("upper-case refused");
    assert_eq!(errors[0].code(), "ops.pattern_mismatch");

    // enum miss on backup level.
    let mut invocation = common::invocation("tenant.backup.run");
    invocation.params = common::params(vec![
        ("passphrase", common::text("correct horse")),
        ("level", common::text("differential-plus")),
    ]);
    let errors = catalogue.validate(&invocation).expect_err("not a choice");
    assert_eq!(errors[0].code(), "ops.not_in_enum");

    // text too long.
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![("name", common::text(&"x".repeat(129)))]);
    let errors = catalogue.validate(&invocation).expect_err("over max_len");
    assert_eq!(errors[0].code(), "ops.too_long");
}

#[test]
fn get_iter_search_behave_and_are_deterministic() {
    let catalogue = common::catalogue();

    let restart = catalogue
        .get(&spot_mgmt_ops::OpId::new("server.service.restart").unwrap())
        .expect("restart is registered");
    assert_eq!(restart.risk, Risk::Medium);
    assert_eq!(restart.target, TargetKind::Server);

    let ids: Vec<&str> = catalogue.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "server.service.echo",
            "server.service.restart",
            "tenant.backup.run",
            "tenant.user.create",
        ],
        "sorted id order"
    );

    assert_eq!(catalogue.search("backup").len(), 1);
    assert_eq!(
        catalogue.search("backup")[0].id.as_str(),
        "tenant.backup.run"
    );
    // Search matches id, title and summary, case-insensitively: "SERVICE" hits "service"
    // in two ids and the restart title.
    assert_eq!(catalogue.search("SERVICE").len(), 2, "case-insensitive");
    assert_eq!(catalogue.search("backup").len(), 1, "the backup id");
    assert!(
        catalogue.search("").is_empty(),
        "empty text matches nothing"
    );
    assert!(catalogue.search("no-such-operation").is_empty());
    assert_eq!(catalogue.search("echo").len(), 1);
    assert_eq!(
        catalogue.search("echo")[0].id.as_str(),
        "server.service.echo"
    );
}

#[test]
fn render_defaults_the_declaration_order_golden() {
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![
        ("name", common::text("nginx")),
        ("wait", common::duration(30)),
        ("force", common::boolean(true)),
    ]);
    let validated = catalogue.validate(&invocation).expect("valid");
    // Declaration order is name, wait, force — not the invocation map's sorted order.
    assert_eq!(
        render(&validated, Shell::Posix),
        "spot ops server service restart --name nginx --wait 30s --force"
    );
}
