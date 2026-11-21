//! The catalogue's shape: ids, risks, collisions, confirm parameters, secrets and validation
//! tables.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_mgmt_ops::{CatalogueBuilder, OperationModule, ParamType, ParamValue, Risk, TargetKind};
use spot_mgmt_sync::{SyncModule, ids};
use support::*;

#[test]
fn registers_exactly_the_eighteen_operations_with_their_risks() {
    let catalogue = catalogue();
    let expected = [
        (ids::HEALTH, Risk::Read),
        (ids::NODE_LIST, Risk::Read),
        (ids::NODE_SHOW, Risk::Read),
        (ids::NODE_ENROL, Risk::High),
        (ids::NODE_SUSPEND, Risk::High),
        (ids::NODE_RESUME, Risk::Medium),
        (ids::NODE_REVOKE, Risk::Critical),
        (ids::NODE_ROTATE_CERTIFICATE, Risk::Medium),
        (ids::NODE_RESEND, Risk::Medium),
        (ids::NODE_NOTIFY_CONTACT, Risk::Low),
        (ids::CONDITION_LIST, Risk::Read),
        (ids::QUARANTINE_LIST, Risk::Read),
        (ids::QUARANTINE_RETRY, Risk::Medium),
        (ids::QUARANTINE_DISCARD, Risk::High),
        (ids::POLICY_SHOW, Risk::Read),
        (ids::POLICY_LINT, Risk::Read),
        (ids::ROLE_CHANGE, Risk::Critical),
        (ids::BOOTSTRAP_STATUS, Risk::Read),
    ];
    assert_eq!(catalogue.len(), expected.len());
    assert_eq!(ids::ALL.len(), expected.len());
    for (id, risk) in expected {
        let spec = catalogue.get(&ids::op_id(id).unwrap()).unwrap();
        assert_eq!(spec.risk, risk, "{id}");
        assert_eq!(spec.target, TargetKind::Tenant, "{id}");
    }
}

#[test]
fn ids_do_not_collide_with_the_linux_database_and_journal_modules() {
    let mut builder = CatalogueBuilder::new();
    spot_agent_linux::catalogue::LinuxModule.register(&mut builder);
    spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
    spot_mgmt_journal::JournalModule.register(&mut builder);
    SyncModule.register(&mut builder);
    let merged = builder.build().expect("no id, parameter or flag collision");
    for id in ids::ALL {
        let owner = merged.module_of(&ids::op_id(id).unwrap()).unwrap();
        assert_eq!(owner, "spot-mgmt-sync", "{id}");
    }
    for id in spot_mgmt_journal::ids::ALL {
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
            assert!(confirm.required, "{}: required", spec.id.as_str());
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
fn discard_also_requires_a_confirm_because_it_destroys_an_item() {
    let catalogue = catalogue();
    let spec = catalogue
        .get(&ids::op_id(ids::QUARANTINE_DISCARD).unwrap())
        .unwrap();
    let confirm = spec.params.iter().find(|p| p.name == "confirm").unwrap();
    assert!(confirm.required && confirm.default.is_none());
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
        for word in ["secret", "password", "token", "key", "code"] {
            assert!(!id.contains(word), "{id} mentions {word}");
        }
        // No parameter names a path, a URL, a shell fragment, a secret or a tenant.
        for p in &spec.params {
            for word in [
                "path", "url", "cmd", "command", "shell", "tenant", "secret", "token", "password",
            ] {
                assert!(!p.name.contains(word), "{id}: parameter {}", p.name);
            }
        }
    }
}

#[test]
fn enrol_has_no_secret_parameter_and_a_bounded_validity() {
    let catalogue = catalogue();
    let spec = catalogue
        .get(&ids::op_id(ids::NODE_ENROL).unwrap())
        .unwrap();
    let names: Vec<_> = spec.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["node", "role", "partitions", "subscription", "valid_hours"]
    );
    let hours = spec
        .params
        .iter()
        .find(|p| p.name == "valid_hours")
        .unwrap();
    assert_eq!(hours.ty, ParamType::Int { min: 1, max: 168 });
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

#[test]
fn every_sample_is_valid() {
    let catalogue = catalogue();
    assert_eq!(samples().len(), catalogue.len());
    for inv in samples() {
        catalogue.validate(&inv).unwrap();
    }
}

#[test]
fn node_parameters_accept_host_names_and_refuse_everything_else() {
    for good in [
        "branch07",
        "branch07.spotledger.app",
        "a",
        "n-1.example.org",
    ] {
        accepted(ids::NODE_SHOW, &[("node", text(good))]);
    }
    let bads = bad_set(
        &[
            "", "Branch07", "-lead", "a b", "a/b", "../etc", "a;rm", "http://x", "a\nb",
        ],
        129,
    );
    for bad in &bads {
        rejected(ids::NODE_SHOW, &[("node", text(bad))]);
    }
    rejected(ids::NODE_SHOW, &[]);
    rejected(ids::NODE_SHOW, &[("node", ParamValue::Int(1))]);
}

#[test]
fn enrol_table() {
    let base = |hours: ParamValue, partitions: ParamValue, role: &str| {
        vec![
            ("node", text("branch07")),
            ("role", text(role)),
            ("partitions", partitions),
            ("subscription", text("branch-standard")),
            ("valid_hours", hours),
        ]
    };
    accepted(
        ids::NODE_ENROL,
        &base(ParamValue::Int(1), list(&["sales"]), "edge"),
    );
    accepted(
        ids::NODE_ENROL,
        &base(ParamValue::Int(168), list(&["sales", "stock"]), "edge"),
    );
    for hours in [0, 169, -1, 10_000] {
        rejected(
            ids::NODE_ENROL,
            &base(ParamValue::Int(hours), list(&["sales"]), "edge"),
        );
    }
    // Only an edge is enrolled; a hub is not.
    rejected(
        ids::NODE_ENROL,
        &base(ParamValue::Int(24), list(&["sales"]), "hub"),
    );
    // A partition is a name, not a path.
    rejected(
        ids::NODE_ENROL,
        &base(ParamValue::Int(24), list(&["../sales"]), "edge"),
    );
    rejected(
        ids::NODE_ENROL,
        &base(ParamValue::Int(24), list(&["Sales"]), "edge"),
    );
    // Partitions are required and a list is required.
    rejected(
        ids::NODE_ENROL,
        &[
            ("node", text("branch07")),
            ("subscription", text("branch-standard")),
        ],
    );
    rejected(
        ids::NODE_ENROL,
        &base(ParamValue::Int(24), text("sales"), "edge"),
    );
    // Defaults: role edge and 24 hours.
    let v = validated(
        ids::NODE_ENROL,
        &[
            ("node", text("branch07")),
            ("partitions", list(&["sales"])),
            ("subscription", text("branch-standard")),
        ],
    );
    let hours = v.params.iter().find(|p| p.name == "valid_hours").unwrap();
    assert_eq!(hours.value, ParamValue::Int(24));
}

#[test]
fn resend_position_table() {
    accepted(ids::NODE_RESEND, &[("node", text("n1"))]);
    accepted(
        ids::NODE_RESEND,
        &[("node", text("n1")), ("from", text("0:0"))],
    );
    for bad in [
        "", "3", "3:", ":4", "a:b", "3:41:1", "-1:2", "3: 41", "3:41\n",
    ] {
        rejected(
            ids::NODE_RESEND,
            &[("node", text("n1")), ("from", text(bad))],
        );
    }
}

#[test]
fn quarantine_and_policy_tables() {
    for bad in &bad_set(&["", "-x", "a b", "a/b", "a;b"], 129) {
        rejected(ids::QUARANTINE_RETRY, &[("item", text(bad))]);
    }
    let discard = |reason: &str, confirm: &str| {
        invocation(
            ids::QUARANTINE_DISCARD,
            &[
                ("item", text("q-1")),
                ("reason_code", text(reason)),
                ("confirm", text(confirm)),
            ],
        )
    };
    let catalogue = catalogue();
    assert!(catalogue.validate(&discard("superseded", "q-1")).is_ok());
    for reason in &bad_set(&["", "Superseded", "has space", "1abc", "a-b"], 65) {
        assert!(
            catalogue.validate(&discard(reason, "q-1")).is_err(),
            "{reason}"
        );
    }
    rejected(
        ids::QUARANTINE_DISCARD,
        &[("item", text("q-1")), ("reason_code", text("superseded"))],
    );
    accepted(ids::POLICY_SHOW, &[("doctype", text("Sales_Invoice"))]);
    for bad in &bad_set(&["", "1x", "a b", "a/b", "../x"], 65) {
        rejected(ids::POLICY_SHOW, &[("doctype", text(bad))]);
    }
    rejected(ids::POLICY_SHOW, &[]);
}

#[test]
fn role_change_table() {
    accepted(
        ids::ROLE_CHANGE,
        &[("to", text("edge")), ("confirm", text("edge"))],
    );
    rejected(
        ids::ROLE_CHANGE,
        &[("to", text("spoke")), ("confirm", text("spoke"))],
    );
    rejected(ids::ROLE_CHANGE, &[("to", text("hub"))]);
    rejected(ids::ROLE_CHANGE, &[("confirm", text("hub"))]);
}

#[test]
fn revoke_needs_its_confirm() {
    rejected(ids::NODE_REVOKE, &[("node", text("n1"))]);
    accepted(
        ids::NODE_REVOKE,
        &[("node", text("n1")), ("confirm", text("n1"))],
    );
}

#[test]
fn read_operations_take_no_unknown_parameter() {
    for id in [
        ids::HEALTH,
        ids::NODE_LIST,
        ids::CONDITION_LIST,
        ids::QUARANTINE_LIST,
        ids::POLICY_LINT,
        ids::BOOTSTRAP_STATUS,
    ] {
        accepted(id, &[]);
        rejected(id, &[("tenant", text("other"))]);
        rejected(id, &[("path", text("/etc/passwd"))]);
    }
}
