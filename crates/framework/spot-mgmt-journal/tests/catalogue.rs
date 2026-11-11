//! The catalogue's shape: ids, risks, collisions, confirm parameters, secrets and validation
//! tables.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_mgmt_journal::{JournalModule, ids};
use spot_mgmt_ops::{CatalogueBuilder, OperationModule, ParamType, ParamValue, Risk, TargetKind};
use support::*;

#[test]
fn registers_exactly_the_sixteen_operations_with_their_risks() {
    let catalogue = catalogue();
    let expected = [
        (ids::STATUS, Risk::Read),
        (ids::RECOVERY_REPORT, Risk::Read),
        (ids::SNAPSHOT_LIST, Risk::Read),
        (ids::SNAPSHOT_TAKE, Risk::Medium),
        (ids::VERIFY, Risk::Low),
        (ids::AUDIT_LAST, Risk::Read),
        (ids::AUDIT_RUN, Risk::Medium),
        (ids::POLICY_GET, Risk::Read),
        (ids::POLICY_SET, Risk::High),
        (ids::RESHIP, Risk::Medium),
        (ids::REBUILD_PLAN, Risk::Read),
        (ids::REBUILD_RUN, Risk::Critical),
        (ids::READER_LIST, Risk::Read),
        (ids::READER_GRANT, Risk::High),
        (ids::READER_REVOKE, Risk::High),
        (ids::SUBJECT_ERASE, Risk::Critical),
    ];
    assert_eq!(catalogue.len(), expected.len());
    for (id, risk) in expected {
        let spec = catalogue.get(&ids::op_id(id).unwrap()).unwrap();
        assert_eq!(spec.risk, risk, "{id}");
        assert_eq!(spec.target, TargetKind::Tenant, "{id}");
    }
}

#[test]
fn ids_do_not_collide_with_the_linux_and_database_modules() {
    let mut builder = CatalogueBuilder::new();
    spot_agent_linux::catalogue::LinuxModule.register(&mut builder);
    spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
    JournalModule.register(&mut builder);
    let merged = builder.build().expect("no id, parameter or flag collision");
    for id in ids::ALL {
        let owner = merged.module_of(&ids::op_id(id).unwrap()).unwrap();
        assert_eq!(owner, "spot-mgmt-journal", "{id}");
    }
}

#[test]
fn every_critical_operation_requires_an_explicit_confirm_parameter() {
    let catalogue = catalogue();
    let mut critical = 0;
    for spec in catalogue.iter().filter(|s| s.risk == Risk::Critical) {
        critical += 1;
        let confirms: Vec<_> = spec
            .params
            .iter()
            .filter(|p| p.name.starts_with("confirm"))
            .collect();
        assert!(
            !confirms.is_empty(),
            "{} has a confirm parameter",
            spec.id.as_str()
        );
        for confirm in confirms {
            assert!(
                confirm.required,
                "{}: {} is required",
                spec.id.as_str(),
                confirm.name
            );
            assert!(
                confirm.default.is_none(),
                "{}: no default",
                spec.id.as_str()
            );
        }
    }
    assert_eq!(critical, 2);
}

#[test]
fn no_parameter_is_secret_and_no_id_mentions_a_secret() {
    for spec in catalogue().iter() {
        assert!(
            spec.params.iter().all(|p| !p.secret),
            "{}",
            spec.id.as_str()
        );
        let id = spec.id.as_str();
        for word in ["secret", "password", "token", "key"] {
            assert!(!id.contains(word), "{id} mentions {word}");
        }
        // No parameter names a path, a URL, a shell fragment or a tenant.
        for p in &spec.params {
            for word in ["path", "url", "cmd", "command", "shell", "tenant"] {
                assert!(!p.name.contains(word), "{id}: parameter {}", p.name);
            }
        }
    }
}

fn rejected(op: &str, params: &[(&str, ParamValue)]) {
    assert!(
        catalogue().validate(&invocation(op, params)).is_err(),
        "{op} {params:?} must be refused"
    );
}

fn accepted(op: &str, params: &[(&str, ParamValue)]) {
    catalogue()
        .validate(&invocation(op, params))
        .unwrap_or_else(|e| panic!("{op} {params:?}: {e:?}"));
}

const HASH: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn snapshot_list_limit_table() {
    accepted(ids::SNAPSHOT_LIST, &[]);
    accepted(ids::SNAPSHOT_LIST, &[("limit", ParamValue::Int(1))]);
    accepted(ids::SNAPSHOT_LIST, &[("limit", ParamValue::Int(200))]);
    rejected(ids::SNAPSHOT_LIST, &[("limit", ParamValue::Int(0))]);
    rejected(ids::SNAPSHOT_LIST, &[("limit", ParamValue::Int(201))]);
    rejected(ids::SNAPSHOT_LIST, &[("limit", text("5"))]);
}

#[test]
fn parameterless_operations_refuse_any_parameter() {
    for id in [
        ids::STATUS,
        ids::RECOVERY_REPORT,
        ids::SNAPSHOT_TAKE,
        ids::VERIFY,
        ids::AUDIT_LAST,
        ids::AUDIT_RUN,
        ids::POLICY_GET,
        ids::RESHIP,
        ids::READER_LIST,
    ] {
        accepted(id, &[]);
        rejected(id, &[("path", text("/etc/passwd"))]);
        rejected(id, &[("tenant", text("other"))]);
    }
}

#[test]
fn policy_set_table() {
    accepted(ids::POLICY_SET, &[("policy", text("local"))]);
    accepted(ids::POLICY_SET, &[("policy", text("replica-sync"))]);
    accepted(
        ids::POLICY_SET,
        &[
            ("policy", text("remote-sync")),
            ("on_unreachable", text("halt")),
        ],
    );
    rejected(ids::POLICY_SET, &[]);
    rejected(ids::POLICY_SET, &[("policy", text("sync"))]);
    rejected(ids::POLICY_SET, &[("policy", ParamValue::Int(1))]);
    rejected(
        ids::POLICY_SET,
        &[
            ("policy", text("remote-sync")),
            ("on_unreachable", text("ignore")),
        ],
    );
}

#[test]
fn rebuild_table() {
    for target in [
        "latest",
        "at 3:41",
        "before 2026-11-01T10:00:00Z",
        "before 2026-11-01T10:00:00.5+05:00",
    ] {
        accepted(
            ids::REBUILD_PLAN,
            &[("target", text(target)), ("mode", text("full"))],
        );
    }
    for target in [
        "",
        "now",
        "at 3",
        "at :4",
        "at 3:4:5",
        "before yesterday",
        "before 2026-11-01",
        "latest; rm -rf /",
        "/var/lib/journal",
    ] {
        rejected(
            ids::REBUILD_PLAN,
            &[("target", text(target)), ("mode", text("full"))],
        );
    }
    rejected(
        ids::REBUILD_PLAN,
        &[("target", text("latest")), ("mode", text("quick"))],
    );
    rejected(ids::REBUILD_PLAN, &[("target", text("latest"))]);
    rejected(ids::REBUILD_PLAN, &[("mode", text("full"))]);
    rejected(
        ids::REBUILD_PLAN,
        &[("target", ParamValue::Int(1)), ("mode", text("full"))],
    );
}

#[test]
fn rebuild_run_needs_the_plan_hash() {
    let base = [("target", text("latest")), ("mode", text("full"))];
    let mut ok = base.to_vec();
    ok.push(("confirm_plan_hash", text(HASH)));
    accepted(ids::REBUILD_RUN, &ok);
    rejected(ids::REBUILD_RUN, &base);
    for bad in [
        "",
        "abc",
        "0123456789ABCDEF0123456789ABCDEF",
        "xyz0123456789abcdef",
        &"a".repeat(129),
    ] {
        let mut params = base.to_vec();
        params.push(("confirm_plan_hash", text(bad)));
        rejected(ids::REBUILD_RUN, &params);
    }
}

#[test]
fn reader_grant_table() {
    accepted(
        ids::READER_GRANT,
        &[("grantee", text("auditor")), ("days", ParamValue::Int(1))],
    );
    accepted(
        ids::READER_GRANT,
        &[
            ("grantee", text("a@b.example")),
            ("days", ParamValue::Int(365)),
        ],
    );
    rejected(
        ids::READER_GRANT,
        &[("grantee", text("auditor")), ("days", ParamValue::Int(0))],
    );
    rejected(
        ids::READER_GRANT,
        &[("grantee", text("auditor")), ("days", ParamValue::Int(366))],
    );
    rejected(
        ids::READER_GRANT,
        &[("grantee", text("auditor")), ("days", text("30"))],
    );
    rejected(ids::READER_GRANT, &[("days", ParamValue::Int(30))]);
    rejected(
        ids::READER_GRANT,
        &[("grantee", text("../root")), ("days", ParamValue::Int(30))],
    );
    rejected(
        ids::READER_GRANT,
        &[("grantee", text("a b")), ("days", ParamValue::Int(30))],
    );
    rejected(
        ids::READER_GRANT,
        &[
            ("grantee", text("auditor")),
            ("days", ParamValue::Int(30)),
            ("external", text("yes")),
        ],
    );
    accepted(ids::READER_REVOKE, &[("grantee", text("auditor"))]);
    rejected(ids::READER_REVOKE, &[]);
    rejected(ids::READER_REVOKE, &[("grantee", text("$(id)"))]);
}

#[test]
fn subject_erase_needs_the_literal_confirm() {
    accepted(
        ids::SUBJECT_ERASE,
        &[("subject", text("s-1")), ("confirm", text("erase"))],
    );
    rejected(ids::SUBJECT_ERASE, &[("subject", text("s-1"))]);
    rejected(ids::SUBJECT_ERASE, &[("confirm", text("erase"))]);
    for wrong in ["", "Erase", "yes", "erase ", "erase now"] {
        rejected(
            ids::SUBJECT_ERASE,
            &[("subject", text("s-1")), ("confirm", text(wrong))],
        );
    }
    rejected(
        ids::SUBJECT_ERASE,
        &[
            ("subject", text("s-1")),
            ("confirm", ParamValue::Bool(true)),
        ],
    );
}

#[test]
fn types_are_closed() {
    // No `Any`, no list and no duration in the journal's vocabulary: every parameter is a
    // bool, a bounded int, a patterned text or a closed enum.
    for spec in catalogue().iter() {
        for p in &spec.params {
            match &p.ty {
                ParamType::Bool | ParamType::Int { .. } | ParamType::Enum(_) => {}
                ParamType::Text { pattern, .. } => {
                    assert!(
                        pattern.is_some(),
                        "{}: {} has a pattern",
                        spec.id.as_str(),
                        p.name
                    );
                }
                other => panic!("{}: {} is {other:?}", spec.id.as_str(), p.name),
            }
        }
    }
}
