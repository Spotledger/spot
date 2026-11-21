//! Drift detection, inheritance, validation and remediation plans (the WP-ADM-08 acceptance).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;

use spot_desired_state::{
    BulkPlan, DesiredFleet, DesiredState, Drift, DriftSubject, Error, OperationSet, ParamValue,
    RemediationPolicy, Risk, ScopeRef, ServiceState, SettingKey, detect_drift, group_by_server,
    operation, remediate,
};
use support::{GROUP, group_state, key, observed, registry, server, servers};

fn drifted_indexes() -> BTreeSet<usize> {
    [7, 42, 91].into_iter().collect()
}

#[test]
fn three_drifted_servers_out_of_a_hundred_are_found_exactly() {
    let members = servers(100);
    let desired = DesiredFleet::new(registry(), vec![group_state(&members)]).unwrap();
    let drifted = drifted_indexes();
    let reports = observed(&members, &drifted);

    let found = detect_drift(&desired, &reports, support::NOW, support::DEADLINE);
    assert_eq!(found.len(), 3, "only the three drifted servers may differ");

    let found_servers: BTreeSet<_> = found.iter().map(|drift| drift.server.clone()).collect();
    let expected: BTreeSet<_> = drifted.iter().map(|index| server(*index)).collect();
    assert_eq!(found_servers, expected);

    for drift in &found {
        assert_eq!(
            drift.subject,
            DriftSubject::Setting(key("runtime.logging.level"))
        );
        assert_eq!(drift.group, GROUP);
        assert_eq!(drift.risk, Risk::Low);
        assert_eq!(drift.desired, Some(ParamValue::Text("warn".to_owned())));
        assert_eq!(drift.actual, Some(ParamValue::Text("debug".to_owned())));
        assert_eq!(drift.remediation.len(), 1);
        assert_eq!(drift.remediation[0].id.as_str(), "settings.set");
        assert!(drift.describe().contains("runtime.logging.level"));
    }

    // Deterministic, grouped by server.
    let mut sorted = found.clone();
    sorted.sort_by(|left, right| {
        left.server
            .cmp(&right.server)
            .then_with(|| left.subject.cmp(&right.subject))
    });
    assert_eq!(found, sorted);
    let grouped = group_by_server(&found);
    assert_eq!(grouped.len(), 3);
    for entry in &grouped {
        assert_eq!(entry.drifts.len(), 1);
        assert_eq!(entry.group, GROUP);
    }
}

#[test]
fn a_server_whose_agent_did_not_report_is_not_guessed_at() {
    let members = servers(3);
    let desired = DesiredFleet::new(registry(), vec![group_state(&members)]).unwrap();
    let mut reports = observed(&members, &BTreeSet::new());
    reports.forget(&server(1));

    let found = detect_drift(&desired, &reports, support::NOW, support::DEADLINE);
    // Recorded security finding: a missing report is no longer skipped; the server reads as unknown, and no
    // setting is guessed for it.
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].server, server(1));
    assert!(found[0].is_silent());
    assert!(found[0].setting().is_none());
}

#[test]
fn one_drift_remediates_to_a_plan_of_size_one() {
    let members = servers(100);
    let desired = DesiredFleet::new(registry(), vec![group_state(&members)]).unwrap();
    let reports = observed(&members, &drifted_indexes());
    let found = detect_drift(&desired, &reports, support::NOW, support::DEADLINE);

    let plan = remediate(&found[..1], &RemediationPolicy::standard());
    assert_eq!(plan.size(), 1);
    assert_eq!(plan.operation_count(), 1);
    assert_eq!(plan.risk, Risk::Low);
    assert_eq!(plan.servers().len(), 1);
    assert_eq!(plan.digest(), plan.digest(), "the digest is stable");

    let mut changed = plan.clone();
    changed.steps[0].operations[0]
        .params
        .insert("value".to_owned(), ParamValue::Text("error".to_owned()));
    assert_ne!(
        changed.digest(),
        plan.digest(),
        "a changed step is a new plan"
    );
}

#[test]
fn three_drifts_make_three_steps_and_the_plan_is_order_independent() {
    let members = servers(100);
    let desired = DesiredFleet::new(registry(), vec![group_state(&members)]).unwrap();
    let reports = observed(&members, &drifted_indexes());
    let found = detect_drift(&desired, &reports, support::NOW, support::DEADLINE);
    let forward = remediate(&found, &RemediationPolicy::standard());

    let mut reversed = found.clone();
    reversed.reverse();
    let backward = remediate(&reversed, &RemediationPolicy::standard());

    assert_eq!(forward, backward);
    assert_eq!(forward.size(), 3);
    assert_eq!(forward.operation_count(), 3);
    assert_eq!(forward.group, GROUP);
    let plan_servers: Vec<_> = forward
        .steps
        .iter()
        .map(|step| step.server.clone())
        .collect();
    let mut sorted = plan_servers.clone();
    sorted.sort();
    assert_eq!(plan_servers, sorted, "steps are in server order");
}

#[test]
fn a_bulk_plan_covers_a_hundred_servers() {
    let members = servers(100);
    let plan = remediate(&support::drifts(&members), &RemediationPolicy::standard());
    assert_eq!(plan.size(), 100);
    assert_eq!(plan.servers().len(), 100);
    assert_eq!(plan.operation_count(), 100);
}

#[test]
fn the_policy_keeps_high_risk_and_excluded_kinds_out() {
    let only = [server(1)];

    let mut critical = support::drifts(&only);
    critical[0].risk = Risk::Critical;
    let plan = remediate(&critical, &RemediationPolicy::standard());
    assert!(plan.is_empty());
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].risk, Risk::Critical);

    let operations = OperationSet::builtin().unwrap();
    let package = Drift {
        server: server(2),
        group: GROUP.to_owned(),
        subject: DriftSubject::Package("nginx".to_owned()),
        risk: Risk::Medium,
        desired: Some(ParamValue::Text("1.24".to_owned())),
        actual: None,
        remediation: vec![operation(
            &operations.install_package,
            [
                ("name", ParamValue::Text("nginx".to_owned())),
                ("version", ParamValue::Text("1.24".to_owned())),
            ],
        )],
    };
    let policy = RemediationPolicy {
        include_packages: false,
        ..RemediationPolicy::standard()
    };
    let plan = remediate(&[package], &policy);
    assert!(plan.is_empty());
    assert_eq!(plan.skipped.len(), 1);
}

#[test]
fn inheritance_is_fleet_then_group_then_server_then_tenant() {
    let first = server(1);
    let second = server(2);
    let group = group_state(&[first.clone(), second.clone()])
        .with_package("nginx", "1.24")
        .with_service("nginx", ServiceState::new(true, true));
    let server_override = DesiredState::for_server(GROUP, first.clone())
        .with_setting(
            key("runtime.logging.level"),
            ParamValue::Text("error".to_owned()),
        )
        .with_package("nginx", "1.25");
    let tenant = DesiredState::for_tenant(GROUP, "acme").with_setting(
        key("runtime.logging.level"),
        ParamValue::Text("trace".to_owned()),
    );
    let fleet = DesiredState::fleet_wide("all");

    let desired =
        DesiredFleet::new(registry(), vec![fleet, group, server_override, tenant]).unwrap();

    let first_effective = desired.effective(&first, None);
    assert_eq!(
        first_effective.setting(&key("runtime.logging.level")),
        Some(&ParamValue::Text("error".to_owned()))
    );
    assert_eq!(
        first_effective.packages.get("nginx"),
        Some(&"1.25".to_owned())
    );
    assert_eq!(
        first_effective.services.get("nginx"),
        Some(&ServiceState::new(true, true))
    );

    let second_effective = desired.effective(&second, None);
    assert_eq!(
        second_effective.setting(&key("runtime.logging.level")),
        Some(&ParamValue::Text("warn".to_owned()))
    );
    assert_eq!(
        second_effective.packages.get("nginx"),
        Some(&"1.24".to_owned())
    );

    let tenant_effective = desired.effective(&first, Some("acme"));
    assert_eq!(
        tenant_effective.setting(&key("runtime.logging.level")),
        Some(&ParamValue::Text("trace".to_owned())),
        "the tenant scope is narrower than the server scope"
    );

    // The settings schema's own diff view says what the desired state changes from default.
    let differences = desired.differences(&first, None);
    assert_eq!(differences.len(), 1);
    assert_eq!(differences[0].key, "runtime.logging.level");
    assert_eq!(
        differences[0].value.as_value(),
        Some(&ParamValue::Text("error".to_owned()))
    );
}

#[test]
fn a_desired_state_is_validated_against_the_typed_settings_schema() {
    let empty_group = DesiredFleet::new(
        registry(),
        vec![DesiredState::fleet_wide("").with_setting(
            key("runtime.logging.level"),
            ParamValue::Text("warn".to_owned()),
        )],
    );
    assert_eq!(empty_group.unwrap_err().code(), "desired_state.empty_group");

    let unknown = DesiredFleet::new(
        registry(),
        vec![
            DesiredState::for_group(GROUP)
                .with_setting(key("runtime.nope"), ParamValue::Text("x".to_owned())),
        ],
    );
    assert_eq!(unknown.unwrap_err().code(), "desired_state.unknown_setting");

    let scope = DesiredFleet::new(
        registry(),
        vec![DesiredState::for_group(GROUP).with_setting(
            key("tenant.default_currency"),
            ParamValue::Text("EUR".to_owned()),
        )],
    );
    assert_eq!(scope.unwrap_err().code(), "desired_state.scope_not_allowed");

    let read_only = DesiredFleet::new(
        registry(),
        vec![
            DesiredState::for_server(GROUP, server(1))
                .with_setting(key("os.kernel.version"), ParamValue::Text("6.1".to_owned())),
        ],
    );
    assert_eq!(read_only.unwrap_err().code(), "desired_state.read_only");

    let invalid = DesiredFleet::new(
        registry(),
        vec![
            DesiredState::for_group(GROUP)
                .with_setting(key("runtime.jobs.worker_concurrency"), ParamValue::Int(999)),
        ],
    );
    assert_eq!(invalid.unwrap_err().code(), "desired_state.invalid_value");

    let duplicate = DesiredFleet::new(
        registry(),
        vec![
            DesiredState::for_group(GROUP),
            DesiredState::for_group(GROUP),
        ],
    );
    assert_eq!(
        duplicate.unwrap_err().code(),
        "desired_state.duplicate_layer"
    );

    let mismatch = DesiredFleet::new(
        registry(),
        vec![DesiredState {
            group: GROUP.to_owned(),
            scope: ScopeRef::Group("other".to_owned()),
            settings: std::collections::BTreeMap::new(),
            packages: std::collections::BTreeMap::new(),
            services: std::collections::BTreeMap::new(),
            servers: BTreeSet::new(),
        }],
    );
    assert_eq!(mismatch.unwrap_err().code(), "desired_state.group_mismatch");

    let unsupported = DesiredFleet::new(
        registry(),
        vec![DesiredState {
            group: GROUP.to_owned(),
            scope: ScopeRef::Company("acme/C1".to_owned()),
            settings: std::collections::BTreeMap::new(),
            packages: std::collections::BTreeMap::new(),
            services: std::collections::BTreeMap::new(),
            servers: BTreeSet::new(),
        }],
    );
    assert_eq!(
        unsupported.unwrap_err().code(),
        "desired_state.unsupported_scope"
    );
}

#[test]
fn a_malformed_setting_key_is_refused() {
    let error = SettingKey::new("logging").unwrap_err();
    assert_eq!(error.code(), "desired_state.invalid_key");
    assert!(matches!(error, Error::InvalidKey { .. }));
}

#[test]
fn the_builtin_operation_set_names_catalogue_operations() {
    let operations = OperationSet::builtin().unwrap();
    assert_eq!(operations.set_setting.as_str(), "settings.set");
    assert_eq!(operations.install_package.as_str(), "os.package.install");
    assert_eq!(operations.start_service.as_str(), "os.service.start");
    assert_eq!(operations.stop_service.as_str(), "os.service.stop");
    assert_eq!(operations.enable_service.as_str(), "os.service.enable");
    assert_eq!(operations.disable_service.as_str(), "os.service.disable");
    assert_eq!(operations.remove_package.as_str(), "os.package.remove");

    let plan = BulkPlan {
        group: GROUP.to_owned(),
        steps: Vec::new(),
        skipped: Vec::new(),
        risk: Risk::Read,
    };
    assert!(plan.is_empty());
}
