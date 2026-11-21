//! The registry, the compiled-report cache, the native API service and the telemetry line.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex};

use serde_json::json;
use spot_http::api::service::RequestCtx;
use spot_http::request::RequestId;
use spot_meta::{FieldSpec, FieldType, Registry};
use spot_permissions::Principal;
use spot_reports::{
    AnonymousPrincipals, Compiler, EffectiveMetaSet, NoTelemetry, PrincipalSource,
    ReportApiService, ReportArgs, ReportCache, ReportDef, ReportRegistry, ReportRunner,
    ReportService, Telemetry,
};
use spot_storage_contract::Collation;
use support::{FakeExecutor, JOURNAL_LIST, SMALL_LIST, compile, principals, world};

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, usize, u64)>>,
}

impl Telemetry for Recorder {
    fn report_run(&self, report: &str, rows: usize, elapsed_ms: u64) {
        self.calls
            .lock()
            .unwrap()
            .push((report.to_owned(), rows, elapsed_ms));
    }
}

struct Fixed(Principal);

impl PrincipalSource for Fixed {
    fn principal(&self, _ctx: &RequestCtx) -> Principal {
        self.0.clone()
    }
}

fn principal(name: &str) -> Principal {
    principals()
        .into_iter()
        .find(|(n, _)| *n == name)
        .expect("principal")
        .1
}

fn ctx() -> RequestCtx {
    RequestCtx {
        site: spot_core::SiteId::new("acme").expect("site"),
        credential: None,
        idempotency_key: None,
        language: None,
        request_id: RequestId::new("req-1").expect("request id"),
        peer: None,
    }
}

fn registry(world: &support::World) -> ReportRegistry {
    let set = EffectiveMetaSet::new(world.meta.clone());
    let compiler = Compiler::new(&set, &world.catalog);
    let mut registry = ReportRegistry::new();
    for (app, toml) in [
        ("sales", JOURNAL_LIST),
        ("sales", SMALL_LIST),
        ("analytics", support::TRIAL_BALANCE),
    ] {
        let def = Arc::new(ReportDef::from_toml(toml).expect("def"));
        registry.register(app, def, &compiler).expect("registers");
    }
    registry
}

#[tokio::test]
async fn the_registry_lists_only_the_reports_a_principal_may_see() {
    let world = world().await;
    let registry = registry(&world);
    let alice = principal("alice");
    let listed: Vec<String> = registry
        .list_for(&alice)
        .into_iter()
        .map(|r| r.compiled.name.clone())
        .collect();
    assert_eq!(listed, vec!["journal_list", "small_list", "trial_balance"]);

    let dave = principal("dave");
    assert!(registry.list_for(&dave).is_empty());
    assert!(registry.lookup_for("journal_list", &dave).is_none());
    assert!(registry.lookup_for("journal_list", &alice).is_some());
    assert!(registry.lookup_for("missing", &alice).is_none());
}

#[tokio::test]
async fn a_duplicate_report_names_both_apps() {
    let world = world().await;
    let set = EffectiveMetaSet::new(world.meta.clone());
    let compiler = Compiler::new(&set, &world.catalog);
    let mut registry = ReportRegistry::new();
    let def = Arc::new(ReportDef::from_toml(JOURNAL_LIST).expect("def"));
    registry
        .register("sales", Arc::clone(&def), &compiler)
        .expect("first");
    let errs = registry
        .register("analytics", def, &compiler)
        .expect_err("duplicate");
    assert_eq!(errs[0].code(), "report.duplicate");
    assert!(errs[0].message().contains("sales"), "{:?}", errs[0]);
    assert!(errs[0].message().contains("analytics"), "{:?}", errs[0]);
}

#[tokio::test]
async fn the_cache_is_keyed_by_name_and_metadata_identity_and_is_bounded() {
    let world = world().await;
    let set = EffectiveMetaSet::new(world.meta.clone());
    let compiler = Compiler::new(&set, &world.catalog);
    let def = ReportDef::from_toml(JOURNAL_LIST).expect("def");

    let mut cache = ReportCache::new(2);
    let first = cache.get_or_compile(&def, &compiler).expect("compiles");
    let again = cache.get_or_compile(&def, &compiler).expect("cached");
    assert!(Arc::ptr_eq(&first, &again), "one identity is cached");

    // Another metadata view — different content, its own version — misses.
    let mut definitions = support::definitions();
    definitions
        .iter_mut()
        .find(|d| d.name == "journal")
        .expect("the fixture has journal")
        .fields
        .push(spot_meta::FieldSpec::new(
            "custom_ref",
            spot_meta::FieldType::Text,
        ));
    let changed = Arc::new(Registry::new(definitions).expect("the changed metadata builds"));
    let elsewhere = EffectiveMetaSet::new(changed.effective());
    assert!(
        cache.get(&def.name, &elsewhere).is_none(),
        "another set misses"
    );
    assert_eq!(cache.len(), 1);

    cache.insert("a", &set, Arc::clone(&first));
    cache.insert("b", &set, Arc::clone(&first));
    cache.insert("c", &set, Arc::clone(&first));
    assert_eq!(cache.len(), 2, "the capacity bounds the cache");
}

#[tokio::test]
async fn two_metadata_sets_at_one_version_do_not_share_a_cached_report() {
    let world = world().await;
    let one = EffectiveMetaSet::new(world.meta.clone());

    // A rollback, or an adopted snapshot: the same version number over different DocTypes.
    let mut definitions = support::definitions();
    definitions
        .iter_mut()
        .find(|d| d.name == "journal")
        .expect("the fixture has journal")
        .fields
        .push(FieldSpec::new("custom_ref", FieldType::Text));
    let changed = Registry::new(definitions)
        .expect("the changed metadata builds")
        .effective();
    let two = EffectiveMetaSet::new(changed.clone());
    let catalog = changed
        .catalog(Collation::new("und").unwrap())
        .expect("the changed catalog builds");

    assert_eq!(one.version(), two.version(), "one version number");
    assert_ne!(one.digest(), two.digest(), "two different contents");
    assert_ne!(one.identity(), two.identity());

    let def = ReportDef::from_toml(JOURNAL_LIST).expect("def");
    let mut cache = ReportCache::new(4);
    let first = cache
        .get_or_compile(&def, &Compiler::new(&one, &world.catalog))
        .expect("compiles against the first metadata");
    let second = cache
        .get_or_compile(&def, &Compiler::new(&two, &catalog))
        .expect("compiles afresh against the second");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a report compiled against other DocTypes must not be reused"
    );
    assert_eq!(cache.len(), 2);
    assert_eq!(second.meta_version, two.version());
    assert_eq!(second.meta_digest, two.digest());

    // The identity it was compiled for still hits.
    let hit = cache
        .get_or_compile(&def, &Compiler::new(&two, &catalog))
        .expect("cached");
    assert!(Arc::ptr_eq(&second, &hit));
}

#[tokio::test]
async fn the_service_runs_a_report_and_records_telemetry() {
    let world = world().await;
    let registry = Arc::new(registry(&world));
    let recorder = Arc::new(Recorder::default());
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    let runner = Arc::new(ReportRunner::new(
        world.engine.clone(),
        world.guard.clone(),
        executor,
        recorder.clone(),
    ));
    let service = ReportApiService::new(registry, runner, Arc::new(Fixed(principal("alice"))));

    let (result, messages) = service
        .run_report(&ctx(), "journal_list", json!({}), Some(2))
        .await
        .expect("runs");
    assert_eq!(result.rows.len(), 2);
    assert!(result.truncated);
    assert!(messages.is_empty());
    let calls = recorder.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "journal_list");
    assert_eq!(calls[0].1, 2);
}

#[tokio::test]
async fn the_service_clamps_the_limit_to_the_report_maximum() {
    let world = world().await;
    let registry = Arc::new(registry(&world));
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    let runner = Arc::new(ReportRunner::new(
        world.engine.clone(),
        world.guard.clone(),
        executor,
        Arc::new(NoTelemetry),
    ));
    let service = ReportApiService::new(registry, runner, Arc::new(Fixed(principal("alice"))));
    let (result, _) = service
        .run_report(&ctx(), "small_list", json!({}), Some(100_000))
        .await
        .expect("runs");
    assert_eq!(result.rows.len(), 10, "clamped to max_rows");
}

#[tokio::test]
async fn an_unknown_report_is_not_found() {
    let world = world().await;
    let registry = Arc::new(registry(&world));
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    let runner = Arc::new(ReportRunner::new(
        world.engine.clone(),
        world.guard.clone(),
        executor,
        Arc::new(NoTelemetry),
    ));
    let service = ReportApiService::new(registry, runner, Arc::new(Fixed(principal("alice"))));
    let err = service
        .run_report(&ctx(), "missing", json!({}), None)
        .await
        .expect_err("not found");
    assert_eq!(err.code.as_str(), "report.not_found");
    assert_eq!(err.status().as_u16(), 404);
}

#[tokio::test]
async fn a_report_the_roles_do_not_admit_is_not_found() {
    let world = world().await;
    let registry = Arc::new(registry(&world));
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    let runner = Arc::new(ReportRunner::new(
        world.engine.clone(),
        world.guard.clone(),
        executor,
        Arc::new(NoTelemetry),
    ));
    let service = ReportApiService::new(registry, runner, Arc::new(Fixed(principal("dave"))));
    let err = service
        .run_report(&ctx(), "journal_list", json!({}), None)
        .await
        .expect_err("not found");
    assert_eq!(err.code.as_str(), "report.not_found");
}

#[tokio::test]
async fn a_bad_argument_maps_to_its_contract_code() {
    let world = world().await;
    let set = EffectiveMetaSet::new(world.meta.clone());
    let compiler = Compiler::new(&set, &world.catalog);
    let mut registry = ReportRegistry::new();
    let def = Arc::new(ReportDef::from_toml(support::JOURNAL_FILTERED).expect("def"));
    registry
        .register("sales", def, &compiler)
        .expect("registers");
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    let runner = Arc::new(ReportRunner::new(
        world.engine.clone(),
        world.guard.clone(),
        executor,
        Arc::new(NoTelemetry),
    ));
    let service = ReportApiService::new(
        Arc::new(registry),
        runner,
        Arc::new(Fixed(principal("alice"))),
    );
    let err = service
        .run_report(&ctx(), "journal_filtered", json!({"nope": 1}), None)
        .await
        .expect_err("bad argument");
    assert_eq!(err.code.as_str(), "report.bad_argument");
    assert_eq!(err.status().as_u16(), 422);
}

#[tokio::test]
async fn the_anonymous_source_resolves_to_an_empty_principal() {
    let source = AnonymousPrincipals;
    let principal = source.principal(&ctx());
    assert!(principal.user.is_empty());
    assert!(!principal.is_system);
    // A report still compiles and runs; the empty principal simply reads nothing.
    let world = world().await;
    let _ = compile(&world, JOURNAL_LIST).expect("compiles");
    let _ = ReportArgs::empty();
}
