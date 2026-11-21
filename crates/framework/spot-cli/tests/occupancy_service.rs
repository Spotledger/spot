//! The occupancy service on a real PostgreSQL, through the production [`OccupancyPort`]
//! (WP-STO-14.T3): the adapter over `spot-occupancy` with a test [`Identity`], the race cases of
//! ADR-087 section 6.3 at the scale the brief names, and the rules the adapter owns (holder
//! from the principal, rights, partition ownership, tenant isolation, nothing leaked in a
//! conflict, the replay of a repeated claim).
//!
//! Every test that needs the database skips (printing `SKIPPED:`) when `SPOT_TEST_PG_URL` is
//! unset or the server lacks `btree_gist`; `SPOT_REQUIRE_PG=1` turns a skip into a failure.
//! `SPOT_OCC_ATTEMPTS` sets the attempts of the long run (default 100000, the brief's 10^5).
//!
//! The over-HTTP twins of these cases are listed in `harness/rust/l3-http/src/cases/occupancy.rs`;
//! they wait for a fixture that carries the occupancy entity (see the task report).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_cli::services::occupancy::{
    ENTITY, Identity, OccupancyApi, PortSettings, Ready, Who, map_error, missing,
};
use spot_core::{Clock, ManualClock, SiteId};
use spot_http::api::ports::occupancy::{
    ClaimIn, HoldOut, HoldStateOut, IntervalStatusOut, OccupancyError, OccupancyPort,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Registry};
use spot_occupancy::{
    Holder, HomeEdges, HubOwnsAll, Limits, MemoryAudit, Occupancy, Owner, OwnerRoutes, Span,
    definition,
};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, Fields, Isolation, Name, RecordId,
    StorageBackend, Transaction, TxOptions, Value,
};
use time::{Duration, OffsetDateTime};

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

/// Tokens to principals: `staff-*` hold `occupancy_manage`, `consumer-*` are consumers.
struct TestIdentity {
    who: HashMap<String, Who>,
}

impl TestIdentity {
    fn new(tokens: &[(&str, Who)]) -> Arc<Self> {
        Arc::new(Self {
            who: tokens
                .iter()
                .map(|(t, w)| ((*t).to_owned(), w.clone()))
                .collect(),
        })
    }
}

impl Identity for TestIdentity {
    fn who<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<Who, ApiError>> {
        Box::pin(async move {
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.as_str())
                .ok_or_else(|| ApiError::from_code(Code::AuthRequired))?;
            self.who
                .get(token)
                .cloned()
                .ok_or_else(|| ApiError::from_code(Code::AuthRequired))
        })
    }
}

fn user(id: &str, manage: bool) -> Who {
    Who {
        holder: Holder::user(id).unwrap(),
        manage,
    }
}

fn consumer(id: &str) -> Who {
    Who {
        holder: Holder::consumer(id).unwrap(),
        manage: false,
    }
}

fn ctx(token: &str, key: Option<&str>) -> RequestCtx {
    RequestCtx {
        site: SiteId::new("occupancy-site").unwrap(),
        credential: Some(Credential::new(token)),
        idempotency_key: key.map(str::to_owned),
        language: None,
        request_id: RequestId::new("req-1").unwrap(),
        peer: None,
    }
}

struct Fx {
    _db: TestDb,
    backend: Arc<PgBackend>,
    catalog: Arc<Catalog>,
    clock: Arc<ManualClock>,
    units: Vec<RecordId>,
}

/// A scratch database with the occupancy catalog and the given rooms, or `None` (skip).
async fn fixture_with(units: Vec<RecordId>) -> Option<Fx> {
    let db = TestDb::create().await?;
    let missing_extensions = db.create_extensions(&["btree_gist"]).await;
    if !missing_extensions.is_empty() {
        assert!(
            std::env::var("SPOT_REQUIRE_PG").as_deref() != Ok("1"),
            "SPOT_REQUIRE_PG=1 but the server does not offer {missing_extensions:?}"
        );
        println!("SKIPPED: the server does not offer {missing_extensions:?}");
        return None;
    }
    let mut room = Definition::new("room");
    room.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let meta = Registry::build(
        vec![room, definition("room", Span::Instants)],
        &Customisations::new(),
    )
    .unwrap_or_else(|e| panic!("the occupancy definition does not build: {e:?}"));
    let catalog = Arc::new(meta.catalog(Collation::new("und").unwrap()).unwrap());
    let backend = Arc::new(PgBackend::connect(db.config()).await.expect("connect"));
    backend.prepare(&catalog).await.expect("prepare");
    let mut tx = backend
        .begin(
            catalog.clone(),
            TxOptions::new(n("test_units"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    for (i, id) in units.iter().enumerate() {
        let mut fields = Fields::new();
        fields.insert(n("name"), Value::Text(format!("room {i}")));
        tx.insert(&DocumentData {
            entity: n("room"),
            id: *id,
            fields,
            children: BTreeMap::new(),
        })
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    Some(Fx {
        _db: db,
        backend,
        catalog,
        clock: Arc::new(ManualClock::new(t0())),
        units,
    })
}

async fn fixture(count: usize) -> Option<Fx> {
    fixture_with((0..count).map(|_| RecordId::new_v7()).collect()).await
}

impl Fx {
    fn service(&self, node: Owner, routes: Arc<dyn OwnerRoutes>) -> Arc<Occupancy<PgBackend>> {
        Arc::new(
            Occupancy::new(
                self.backend.clone(),
                self.catalog.clone(),
                self.clock.clone() as Arc<dyn Clock>,
                node,
                routes,
                Arc::new(MemoryAudit::new()),
                Limits::default(),
            )
            .expect("the service builds"),
        )
    }

    fn api(&self, identity: Arc<dyn Identity>) -> Arc<OccupancyApi> {
        self.api_on(identity, Owner::Hub, Arc::new(HubOwnsAll), true)
    }

    fn api_on(
        &self,
        identity: Arc<dyn Identity>,
        node: Owner,
        routes: Arc<dyn OwnerRoutes>,
        open: bool,
    ) -> Arc<OccupancyApi> {
        let ready = Ready::new();
        ready.set(open);
        Arc::new(OccupancyApi::new(
            self.service(node.clone(), routes.clone()),
            identity,
            routes,
            node,
            self.clock.clone() as Arc<dyn Clock>,
            ready,
            PortSettings {
                max_span: Duration::days(30),
                max_ttl_seconds: 3600,
                span: Span::Instants,
            },
        ))
    }

    /// The rows of `occupancy` as `(unit, state)`, read with plain SQL.
    async fn rows(&self) -> Vec<(String, String)> {
        let client = self.backend.pool().get().await.unwrap();
        let sql = format!("SELECT unit::text, state FROM {ENTITY} ORDER BY unit, id");
        client
            .query(sql.as_str(), &[])
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
            .collect()
    }

    /// Overlapping pairs of one unit in the table: a direct SQL query, not the service's own view.
    async fn overlaps(&self) -> i64 {
        let client = self.backend.pool().get().await.unwrap();
        let sql = format!(
            "SELECT count(*) FROM {ENTITY} a JOIN {ENTITY} b \
             ON a.unit = b.unit AND a.id < b.id AND a.during && b.during"
        );
        client.query_one(sql.as_str(), &[]).await.unwrap().get(0)
    }
}

fn claim_in(unit: RecordId, from_day: i64, days: i64, ttl: Option<u32>) -> ClaimIn {
    let start = t0() + Duration::days(from_day);
    ClaimIn {
        unit: unit.to_string(),
        start,
        end: start + Duration::days(days),
        ttl_seconds: ttl,
    }
}

fn is_api(err: &OccupancyError, code: Code) -> bool {
    matches!(err, OccupancyError::Api(e) if *e == ApiError::from_code(code))
}

// --- the race cases ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_claims_of_one_interval_exactly_one_wins() {
    let Some(fx) = fixture(1).await else { return };
    let identity = TestIdentity::new(&[("a", user("a", false)), ("b", user("b", false))]);
    let api = fx.api(identity);
    let unit = fx.units[0];
    let (ctx_a, ctx_b) = (ctx("a", None), ctx("b", None));
    let (ra, rb) = tokio::join!(
        api.claim(&ctx_a, ENTITY, claim_in(unit, 1, 2, None)),
        api.claim(&ctx_b, ENTITY, claim_in(unit, 1, 2, None)),
    );
    let wins = [&ra, &rb].iter().filter(|r| r.is_ok()).count();
    let conflicts = [&ra, &rb]
        .iter()
        .filter(|r| matches!(r, Err(OccupancyError::Conflict)))
        .count();
    assert_eq!((wins, conflicts), (1, 1), "{ra:?} {rb:?}");
    assert_eq!(fx.rows().await.len(), 1);
    assert_eq!(fx.overlaps().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_hundred_takers_on_twenty_units_never_double_book() {
    let Some(fx) = fixture(20).await else { return };
    let tokens: Vec<(String, Who)> = (0..200)
        .map(|i| (format!("t{i}"), user(&format!("t{i}"), false)))
        .collect();
    let identity = TestIdentity::new(
        &tokens
            .iter()
            .map(|(t, w)| (t.as_str(), w.clone()))
            .collect::<Vec<_>>(),
    );
    let api = fx.api(identity);
    let mut takers = Vec::new();
    for i in 0..200usize {
        let api = api.clone();
        let unit = fx.units[i % 20];
        takers.push(tokio::spawn(async move {
            let token = format!("t{i}");
            let hold = api
                .claim(&ctx(&token, None), ENTITY, claim_in(unit, 3, 2, None))
                .await;
            match hold {
                Ok(h) => {
                    // Every winner confirms: the confirmed rows are the winners.
                    let confirmed = api
                        .confirm(&ctx(&token, None), ENTITY, &h.id)
                        .await
                        .expect("a winner confirms its own hold");
                    assert_eq!(confirmed.state, HoldStateOut::Confirmed);
                    Some((unit, h.id))
                }
                Err(OccupancyError::Conflict) => None,
                Err(other) => panic!("a taker got {other:?}"),
            }
        }));
    }
    let mut winners = Vec::new();
    for t in takers {
        if let Some(w) = t.await.unwrap() {
            winners.push(w);
        }
    }
    let units: BTreeSet<_> = winners.iter().map(|(u, _)| *u).collect();
    assert_eq!(winners.len(), 20, "one winner per unit");
    assert_eq!(units.len(), 20, "no unit has two winners");
    let rows = fx.rows().await;
    assert_eq!(rows.len(), winners.len());
    assert!(rows.iter().all(|(_, state)| state == "confirmed"));
    let distinct: BTreeSet<_> = rows.iter().map(|(u, _)| u.clone()).collect();
    assert_eq!(
        distinct.len(),
        winners.len(),
        "confirmed rows equal the distinct winners"
    );
    assert_eq!(fx.overlaps().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_hundred_thousand_attempts_across_fifty_units_leave_no_overlap() {
    let attempts: u64 = std::env::var("SPOT_OCC_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let Some(fx) = fixture(50).await else { return };
    const WORKERS: u64 = 32;
    let tokens: Vec<(String, Who)> = (0..WORKERS)
        .map(|i| (format!("w{i}"), user(&format!("w{i}"), false)))
        .collect();
    let identity = TestIdentity::new(
        &tokens
            .iter()
            .map(|(t, w)| (t.as_str(), w.clone()))
            .collect::<Vec<_>>(),
    );
    let api = fx.api(identity);
    let service = fx.service(Owner::Hub, Arc::new(HubOwnsAll));
    let done = Arc::new(AtomicBool::new(false));
    // The clock and the sweeper race the takers: a hold lives a few ticks.
    let sweeper = {
        let (service, clock, done) = (service.clone(), fx.clock.clone(), done.clone());
        tokio::spawn(async move {
            while !done.load(Ordering::Relaxed) {
                clock.advance(std::time::Duration::from_secs(120));
                service.sweep_all(5).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
    };
    let per_worker = attempts / WORKERS + 1;
    let units = Arc::new(fx.units.clone());
    let mut workers = Vec::new();
    for w in 0..WORKERS {
        let (api, units) = (api.clone(), units.clone());
        workers.push(tokio::spawn(async move {
            let token = format!("w{w}");
            let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ (w + 1);
            let mut won = 0u64;
            for _ in 0..per_worker {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let r = seed >> 33;
                let unit = units[(r % 50) as usize];
                let from = 1 + ((r >> 8) % 24) as i64;
                let days = 1 + ((r >> 16) % 4) as i64;
                match api
                    .claim(
                        &ctx(&token, None),
                        ENTITY,
                        claim_in(unit, from, days, Some(600)),
                    )
                    .await
                {
                    Ok(h) => {
                        won += 1;
                        if (r >> 24).is_multiple_of(2) {
                            // A late confirm is the typed answer, never a success after expiry.
                            match api.confirm(&ctx(&token, None), ENTITY, &h.id).await {
                                Ok(_) | Err(OccupancyError::Expired) => {}
                                Err(e) if is_api(&e, Code::ServerUnavailable) => {}
                                Err(e) => panic!("confirm failed unexpectedly: {e:?}"),
                            }
                        }
                    }
                    Err(OccupancyError::Conflict) => {}
                    // Retries exhausted under load: allowed, and cannot double-book.
                    Err(e) if is_api(&e, Code::ServerUnavailable) => {}
                    Err(e) => panic!("claim failed unexpectedly: {e:?}"),
                }
            }
            won
        }));
    }
    let mut won = 0;
    for w in workers {
        won += w.await.unwrap();
    }
    done.store(true, Ordering::Relaxed);
    sweeper.await.unwrap();
    assert!(won > 0, "the run did not contend");
    assert_eq!(
        fx.overlaps().await,
        0,
        "double booking after {attempts} attempts"
    );
}

// --- expiry --------------------------------------------------------------------------------

#[tokio::test]
async fn an_expired_unswept_hold_is_replaced_and_the_old_holders_late_confirm_is_expired() {
    let Some(fx) = fixture(1).await else { return };
    let identity = TestIdentity::new(&[("old", user("old", false)), ("new", user("new", false))]);
    let api = fx.api(identity);
    let unit = fx.units[0];
    let old = api
        .claim(&ctx("old", None), ENTITY, claim_in(unit, 1, 2, Some(60)))
        .await
        .unwrap();
    // While it lives, the interval is taken.
    let blocked = api
        .claim(&ctx("new", None), ENTITY, claim_in(unit, 1, 2, None))
        .await;
    assert_eq!(blocked.unwrap_err(), OccupancyError::Conflict);
    // It expires; nothing swept it. The next claim takes it over in one transaction.
    fx.clock.advance(std::time::Duration::from_secs(61));
    let fresh = api
        .claim(&ctx("new", None), ENTITY, claim_in(unit, 1, 2, None))
        .await
        .unwrap();
    assert_eq!(fresh.holder, "user:new");
    let late = api.confirm(&ctx("old", None), ENTITY, &old.id).await;
    assert_eq!(late.unwrap_err(), OccupancyError::Expired);
    assert_eq!(fx.rows().await.len(), 1);
    assert_eq!(fx.overlaps().await, 0);
}

#[tokio::test]
async fn a_late_confirm_before_the_sweeper_runs_is_still_refused() {
    let Some(fx) = fixture(1).await else { return };
    let identity = TestIdentity::new(&[("a", user("a", false))]);
    let api = fx.api(identity);
    let hold = api
        .claim(
            &ctx("a", None),
            ENTITY,
            claim_in(fx.units[0], 1, 1, Some(30)),
        )
        .await
        .unwrap();
    fx.clock.advance(std::time::Duration::from_secs(31));
    let late = api.confirm(&ctx("a", None), ENTITY, &hold.id).await;
    assert_eq!(late.unwrap_err(), OccupancyError::Expired);
}

#[tokio::test]
async fn a_sweeper_pass_that_rolls_back_loses_nothing() {
    let Some(fx) = fixture(1).await else { return };
    let identity = TestIdentity::new(&[("a", user("a", false))]);
    let api = fx.api(identity);
    let service = fx.service(Owner::Hub, Arc::new(HubOwnsAll));
    for day in 1..=3 {
        api.claim(
            &ctx("a", None),
            ENTITY,
            claim_in(fx.units[0], day * 2, 1, Some(10)),
        )
        .await
        .unwrap();
    }
    fx.clock.advance(std::time::Duration::from_secs(11));
    // A pass that dies mid-batch: it locks and deletes, then its transaction is rolled back.
    {
        let client = fx.backend.pool().get().await.unwrap();
        client.batch_execute("BEGIN").await.unwrap();
        client
            .batch_execute(&format!("DELETE FROM {ENTITY} WHERE state = 'tentative'"))
            .await
            .unwrap();
        client.batch_execute("ROLLBACK").await.unwrap();
    }
    assert_eq!(
        fx.rows().await.len(),
        3,
        "the rolled-back pass deleted nothing"
    );
    let report = service.sweep_all(5).await.unwrap();
    assert_eq!(report.expired, 3, "the next pass finds the same rows");
    assert!(fx.rows().await.is_empty());
}

// --- partition ownership -------------------------------------------------------------------

#[tokio::test]
async fn a_write_on_a_node_that_does_not_own_the_unit_is_not_owner_and_touches_nothing() {
    let Some(fx) = fixture(2).await else { return };
    let homed = fx.units[0];
    let routes: Arc<dyn OwnerRoutes> = Arc::new(HomeEdges::new().with_home(homed, "branch-1"));
    let identity = TestIdentity::new(&[("a", user("a", false))]);
    // The hub node: the homed unit belongs to the edge.
    let api = fx.api_on(identity, Owner::Hub, routes, true);
    let refused = api
        .claim(&ctx("a", None), ENTITY, claim_in(homed, 1, 1, None))
        .await;
    assert_eq!(refused.unwrap_err(), OccupancyError::NotOwner);
    assert!(fx.rows().await.is_empty(), "nothing was written");
    // A unit the hub owns still works.
    api.claim(&ctx("a", None), ENTITY, claim_in(fx.units[1], 1, 1, None))
        .await
        .unwrap();
}

// --- identity and rights -------------------------------------------------------------------

#[tokio::test]
async fn consumers_claim_for_themselves_and_cannot_release_anothers_hold() {
    let Some(fx) = fixture(1).await else { return };
    let identity = TestIdentity::new(&[
        ("c1", consumer("c1")),
        ("c2", consumer("c2")),
        ("staff", user("boss", true)),
    ]);
    let api = fx.api(identity);
    let unit = fx.units[0];
    let hold = api
        .claim(&ctx("c1", None), ENTITY, claim_in(unit, 1, 1, None))
        .await
        .unwrap();
    assert_eq!(hold.holder, "consumer:c1", "the holder is the principal");
    // Another consumer can neither confirm nor release it.
    assert_eq!(
        api.confirm(&ctx("c2", None), ENTITY, &hold.id)
            .await
            .unwrap_err(),
        OccupancyError::NotHolder
    );
    assert_eq!(
        api.release(&ctx("c2", None), ENTITY, &hold.id)
            .await
            .unwrap_err(),
        OccupancyError::NotHolder
    );
    assert_eq!(fx.rows().await.len(), 1);
    // The holder confirms; a staff user with the permission releases it.
    api.confirm(&ctx("c1", None), ENTITY, &hold.id)
        .await
        .unwrap();
    api.release(&ctx("staff", None), ENTITY, &hold.id)
        .await
        .unwrap();
    assert!(fx.rows().await.is_empty());
    // Releasing twice succeeds.
    api.release(&ctx("c1", None), ENTITY, &hold.id)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_request_without_a_valid_credential_is_refused_before_storage() {
    let Some(fx) = fixture(1).await else { return };
    let api = fx.api(TestIdentity::new(&[("a", user("a", false))]));
    let refused = api
        .claim(
            &ctx("stranger", None),
            ENTITY,
            claim_in(fx.units[0], 1, 1, None),
        )
        .await
        .unwrap_err();
    assert!(is_api(&refused, Code::AuthRequired), "{refused:?}");
    assert!(fx.rows().await.is_empty());
}

#[tokio::test]
async fn a_consumer_reads_busy_and_staff_read_the_state_and_neither_sees_the_holder() {
    let Some(fx) = fixture(1).await else { return };
    let identity = TestIdentity::new(&[
        ("c1", consumer("c1")),
        ("c2", consumer("c2")),
        ("staff", user("boss", true)),
    ]);
    let api = fx.api(identity);
    let unit = fx.units[0];
    let hold = claim_in(unit, 2, 2, None);
    api.claim(&ctx("c1", None), ENTITY, hold.clone())
        .await
        .unwrap();
    let from = t0();
    let to = t0() + Duration::days(10);
    let seen_by_consumer = api
        .availability(&ctx("c2", None), ENTITY, &unit.to_string(), from, to)
        .await
        .unwrap();
    let statuses: Vec<_> = seen_by_consumer
        .intervals
        .iter()
        .map(|i| i.status)
        .collect();
    assert_eq!(
        statuses,
        vec![
            IntervalStatusOut::Free,
            IntervalStatusOut::Busy,
            IntervalStatusOut::Free
        ]
    );
    assert!(
        seen_by_consumer
            .intervals
            .iter()
            .all(|i| i.holder.is_none())
    );
    let seen_by_staff = api
        .availability(&ctx("staff", None), ENTITY, &unit.to_string(), from, to)
        .await
        .unwrap();
    assert_eq!(seen_by_staff.intervals[1].status, IntervalStatusOut::Held);
    assert!(seen_by_staff.intervals.iter().all(|i| i.holder.is_none()));
}

#[tokio::test]
async fn the_conflict_answer_reveals_nothing_about_the_other_holder() {
    let Some(fx) = fixture(1).await else { return };
    let api = fx.api(TestIdentity::new(&[
        ("a", user("alice-secret-id", false)),
        ("b", user("b", false)),
    ]));
    let unit = fx.units[0];
    api.claim(&ctx("a", None), ENTITY, claim_in(unit, 1, 2, None))
        .await
        .unwrap();
    let conflict = api
        .claim(&ctx("b", None), ENTITY, claim_in(unit, 2, 2, None))
        .await
        .unwrap_err();
    assert_eq!(conflict, OccupancyError::Conflict);
    let text = format!("{conflict:?}");
    assert!(!text.contains("alice"), "{text}");
    assert!(!text.contains(&unit.to_string()), "{text}");
}

// --- tenants -------------------------------------------------------------------------------

#[tokio::test]
async fn tenant_b_cannot_see_or_affect_tenant_as_units_even_with_the_same_unit_id() {
    let shared = RecordId::new_v7();
    let Some(a) = fixture_with(vec![shared]).await else {
        return;
    };
    let Some(b) = fixture_with(vec![shared]).await else {
        return;
    };
    let ids = TestIdentity::new(&[("x", user("x", false)), ("y", user("y", false))]);
    let api_a = a.api(ids.clone());
    let api_b = b.api(ids);
    api_a
        .claim(&ctx("x", None), ENTITY, claim_in(shared, 1, 3, None))
        .await
        .unwrap();
    // B sees the unit free and can claim the same dates: A's row is in A's database.
    let seen = api_b
        .availability(
            &ctx("y", None),
            ENTITY,
            &shared.to_string(),
            t0(),
            t0() + Duration::days(10),
        )
        .await
        .unwrap();
    assert!(
        seen.intervals
            .iter()
            .all(|i| i.status == IntervalStatusOut::Free)
    );
    api_b
        .claim(&ctx("y", None), ENTITY, claim_in(shared, 1, 3, None))
        .await
        .unwrap();
    assert_eq!(a.rows().await.len(), 1);
    assert_eq!(b.rows().await.len(), 1);
}

// --- the gate, the replay and the start-up check ---------------------------------------------

#[tokio::test]
async fn a_port_whose_check_failed_serves_nothing() {
    let Some(fx) = fixture(1).await else { return };
    let api = fx.api_on(
        TestIdentity::new(&[("a", user("a", false))]),
        Owner::Hub,
        Arc::new(HubOwnsAll),
        false,
    );
    let refused = api
        .claim(&ctx("a", None), ENTITY, claim_in(fx.units[0], 1, 1, None))
        .await
        .unwrap_err();
    assert!(is_api(&refused, Code::ServerUnavailable), "{refused:?}");
    assert!(fx.rows().await.is_empty());
}

#[tokio::test]
async fn an_unknown_entity_is_not_found() {
    let Some(fx) = fixture(1).await else { return };
    let api = fx.api(TestIdentity::new(&[("a", user("a", false))]));
    let refused = api
        .claim(
            &ctx("a", None),
            "room_hold",
            claim_in(fx.units[0], 1, 1, None),
        )
        .await
        .unwrap_err();
    assert!(is_api(&refused, Code::DocumentNotFound), "{refused:?}");
}

#[tokio::test]
async fn a_repeated_claim_with_one_key_answers_the_first_answer() {
    let Some(fx) = fixture(1).await else { return };
    let api = fx.api(TestIdentity::new(&[("a", user("a", false))]));
    let unit = fx.units[0];
    let first: HoldOut = api
        .claim(&ctx("a", Some("k1")), ENTITY, claim_in(unit, 1, 1, None))
        .await
        .unwrap();
    let again = api
        .claim(&ctx("a", Some("k1")), ENTITY, claim_in(unit, 1, 1, None))
        .await
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(fx.rows().await.len(), 1);
    // The same key for another request is a conflict of intent.
    let other = api
        .claim(&ctx("a", Some("k1")), ENTITY, claim_in(unit, 5, 1, None))
        .await
        .unwrap_err();
    assert!(is_api(&other, Code::ApiIdempotencyConflict), "{other:?}");
}

#[tokio::test]
async fn limits_refuse_a_long_range_and_a_long_ttl_before_storage() {
    let Some(fx) = fixture(1).await else { return };
    let api = fx.api(TestIdentity::new(&[("a", user("a", false))]));
    let unit = fx.units[0];
    assert_eq!(
        api.claim(&ctx("a", None), ENTITY, claim_in(unit, 1, 31, None))
            .await
            .unwrap_err(),
        OccupancyError::InvalidRange
    );
    assert_eq!(
        api.claim(&ctx("a", None), ENTITY, claim_in(unit, 1, 1, Some(3601)))
            .await
            .unwrap_err(),
        OccupancyError::TtlTooLong
    );
    assert!(fx.rows().await.is_empty());
}

#[tokio::test]
async fn the_startup_check_names_a_missing_constraint() {
    let Some(fx) = fixture(1).await else { return };
    assert_eq!(missing(&fx.backend).await.unwrap(), None);
    let client = fx.backend.pool().get().await.unwrap();
    // The physical name is `spot_<table>_<declared name>_excl`.
    client
        .batch_execute(&format!(
            "ALTER TABLE {ENTITY} DROP CONSTRAINT spot_{ENTITY}_ex_occupancy_during_excl"
        ))
        .await
        .unwrap();
    let reason = missing(&fx.backend)
        .await
        .unwrap()
        .expect("the constraint is gone");
    assert!(reason.contains("ex_occupancy_during"), "{reason}");
}

#[test]
fn service_errors_map_without_carrying_storage_detail() {
    use spot_occupancy::OccupancyError as E;
    assert_eq!(map_error(E::NotHolder), OccupancyError::NotHolder);
    assert_eq!(
        map_error(E::InvalidRange("x".into())),
        OccupancyError::InvalidRange
    );
}
