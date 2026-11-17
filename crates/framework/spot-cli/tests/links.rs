//! The hosted link service: off by default, refuses to start half-wired, and every operation is
//! behind the `link.manage` right. No PostgreSQL is needed: the store's pool is built lazily and a
//! denied operation never reaches it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::SiteCtx;
use spot_cli::services::links::{
    LinkHost, LinksPort, LinksPorts, ORDER, Settings, parse_tenant, register,
};
use spot_core::SystemClock;
use spot_permissions::Principal;
use spot_tenancy::TenantId;
use spot_tenant_link::{AuditSink, Result as LinkResult, StepUpGate, StepUpProof};
use spot_tenant_link_pg::{Caller, DenyAll, OpError, PgLinkStore};

fn ctx(table: Option<&str>, assembling: bool) -> SiteCtx<'static> {
    let mut settings = BTreeMap::new();
    if let Some(text) = table {
        settings.insert("links".to_owned(), text.parse::<toml::Value>().unwrap());
    }
    SiteCtx::new(
        "site",
        Duration::from_secs(1),
        Arc::new(SystemClock::new()),
        settings,
        assembling,
    )
}

fn tenant_text() -> String {
    TenantId::new_v7(1_800_000_000_000, [7u8; 10]).to_string()
}

#[test]
fn it_is_off_by_default_and_registers_nothing() {
    let mut c = ctx(None, true);
    register(&mut c).unwrap();
    assert!(c.port::<dyn LinksPort>().is_none());
    let mut c = ctx(Some("enabled = false"), true);
    register(&mut c).unwrap();
    assert!(c.port::<dyn LinksPort>().is_none());
    assert_eq!(ORDER, 1400);
}

#[test]
fn a_reload_build_keeps_the_running_service() {
    let table = format!("enabled = true\ntenant = \"{}\"", tenant_text());
    let mut c = ctx(Some(&table), false);
    register(&mut c).unwrap();
    assert!(c.port::<dyn LinksPort>().is_none());
}

#[test]
fn enabled_without_the_ports_refuses_to_start_and_names_them() {
    let table = format!("enabled = true\ntenant = \"{}\"", tenant_text());
    let mut c = ctx(Some(&table), true);
    let error = register(&mut c).unwrap_err().to_string();
    assert!(error.contains("LinksPorts"), "{error}");
}

#[test]
fn the_tenant_must_be_a_uuid_v7_and_unknown_keys_are_refused() {
    let mut c = ctx(Some("enabled = true\ntenant = \"not-a-uuid\""), true);
    assert!(register(&mut c).unwrap_err().to_string().contains("UUID"));
    let v4 = "enabled = true\ntenant = \"67e55044-10b1-426f-9247-bb680e5fe0c8\"";
    let mut c = ctx(Some(v4), true);
    assert!(register(&mut c).unwrap_err().to_string().contains("UUIDv7"));
    let mut c = ctx(Some("enabled = true\nsurprise = 1"), true);
    assert!(register(&mut c).is_err());
    let settings = Settings {
        enabled: true,
        tenant: tenant_text(),
    };
    assert!(parse_tenant(&settings).is_ok());
}

struct Sink;

impl AuditSink for Sink {
    fn append(&self, _record: spot_audit_chain::AuditRecord) -> LinkResult<u64> {
        Ok(1)
    }
}

struct Gate;

impl StepUpGate for Gate {
    fn confirm(&self, _actor: &str, _proof: &StepUpProof, _at: u64) -> LinkResult<()> {
        Ok(())
    }
}

#[test]
fn every_operation_is_behind_the_link_manage_right() {
    let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
    let mut pg = deadpool_postgres::Config::new();
    pg.url = Some("postgres://nobody@127.0.0.1:1/none".to_owned());
    let pool = pg
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap();
    let tenant = TenantId::new_v7(1_800_000_000_000, [7u8; 10]);
    let store = Arc::new(PgLinkStore::new(pool, runtime, tenant));
    let host = LinkHost::new(
        store,
        LinksPorts {
            audit: Arc::new(Sink),
            gate: Arc::new(Gate),
            permissions: Arc::new(DenyAll),
        },
    );
    let caller = Caller::new(Principal::user("alice", Vec::new()));
    match host.list(&caller, 1) {
        Err(OpError::Denied { operation }) => assert_eq!(operation, "list"),
        other => panic!("expected a denial, got {other:?}"),
    }
}
