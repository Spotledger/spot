//! Catalogue versus contract, the fail-closed stubs and the error mapping.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, Invocation, OperationModule, ParamType, ParamValue, Risk,
};
use spot_migration_ops::catalogue::{requires, stub_registry};
use spot_migration_ops::{LifecycleError, MigrationOpsModule, specs};

fn build() -> Catalogue {
    let mut builder = CatalogueBuilder::new();
    MigrationOpsModule.register(&mut builder);
    builder.build().unwrap()
}

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

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

#[test]
fn the_module_builds_a_catalogue_of_ten_operations() {
    let catalogue = build();
    assert_eq!(catalogue.len(), 10);
}

#[test]
fn every_entry_agrees_with_the_contract() {
    for spec in specs() {
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
            assert!(
                line.contains(&format!("\"name\": \"{}\"", param.name)),
                "{}",
                param.name
            );
            if let ParamType::Text { max_len, pattern } = &param.ty {
                assert!(line.contains(&format!("\"max_len\": {max_len}")));
                if let Some(pattern) = pattern {
                    assert!(line.contains(&format!("\"pattern\": \"{pattern}\"")));
                }
            }
        }
        let count = line.matches("\"name\":").count();
        assert_eq!(
            count,
            spec.params.len(),
            "{} parameter count",
            spec.id.as_str()
        );
        for requirement in requires(spec.id.as_str()) {
            assert!(line.contains(&format!("\"{requirement}\"")));
        }
        let requires_empty = line.contains("\"requires\": []");
        assert_eq!(requires_empty, requires(spec.id.as_str()).is_empty());
    }
}

#[test]
fn every_contract_migrate_operation_is_in_the_catalogue() {
    let ids: Vec<String> = specs().iter().map(|s| s.id.as_str().to_owned()).collect();
    for line in CONTRACT.lines() {
        if let Some(rest) = line.strip_prefix("  site.migrate.") {
            let name = rest.split(':').next().unwrap();
            assert!(ids.contains(&format!("site.migrate.{name}")), "{name}");
        }
    }
}

#[test]
fn every_stub_refuses_with_not_implemented() {
    let catalogue = build();
    let registry = stub_registry();
    assert_eq!(registry.len(), 10);
    for spec in specs() {
        let mut invocation = Invocation::new(spec.id.clone());
        for param in &spec.params {
            let value = match param.name.as_str() {
                "site" => "demo".to_owned(),
                "plan_hash" => "0".repeat(64),
                _ => "because".to_owned(),
            };
            invocation = invocation.with(param.name.clone(), ParamValue::Text(value));
        }
        let validated = catalogue.validate(&invocation).unwrap();
        let handler = registry.get(&spec.id).unwrap();
        let outcome = block_on(handler.handle(&validated));
        match outcome {
            Err(LifecycleError::NotImplemented { op }) => assert_eq!(op, spec.id.as_str()),
            other => panic!("{} did not refuse: {other:?}", spec.id.as_str()),
        }
    }
}

#[test]
fn off_runtime_runs_work_on_another_thread_and_reports_a_panic() {
    let here = std::thread::current().id();
    let there = block_on(spot_migration_ops::off_runtime(|| {
        std::thread::current().id()
    }))
    .unwrap();
    assert_ne!(here, there);
    assert_eq!(block_on(spot_migration_ops::off_runtime(|| 40 + 2)), Ok(42));
    let panicked = block_on(spot_migration_ops::off_runtime(|| -> u8 { panic!("boom") }));
    assert_eq!(panicked, Err(LifecycleError::Store));
}
