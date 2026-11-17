//! A scratch database, a bridge on it, a store per tenant and the fixtures of a promotion.
//! Nothing reads a clock.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_custom_bundle::{
    Bundle, CaptureOptions, FakeSite, Limits, SectionRegistry, SiteReader, SiteWriter, capture_with,
};
use spot_environments::{EnvRef, Role};
use spot_migration_approval::{InMemoryApprovalStore, MemoryAuditSink};
use spot_migration_audit::{ApplyEvent, ApprovalEvent, FinishEvent, RehearsalEvent, RunId};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_promotion::ports::PromotionFuture;
use spot_promotion::{
    CustomBundleApplier, MemoryPromotionStore, OpenRun, Promotion, PromotionAudit, PromotionPolicy,
    PromotionService, PromotionStore, TargetSite,
};
use spot_promotion_pg::PgPromotionStore;
use spot_tenancy::TenantId;

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (and a printed `SKIPPED`) without `SPOT_TEST_PG_URL`.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

pub fn backend(fx: &Fixture, pool: usize) -> PgBackend {
    let mut config = fx.db.config();
    config.pool_max_size = pool;
    PgBackend::connect_unchecked(config).expect("backend")
}

pub fn bridge_config(name: &str, max_in_flight: usize) -> BridgeConfig {
    BridgeConfig {
        name: name.to_owned(),
        workers: 2,
        max_in_flight,
        queue_capacity: 64,
        default_deadline: Duration::from_secs(5),
        max_deadline: Duration::from_secs(20),
    }
}

/// A prepared store for `tenant` on a fresh bridge (a "node": one process) over the fixture's
/// database.
pub fn node(fx: &Fixture, name: &str, tenant: TenantId) -> PgPromotionStore {
    let backend = backend(fx, 8);
    let bridge = Arc::new(Bridge::for_backend(bridge_config(name, 6), &backend).expect("bridge"));
    let store = PgPromotionStore::new(bridge, &backend.pool(), tenant).expect("store");
    store.ensure_schema().expect("schema");
    store
}

/// A tenant id from a seed.
pub fn tenant(seed: u8) -> TenantId {
    TenantId::new_v7(1_700_000_000_000 + u64::from(seed), [seed; 10])
}

/// An environment address in set `acme`.
pub fn env(role: Role, tenant: TenantId) -> EnvRef {
    EnvRef {
        set: "acme".to_owned(),
        role,
        tenant,
    }
}

/// A site that belongs to one environment.
pub struct EnvSite {
    pub site: FakeSite,
    pub env: EnvRef,
}

impl TargetSite for EnvSite {
    fn environment(&self) -> EnvRef {
        self.env.clone()
    }
    fn as_reader(&self) -> &dyn SiteReader {
        &self.site
    }
    fn as_writer(&mut self) -> &mut dyn SiteWriter {
        &mut self.site
    }
}

/// A target with an app and no script: the script of [`bundle`] is an additive change.
pub fn target_site(tenant: TenantId) -> EnvSite {
    EnvSite {
        site: FakeSite::new().with_app("base", semver::Version::new(1, 0, 0)),
        env: env(Role::Test, tenant),
    }
}

/// The bundle a promotion carries: a site with one script, captured.
pub fn bundle() -> Bundle {
    let source = FakeSite::new()
        .with_app("base", semver::Version::new(1, 0, 0))
        .with_script(
            "party_guard",
            "base",
            "party.before_save",
            "export function run(doc: any) { return doc; }",
        );
    capture_with(
        &source,
        &CaptureOptions::new("site", "1.0.0", "user:req", "2026-10-20T00:00:00Z"),
        &SectionRegistry::new(),
        &Limits::default(),
    )
    .unwrap()
}

/// An audit port that records nothing.
pub struct NoAudit;

impl PromotionAudit for NoAudit {
    fn open<'a>(&'a self, _request: OpenRun<'a>) -> PromotionFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    fn approval<'a>(&'a self, _promotion: &'a RunId, _event: ApprovalEvent) -> PromotionFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    fn rehearsal<'a>(
        &'a self,
        _promotion: &'a RunId,
        _event: RehearsalEvent,
    ) -> PromotionFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    fn applied<'a>(&'a self, _promotion: &'a RunId, _event: ApplyEvent) -> PromotionFuture<'a> {
        Box::pin(async { Ok(()) })
    }
    fn finish<'a>(&'a self, _promotion: &'a RunId, _event: FinishEvent) -> PromotionFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

/// A promotion service over `store` (the default policy: a test target auto-approves).
pub fn service(store: Arc<dyn PromotionStore>) -> PromotionService {
    PromotionService::new(
        store,
        Arc::new(InMemoryApprovalStore::new()),
        Arc::new(MemoryAuditSink::new()),
        Arc::new(NoAudit),
        Arc::new(CustomBundleApplier::with_builtin_sections()),
        PromotionPolicy::default(),
    )
    .expect("service")
}

/// A requested (never planned) promotion with id `id` to the test environment of `to`, made by
/// a service over a throwaway memory store.
pub fn requested(id: &str, to: TenantId) -> Promotion {
    service(Arc::new(MemoryPromotionStore::new()))
        .request(
            id,
            &bundle(),
            env(Role::Dev, tenant(200)),
            env(Role::Test, to),
            "user:req",
            1_000,
        )
        .expect("request")
}
