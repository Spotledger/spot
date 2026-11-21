//! The edge's production intake factory (WP-RTB-02.T12): the settings, the module's gating, the
//! adapters, and the PostgreSQL restore state. The database tests need `SPOT_TEST_PG_URL` and are
//! skipped, with a printed reason, when it is unset (`SPOT_REQUIRE_PG=1` makes that a failure).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::SiteCtx;
use spot_cli::services::node_role::{ActiveRole, Role};
use spot_cli::services::sync_node_intake::{
    self, PgState, RESTORE_INCOMPLETE, Settings, SiteClock,
};
use spot_core::{Clock, ManualClock};
use spot_events_contract::SourceId;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_sync_closure::{Bundle, BundleKind};
use spot_sync_node::HoldSet;
use spot_sync_node::{ParkedBundle, ParkingStore, RecordKey, RepairCause, RepairItem};
use spot_sync_node_host::IntakeFactory;
use spot_sync_node_host::intake::{STATEMENTS, StateStore, parked_first_positions};
use spot_sync_node_pg::{ApplyClock, Db, PgParkingStore};
use time::OffsetDateTime;

const POOL: usize = 6;
const TENANT: &str = "acme";

fn ctx_with_role(role: Option<Role>) -> SiteCtx<'static> {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let mut ctx = SiteCtx::new("site", Duration::from_secs(1), clock, BTreeMap::new(), true);
    if let Some(role) = role {
        ctx.provide(Arc::new(ActiveRole::new(role))).unwrap();
    }
    ctx
}

#[test]
fn defaults_are_the_library_defaults_and_zero_is_refused() {
    let bounds = Settings::default().bounds().unwrap();
    assert_eq!(bounds.max_skew, 0);
    assert_eq!(bounds.parking, spot_sync_node::ParkingLimits::default());
    assert_eq!(bounds.apply, spot_sync_node_pg::ApplyLimits::default());

    for bad in [
        Settings {
            parking_max_bundles: Some(0),
            ..Settings::default()
        },
        Settings {
            parking_max_age_secs: Some(0),
            ..Settings::default()
        },
        Settings {
            apply_deadline_secs: Some(0),
            ..Settings::default()
        },
        Settings {
            max_writes: Some(0),
            ..Settings::default()
        },
        Settings {
            parking_max_age_secs: Some(u64::MAX),
            ..Settings::default()
        },
    ] {
        assert!(bad.bounds().is_err(), "{bad:?}");
    }
}

#[test]
fn an_unknown_setting_is_refused() {
    let parsed: Result<Settings, _> = toml::from_str("parking_max_bundles = 5\nspeed = 3");
    assert!(parsed.is_err());
    let parsed: Settings = toml::from_str("parking_max_bundles = 5\nmax_skew = 1").unwrap();
    let bounds = parsed.bounds().unwrap();
    assert_eq!((bounds.parking.max_bundles, bounds.max_skew), (5, 1));
}

#[test]
fn the_module_runs_before_sync_node_and_after_the_role() {
    const { assert!(sync_node_intake::ORDER > spot_cli::services::node_role::ORDER) };
    const { assert!(sync_node_intake::ORDER < spot_cli::services::sync_node::ORDER) };
}

#[test]
fn only_an_edge_with_every_part_wired_provides_a_factory() {
    for role in [
        None,
        Some(Role::Standalone),
        Some(Role::Hub),
        Some(Role::Edge),
    ] {
        let mut ctx = ctx_with_role(role);
        sync_node_intake::register(&mut ctx).unwrap();
        assert!(
            ctx.port::<dyn IntakeFactory>().is_none(),
            "{role:?}: no factory without the ports of the apply path"
        );
    }
}

#[test]
fn the_apply_instant_is_the_injected_clock_in_microseconds() {
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(100),
    ));
    let site = SiteClock(clock.clone());
    assert_eq!(site.now_micros(), 100_000_000);
    clock.advance(Duration::from_millis(1_500));
    assert_eq!(site.now_micros(), 101_500_000);
}

#[test]
fn every_statement_names_its_tenant() {
    for statement in STATEMENTS {
        assert!(statement.contains("tenant = $1"), "{statement}");
    }
}

// ---------------------------------------------------------------------------------------------
// PostgreSQL.
// ---------------------------------------------------------------------------------------------

struct World {
    _rt: tokio::runtime::Runtime,
    _db: TestDb,
    bridge: Arc<Bridge>,
    db: Db,
}

fn world() -> Option<World> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let test_db = match rt.block_on(TestDb::create()) {
        Some(db) => db,
        None => {
            assert!(
                std::env::var("SPOT_REQUIRE_PG").as_deref() != Ok("1"),
                "SPOT_REQUIRE_PG=1 but no database is configured"
            );
            eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
            return None;
        }
    };
    let mut config: PgConfig = test_db.config();
    config.pool_max_size = POOL;
    let backend = PgBackend::connect_unchecked(config).expect("backend");
    let bridge = Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "sync_node_intake_test".to_owned(),
                workers: 2,
                max_in_flight: 3,
                queue_capacity: 16,
                default_deadline: Duration::from_secs(30),
                max_deadline: Duration::from_secs(60),
            },
            &backend,
        )
        .expect("bridge"),
    );
    let db = Db::new(Arc::clone(&bridge), POOL).expect("db");
    db.prepare().expect("prepare");
    Some(World {
        _rt: rt,
        _db: test_db,
        bridge,
        db,
    })
}

fn hub() -> SourceId {
    SourceId::new("head-office#hub").unwrap()
}

fn repair(first: u64) -> RepairItem {
    RepairItem {
        tenant: TENANT.to_owned(),
        first_position: first,
        missing: vec![RecordKey::new("Doc", "m")],
        cause: RepairCause::Expired,
    }
}

fn parked(first: u64) -> ParkedBundle {
    ParkedBundle {
        seq: first,
        tenant: TENANT.to_owned(),
        bundle: Bundle {
            kind: BundleKind::Root,
            records: Vec::new(),
            bytes: 0,
        },
        missing: BTreeSet::from([RecordKey::new("Doc", "m")]),
        parked_at_ms: 0,
        first_position: first,
        bytes: 0,
        serve: None,
    }
}

#[test]
fn an_empty_database_restores_to_nothing() {
    let Some(w) = world() else { return };
    let mut state = PgState::new(Arc::clone(&w.bridge), &w.db, &hub());
    let loaded = state.load(TENANT).unwrap();
    assert_eq!(loaded.applied_prefix, None);
    assert!(loaded.holds.is_none());
    assert!(loaded.parked_first_positions.is_empty());
    assert!(loaded.repair.is_empty());
}

#[test]
fn holds_repair_items_and_parked_bundles_come_back() {
    let Some(w) = world() else { return };
    let mut state = PgState::new(Arc::clone(&w.bridge), &w.db, &hub());
    state.save_holds(TENANT, &HoldSet::new()).unwrap();
    state.push_repair(&[repair(20), repair(25)]).unwrap();
    let mut parking = PgParkingStore::new(w.db.clone());
    parking.park(parked(30));
    parking.park(parked(30));
    assert!(parking.take_failure().is_none());

    let loaded = state.load(TENANT).unwrap();
    assert_eq!(loaded.holds, Some(HoldSet::new()));
    assert_eq!(
        loaded
            .repair
            .iter()
            .map(|r| r.first_position)
            .collect::<Vec<_>>(),
        [20, 25]
    );
    assert_eq!(loaded.parked_first_positions, [30, 30]);
    assert_eq!(
        parked_first_positions(&w.bridge, TENANT).unwrap(),
        [30, 30],
        "one entry per bundle"
    );
    // Another tenant sees none of it.
    let other = state.load("other").unwrap();
    assert!(other.repair.is_empty() && other.parked_first_positions.is_empty());
}

#[test]
fn a_repair_list_past_one_read_refuses_the_restore() {
    let Some(w) = world() else { return };
    let mut state = PgState::new(Arc::clone(&w.bridge), &w.db, &hub());
    let items: Vec<RepairItem> = (0..1_001).map(repair).collect();
    state.push_repair(&items).unwrap();
    let error = state.load(TENANT).unwrap_err();
    assert_eq!(error.code, RESTORE_INCOMPLETE);
}
