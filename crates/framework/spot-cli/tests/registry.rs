//! The services registry: order, ports, settings, open paths, and the generator's scanner.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::{get, post};
use serde::Deserialize;
use spot_cli::services::{
    Idle, Level, MemoryOpenAudit, OpenOutcome, OpenPath, OpenPaths, RateLimit, RegisterFuture,
    Registration, SiteCtx, State, register_all,
};
use spot_core::ManualClock;
use time::macros::datetime;

/// The build script's scanner, tested without a build.
#[path = "../build.rs"]
mod scanner;

trait Greeter: Send + Sync {
    fn hello(&self) -> &'static str;
}
struct English;
impl Greeter for English {
    fn hello(&self) -> &'static str {
        "hello"
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-02 10:00:00 UTC)))
}

fn ctx(clock: Arc<ManualClock>) -> SiteCtx<'static> {
    SiteCtx::new("acme", Duration::from_secs(1), clock, BTreeMap::new(), true)
}

// --- order ----------------------------------------------------------------------------------

fn record(ctx: &mut SiteCtx<'_>, name: &'static str) {
    ctx.add(Idle::new(name, Level::Optional, || {
        Box::pin(async { Ok(()) })
    }));
}
fn reg_a<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        record(ctx, "a");
        Ok(())
    })
}
fn reg_b<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        record(ctx, "b");
        Ok(())
    })
}
fn reg_c<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        record(ctx, "c");
        Ok(())
    })
}

#[tokio::test]
async fn modules_start_by_order_then_name() {
    let table = [
        Registration {
            name: "c",
            order: 1000,
            register: reg_c,
        },
        Registration {
            name: "b",
            order: 1000,
            register: reg_b,
        },
        Registration {
            name: "a",
            order: 1100,
            register: reg_a,
        },
    ];
    let mut ctx = ctx(clock());
    register_all(&mut ctx, &[&table]).await.unwrap();
    let assembled = ctx.finish(Router::new());
    let assembly = assembled.assembly.unwrap();
    let names: Vec<_> = assembly.services().iter().map(|s| s.name()).collect();
    assert_eq!(names, ["b", "c", "a"]);
}

#[tokio::test]
async fn a_duplicate_module_name_is_refused() {
    let table = [
        Registration {
            name: "a",
            order: 1000,
            register: reg_a,
        },
        Registration {
            name: "a",
            order: 1100,
            register: reg_b,
        },
    ];
    let err = register_all(&mut ctx(clock()), &[&table])
        .await
        .unwrap_err();
    assert!(err.0.contains("`a` is registered twice"), "{err}");
}

#[tokio::test]
async fn a_failing_register_stops_the_site_and_names_the_module() {
    fn bad<'a, 'b>(_: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
        Box::pin(async { Err(spot_cli::Error("boom".into())) })
    }
    let table = [
        Registration {
            name: "bad",
            order: 1000,
            register: bad,
        },
        Registration {
            name: "later",
            order: 1100,
            register: reg_a,
        },
    ];
    let mut ctx = ctx(clock());
    let err = register_all(&mut ctx, &[&table]).await.unwrap_err();
    assert!(err.0.contains("`bad`") && err.0.contains("boom"), "{err}");
    // The later module never ran.
    assert!(
        ctx.finish(Router::new())
            .assembly
            .unwrap()
            .services()
            .is_empty()
    );
}

// --- ports ----------------------------------------------------------------------------------

fn provider<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        let port: Arc<dyn Greeter> = Arc::new(English);
        ctx.provide(port)
    })
}
fn second_provider<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        let port: Arc<dyn Greeter> = Arc::new(English);
        ctx.provide(port)
    })
}
fn consumer<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        let greeter = ctx.require_port::<dyn Greeter>()?;
        assert_eq!(greeter.hello(), "hello");
        Ok(())
    })
}

#[tokio::test]
async fn a_port_reaches_a_later_module() {
    let table = [
        Registration {
            name: "provider",
            order: 1000,
            register: provider,
        },
        Registration {
            name: "consumer",
            order: 1100,
            register: consumer,
        },
    ];
    register_all(&mut ctx(clock()), &[&table]).await.unwrap();
}

#[tokio::test]
async fn providing_a_port_twice_names_both_modules() {
    let table = [
        Registration {
            name: "first",
            order: 1000,
            register: provider,
        },
        Registration {
            name: "second",
            order: 1100,
            register: second_provider,
        },
    ];
    let err = register_all(&mut ctx(clock()), &[&table])
        .await
        .unwrap_err();
    assert!(
        err.0.contains("`first`") && err.0.contains("`second`") && err.0.contains("Greeter"),
        "{err}"
    );
}

#[tokio::test]
async fn a_missing_required_port_names_the_module_and_the_port() {
    // The consumer starts before the provider: the provider's ORDER must be smaller.
    let table = [
        Registration {
            name: "consumer",
            order: 1000,
            register: consumer,
        },
        Registration {
            name: "provider",
            order: 1100,
            register: provider,
        },
    ];
    let err = register_all(&mut ctx(clock()), &[&table])
        .await
        .unwrap_err();
    assert!(
        err.0.contains("`consumer`") && err.0.contains("Greeter"),
        "{err}"
    );
}

#[tokio::test]
async fn a_finaliser_runs_after_every_module_and_keeps_its_own_order() {
    fn early<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
        Box::pin(async move {
            ctx.finally(|ctx| {
                record(ctx, "early");
                Ok(())
            });
            Ok(())
        })
    }
    let table = [
        Registration {
            name: "early",
            order: 1000,
            register: early,
        },
        Registration {
            name: "mid",
            order: 1100,
            register: reg_a,
        },
    ];
    let mut ctx = ctx(clock());
    register_all(&mut ctx, &[&table]).await.unwrap();
    let assembly = ctx.finish(Router::new()).assembly.unwrap();
    let names: Vec<_> = assembly.services().iter().map(|s| s.name()).collect();
    assert_eq!(names, ["early", "a"]);
}

// --- settings -------------------------------------------------------------------------------

#[derive(Deserialize, Default, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Probe {
    #[serde(default)]
    depth: u16,
    #[serde(default)]
    name: String,
}

fn with_settings(toml: &str) -> SiteCtx<'static> {
    let table: toml::Table = toml.parse().unwrap();
    SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        table.into_iter().collect(),
        true,
    )
}

#[test]
fn settings_are_typed_and_absence_defaults() {
    let ctx = with_settings("[probe]\ndepth = 3\n");
    assert_eq!(ctx.settings::<Probe>("probe").unwrap().depth, 3);
    assert_eq!(
        ctx.settings_or_default::<Probe>("absent").unwrap(),
        Probe::default()
    );
    assert!(ctx.settings::<Probe>("absent").is_err());
}

#[test]
fn settings_errors_name_the_table_and_key_and_never_a_value() {
    let ctx = with_settings("[probe]\ndepth = \"hunter2-secret\"\n");
    let err = ctx.settings::<Probe>("probe").unwrap_err().0;
    assert!(err.contains("services.probe.depth"), "{err}");
    assert!(!err.contains("hunter2"), "{err}");
    let ctx = with_settings("[probe]\nnope = \"hunter2-secret\"\n");
    let err = ctx.settings::<Probe>("probe").unwrap_err().0;
    assert!(
        err.contains("services.probe") && err.contains("nope"),
        "{err}"
    );
    assert!(!err.contains("hunter2"), "{err}");
}

// --- open paths -----------------------------------------------------------------------------

fn hook() -> OpenPath {
    OpenPath {
        method: Method::POST,
        pattern: "/api/v1/process/hook/{trigger}",
        limit: RateLimit { per_minute: 2 },
    }
}

fn open_a<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move { ctx.open_paths(&[hook()]) })
}
fn open_b<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        let mut same = hook();
        // The parameter's name does not make it a different route.
        same.pattern = "/api/v1/process/hook/{other}";
        ctx.open_paths(&[same])
    })
}

fn request(method: Method, path: &str) -> Request {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

async fn open_site(
    clock: Arc<ManualClock>,
    audit: Arc<MemoryOpenAudit>,
) -> (Arc<OpenPaths>, Router) {
    let table = [Registration {
        name: "hooks",
        order: 1000,
        register: open_a,
    }];
    let mut ctx = SiteCtx::new("acme", Duration::from_secs(1), clock, BTreeMap::new(), true)
        .with_open_audit(audit);
    register_all(&mut ctx, &[&table]).await.unwrap();
    let router = Router::new()
        .route("/api/v1/process/hook/{trigger}", post(|| async { "ran" }))
        .route("/api/v1/private", get(|| async { "secret" }));
    let assembled = ctx.finish(router);
    (assembled.open, assembled.router)
}

#[tokio::test]
async fn only_the_declared_method_and_pattern_are_open() {
    let audit = Arc::new(MemoryOpenAudit::default());
    let (open, router) = open_site(clock(), audit).await;
    // Undeclared paths and other methods are handed back: the host then authenticates first.
    for (method, path) in [
        (Method::GET, "/api/v1/private"),
        (Method::GET, "/api/v1/process/hook/x"),
        (Method::POST, "/api/v1/process/hook"),
        (Method::POST, "/api/v1/process/hook/a/b"),
        (Method::POST, "/api/v1/process/hook/"),
        (Method::POST, "/api/v1/process/hooks/x"),
    ] {
        assert!(
            open.serve(request(method.clone(), path), &router)
                .await
                .is_err(),
            "{method} {path} must stay behind authentication"
        );
    }
    let response = open
        .serve(request(Method::POST, "/api/v1/process/hook/abc"), &router)
        .await
        .map_err(|_| "not open")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_rate_limit_refuses_with_429_and_resets_each_minute() {
    let clock = clock();
    let audit = Arc::new(MemoryOpenAudit::default());
    let (open, router) = open_site(clock.clone(), audit.clone()).await;
    let call = |path: &'static str| {
        let open = open.clone();
        let router = router.clone();
        async move {
            open.serve(request(Method::POST, path), &router)
                .await
                .map_err(|_| "not open")
                .unwrap()
                .status()
        }
    };
    assert_eq!(call("/api/v1/process/hook/one").await, StatusCode::OK);
    assert_eq!(call("/api/v1/process/hook/two").await, StatusCode::OK);
    assert_eq!(
        call("/api/v1/process/hook/three").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    clock.advance(Duration::from_secs(60));
    assert_eq!(call("/api/v1/process/hook/four").await, StatusCode::OK);
    // One audit record per request: the declared pattern and the outcome, never the concrete
    // path (a parameter can be a secret).
    let records = audit.records();
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].outcome, OpenOutcome::Served(200));
    assert_eq!(records[2].outcome, OpenOutcome::RateLimited);
    for r in &records {
        assert_eq!(r.pattern, "/api/v1/process/hook/{trigger}");
        assert_eq!(r.module, "hooks");
        assert_eq!(r.site, "acme");
        assert!(!format!("{r:?}").contains("three"));
    }
}

#[tokio::test]
async fn a_duplicate_declaration_fails_the_start_and_names_both_modules() {
    let table = [
        Registration {
            name: "one",
            order: 1000,
            register: open_a,
        },
        Registration {
            name: "two",
            order: 1100,
            register: open_b,
        },
    ];
    let err = register_all(&mut ctx(clock()), &[&table])
        .await
        .unwrap_err();
    assert!(err.0.contains("`one`") && err.0.contains("`two`"), "{err}");
}

#[test]
fn malformed_reserved_and_unlimited_declarations_are_refused() {
    let mut ctx = ctx(clock());
    let bad = |pattern: &'static str, per_minute: u32| OpenPath {
        method: Method::POST,
        pattern,
        limit: RateLimit { per_minute },
    };
    assert!(ctx.open_paths(&[bad("/x/{a}/{b}", 5)]).is_err());
    assert!(ctx.open_paths(&[bad("/x/*", 5)]).is_err());
    assert!(ctx.open_paths(&[bad("/x/y", 0)]).is_err());
    assert!(ctx.open_paths(&[bad("/api/v1/auth/login", 5)]).is_err());
    assert!(ctx.open_paths(&[bad("/metrics", 5)]).is_err());
    assert!(ctx.open_paths(&[bad("/api/v1/realtime", 5)]).is_err());
}

#[tokio::test]
async fn open_paths_are_listed_in_health() {
    let table = [Registration {
        name: "hooks",
        order: 1000,
        register: open_a,
    }];
    let mut ctx = ctx(clock());
    register_all(&mut ctx, &[&table]).await.unwrap();
    let assembly = ctx.finish(Router::new()).assembly.unwrap();
    assert_eq!(
        assembly.health().open_paths(),
        ["POST /api/v1/process/hook/{trigger} (hooks)"]
    );
}

// --- the start-up check of a registered Idle service lands in Health -------------------------

fn idle_fails<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        ctx.add(Idle::new("probe", Level::Optional, || {
            Box::pin(async { Err("nothing to probe".to_owned()) })
        }));
        Ok(())
    })
}

#[tokio::test]
async fn a_registered_idle_service_check_lands_in_health() {
    let table = [Registration {
        name: "probe",
        order: 1000,
        register: idle_fails,
    }];
    let mut ctx = ctx(clock());
    register_all(&mut ctx, &[&table]).await.unwrap();
    let mut assembly = ctx.finish(Router::new()).assembly.unwrap();
    assembly.start().await.unwrap();
    let states = assembly.health().states();
    assert_eq!(
        states["probe"],
        (Level::Optional, State::Stopped("nothing to probe".into()))
    );
    assembly.shutdown().await;
}

// --- a reload keeps the running services ------------------------------------------------------

#[tokio::test]
async fn a_reload_build_adds_no_service_but_keeps_routes_and_open_paths() {
    let table = [Registration {
        name: "hooks",
        order: 1000,
        register: open_a,
    }];
    let mut ctx = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        false,
    );
    register_all(&mut ctx, &[&table]).await.unwrap();
    let assembled = ctx.finish(Router::new());
    assert!(assembled.assembly.is_none());
    assert_eq!(assembled.open.listing().len(), 1);
}

// --- the generator's scanner ------------------------------------------------------------------

#[test]
fn the_scanner_reads_order_and_register_at_column_zero() {
    let helper = "impl Mail {\n    pub fn register(&self) {}\n}\n";
    assert_eq!(scanner::inspect("helper.rs", helper), Ok((false, false)));
    let sync = "pub const ORDER: u32 = 1000;\npub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> { Ok(()) }\n";
    assert_eq!(scanner::inspect("s.rs", sync), Ok((true, false)));
    let asynch = "pub const ORDER: u32 = 1000;\npub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> { Ok(()) }\n";
    assert_eq!(scanner::inspect("a.rs", asynch), Ok((true, true)));
    assert!(scanner::inspect("x.rs", "pub fn register(ctx: &mut SiteCtx<'_>) {}").is_err());
    assert!(scanner::inspect("x.rs", "pub const ORDER: u32 = 1;").is_err());
}

#[test]
fn a_bad_file_name_fails_the_scan_with_that_name() {
    let dir = std::env::temp_dir().join(format!("spot-registry-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mod.rs"), "").unwrap();
    std::fs::write(dir.join("good_one.rs"), "// helper\n").unwrap();
    let ok = scanner::scan(&dir).unwrap();
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].name, "good_one");
    std::fs::write(dir.join("Bad-Name.rs"), "").unwrap();
    let err = scanner::scan(&dir).unwrap_err();
    assert!(err.contains("Bad-Name.rs"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_generated_source_uses_forward_slashes_and_lists_registering_modules() {
    let modules = vec![
        scanner::Module {
            name: "a".into(),
            path: "src/services/a.rs".into(),
            registers: true,
            is_async: true,
        },
        scanner::Module {
            name: "b".into(),
            path: "src/services/b.rs".into(),
            registers: false,
            is_async: false,
        },
    ];
    let source = scanner::generate("C:\\work\\spot-cli", &modules);
    assert!(source.contains("#[path = \"C:/work/spot-cli/src/services/a.rs\"]"));
    assert!(source.contains("pub mod b;"));
    assert!(source.contains("name: \"a\""));
    assert!(!source.contains("name: \"b\""));
}
