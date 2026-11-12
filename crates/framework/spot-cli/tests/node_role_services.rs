//! The role-conditioned service sets (WP-RTB-03.T6): which modules the registry runs for each
//! node role, proved over test-only modules with real `register_all_with_roles` and a real
//! `Services` assembly. No database: the pure registry tests run everywhere; the hosted checks
//! over a real `spot serve` live in `node_role_host.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use spot_cli::services::node_role::{self, ActiveRole, Role};
use spot_cli::services::{
    Idle, Level, RegisterFuture, Registration, RoleCondition, Services, SiteCtx,
    register_all_with_roles,
};
use spot_core::{Clock, ManualClock};
use spot_http::health::ReadinessCheck;
use time::OffsetDateTime;

const ROLE_TABLE: &str = "fake_role";

fn ctx(role: &str, clock: Arc<dyn Clock>) -> SiteCtx<'static> {
    let mut settings = BTreeMap::new();
    let mut table = toml::map::Map::new();
    table.insert("role".to_owned(), toml::Value::String(role.to_owned()));
    settings.insert(ROLE_TABLE.to_owned(), toml::Value::Table(table));
    SiteCtx::new("site", Duration::from_secs(1), clock, settings, true)
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::UNIX_EPOCH + time::Duration::hours(100),
    ))
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RoleSetting {
    role: String,
}

/// Stands in for `node_role` at its `ORDER`: provides the role the way the real module does.
fn fake_role<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    let done = (|| {
        let setting: RoleSetting = ctx.settings_or_default(ROLE_TABLE)?;
        let role = Role::parse(&setting.role).map_err(|e| spot_cli::Error(e.to_string()))?;
        ctx.provide(Arc::new(ActiveRole::new(role)))
    })();
    Box::pin(async move { done })
}

fn idle(name: &'static str, level: Level) -> Arc<Idle> {
    Idle::new(name, level, || Box::pin(async { Ok(()) }))
}

/// No `ROLES`: runs in every role.
fn local_stack<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    ctx.add(idle("local_stack", Level::Required));
    Box::pin(async { Ok(()) })
}

thread_local! {
    /// Per test thread: `#[tokio::test]` runs each test on its own thread, so parallel tests
    /// that register `hub_only` never move another test's count.
    static HUB_CALLS: Cell<usize> = const { Cell::new(0) };
}

/// `ROLES = ["hub"]`: counts its `register` calls.
fn hub_only<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    HUB_CALLS.with(|c| c.set(c.get() + 1));
    ctx.add(idle("hub_only", Level::Required));
    Box::pin(async { Ok(()) })
}

/// `ROLES = ["edge"]`: the sync link, offline for three hours.
fn edge_link<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    let now = ctx.clock().now_utc();
    let detail = node_role::offline_detail(now - time::Duration::hours(3), now);
    ctx.add(Idle::new("sync_node", Level::Optional, move || {
        let detail = detail.clone();
        Box::pin(async move { Err(detail) })
    }));
    Box::pin(async { Ok(()) })
}

const TABLE: &[Registration] = &[
    Registration {
        name: "fake_role",
        order: 1005,
        register: fake_role,
    },
    Registration {
        name: "local_stack",
        order: 1010,
        register: local_stack,
    },
    Registration {
        name: "hub_only",
        order: 1020,
        register: hub_only,
    },
    Registration {
        name: "edge_link",
        order: 1030,
        register: edge_link,
    },
];

const CONDITIONS: &[RoleCondition] = &[
    RoleCondition {
        name: "hub_only",
        roles: &["hub"],
    },
    RoleCondition {
        name: "edge_link",
        roles: &["edge"],
    },
];

async fn names_for(role: &str) -> Vec<&'static str> {
    let mut c = ctx(role, clock());
    register_all_with_roles(&mut c, &[TABLE], CONDITIONS)
        .await
        .unwrap();
    let assembled = c.finish(Router::new());
    let assembly = assembled.assembly.unwrap();
    assembly.services().iter().map(|s| s.name()).collect()
}

#[tokio::test]
async fn each_role_lists_exactly_its_set() {
    assert_eq!(names_for("standalone").await, ["local_stack"]);
    assert_eq!(names_for("hub").await, ["local_stack", "hub_only"]);
    assert_eq!(names_for("edge").await, ["local_stack", "sync_node"]);
}

#[tokio::test]
async fn a_skipped_module_never_registers() {
    let before = HUB_CALLS.with(Cell::get);
    names_for("standalone").await;
    names_for("edge").await;
    assert_eq!(
        HUB_CALLS.with(Cell::get),
        before,
        "register ran on a skipped role"
    );
    names_for("hub").await;
    assert_eq!(HUB_CALLS.with(Cell::get), before + 1);
}

#[tokio::test]
async fn an_edge_with_an_unreachable_hub_is_ready_and_says_offline() {
    let mut c = ctx("edge", clock());
    register_all_with_roles(&mut c, &[TABLE], CONDITIONS)
        .await
        .unwrap();
    let mut assembly = c.finish(Router::new()).assembly.unwrap();
    assembly.start().await.unwrap();
    let result = assembly.health_check().check().await;
    assert!(
        result.ok,
        "an unreachable hub must not fail readiness: {}",
        result.detail
    );
    assert!(result.detail.contains("offline 3h"), "{}", result.detail);
    assert!(!result.detail.contains("http") && !result.detail.contains("sha256"));
    assembly.shutdown().await;
}

#[tokio::test]
async fn a_hub_and_a_standalone_are_ready_with_no_degradation() {
    for role in ["standalone", "hub"] {
        let mut c = ctx(role, clock());
        register_all_with_roles(&mut c, &[TABLE], CONDITIONS)
            .await
            .unwrap();
        let mut assembly: Services = c.finish(Router::new()).assembly.unwrap();
        assembly.start().await.unwrap();
        let result = assembly.health_check().check().await;
        assert!(
            result.ok && !result.detail.contains("offline"),
            "{role}: {}",
            result.detail
        );
        assembly.shutdown().await;
    }
}

#[tokio::test]
async fn an_unknown_role_starts_nothing() {
    let mut c = ctx("branch", clock());
    let err = register_all_with_roles(&mut c, &[TABLE], CONDITIONS)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("standalone, hub or edge"), "{err}");
    // A refusal builds no service set: nothing was added before the role module failed.
    assert!(
        c.finish(Router::new())
            .assembly
            .unwrap()
            .services()
            .is_empty()
    );
}

#[tokio::test]
async fn a_conditioned_module_before_the_role_is_known_is_an_error() {
    let early: &[Registration] = &[
        Registration {
            name: "hub_only",
            order: 1001,
            register: hub_only,
        },
        Registration {
            name: "fake_role",
            order: 1005,
            register: fake_role,
        },
    ];
    let mut c = ctx("hub", clock());
    let err = register_all_with_roles(&mut c, &[early], &CONDITIONS[..1])
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("starts before the site's role is known"),
        "{err}"
    );
}

#[tokio::test]
async fn a_condition_naming_no_role_or_no_module_is_an_error() {
    let bogus = [RoleCondition {
        name: "hub_only",
        roles: &["everything"],
    }];
    let mut c = ctx("hub", clock());
    let err = register_all_with_roles(&mut c, &[TABLE], &bogus)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not standalone, hub or edge"),
        "{err}"
    );
    let orphan = [RoleCondition {
        name: "ghost",
        roles: &["hub"],
    }];
    let mut c = ctx("hub", clock());
    let err = register_all_with_roles(&mut c, &[TABLE], &orphan)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no registration"), "{err}");
}

#[test]
fn the_table_is_consistent_for_each_role() {
    assert!(node_role::service_set(Role::Standalone).is_empty());
    for module in node_role::hub_only_modules() {
        assert!(!node_role::service_set(Role::Edge).contains(&module));
        assert!(!node_role::service_set(Role::Standalone).contains(&module));
    }
}
