//! The catalogue against the contract: every id, risk, summary, parameter, bound and requirement
//! of the eighteen site operations.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use spot_mgmt_ops::{ParamType, Risk};
use spot_mgmt_site::{SITE_OPERATIONS, handlers, requires, specs};

const CONTRACT: &str = include_str!("../../../../contracts/management-api/openapi.yaml");

fn contract_line(id: &str) -> &'static str {
    let prefix = format!("  {id}: {{");
    CONTRACT
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("{id} is not in the contract export"))
}

fn risk_name(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "read",
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
        Risk::Critical => "critical",
    }
}

#[test]
fn the_eight_entries_agree_with_the_contract() {
    let all = specs();
    assert_eq!(all.len(), 8);
    for spec in all {
        let line = contract_line(spec.id.as_str());
        assert!(line.contains(&format!("\"id\": \"{}\"", spec.id.as_str())));
        assert!(
            line.contains(&format!("\"risk\": \"{}\"", risk_name(spec.risk))),
            "{}",
            spec.id.as_str()
        );
        assert!(
            line.contains(&format!("\"summary\": \"{}\"", spec.summary)),
            "{}",
            spec.id.as_str()
        );
        for param in &spec.params {
            assert!(line.contains(&format!("\"name\": \"{}\"", param.name)));
            assert!(line.contains(&format!("\"is_secret\": {}", param.secret)));
            if let ParamType::Text { max_len, pattern } = &param.ty {
                assert!(line.contains(&format!("\"max_len\": {max_len}")));
                if let Some(pattern) = pattern {
                    assert!(line.contains(&format!("\"pattern\": \"{pattern}\"")));
                }
            }
        }
        assert_eq!(
            line.matches("\"name\":").count(),
            spec.params.len(),
            "{} parameter count",
            spec.id.as_str()
        );
        for requirement in requires(spec.id.as_str()) {
            assert!(line.contains(&format!("\"{requirement}\"")));
        }
        assert_eq!(
            line.contains("\"requires\": []"),
            requires(spec.id.as_str()).is_empty()
        );
    }
}

#[test]
fn every_site_operation_of_the_contract_is_covered() {
    let mut contract = Vec::new();
    for line in CONTRACT.lines() {
        if let Some(rest) = line.strip_prefix("  site.")
            && let Some((name, _)) = rest.split_once(": {")
        {
            contract.push(format!("site.{name}"));
        }
    }
    assert_eq!(contract.len(), 18, "{contract:?}");
    let mut served: Vec<String> = handlers(common::context(common::FakeCli::new(vec![])))
        .iter()
        .map(|op| op.id().to_owned())
        .collect();
    served.sort();
    contract.sort();
    assert_eq!(served, contract);
}

#[test]
fn the_catalogue_builds_and_every_id_has_a_handler() {
    let catalogue = common::catalogue();
    assert_eq!(catalogue.len(), 18);
    let ops = handlers(common::context(common::FakeCli::new(vec![])));
    for spec in catalogue.iter() {
        assert!(ops.iter().any(|op| op.id() == spec.id.as_str()));
    }
    for id in SITE_OPERATIONS {
        assert!(ops.iter().any(|op| op.id() == id), "{id}");
    }
}

#[test]
fn only_reads_and_checks_can_be_cancelled() {
    let ops = handlers(common::context(common::FakeCli::new(vec![])));
    for op in &ops {
        let may = matches!(
            op.id(),
            "site.backup.list"
                | "site.backup.verify"
                | "site.status"
                | "site.migrate.show"
                | "site.migrate.audit"
        );
        assert_eq!(op.cancellable, may, "{}", op.id());
    }
}

#[test]
fn restore_apply_and_maintenance_enter_are_critical_and_need_a_signature() {
    for id in ["site.restore.apply", "site.maintenance.enter"] {
        let spec = specs().into_iter().find(|s| s.id.as_str() == id).unwrap();
        assert_eq!(spec.risk, Risk::Critical);
        assert!(requires(id).contains(&"signature"));
    }
    assert!(requires("site.restore.apply").contains(&"approval:restore"));
}

#[test]
fn the_confirm_parameter_is_a_secret() {
    let spec = specs()
        .into_iter()
        .find(|s| s.id.as_str() == "site.restore.apply")
        .unwrap();
    assert!(spec.params.iter().any(|p| p.name == "confirm" && p.secret));
}
