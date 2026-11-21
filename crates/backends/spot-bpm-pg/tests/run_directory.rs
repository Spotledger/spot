//! The visibility-filtered, keyset-paged Case listing (WP-BPM-02.T9), over a real PostgreSQL.
//!
//! Cases are inserted with plain SQL (the listing reads only `spot_bpm_case` columns), so a test
//! controls every start instant, initiator, process and status. Tests skip, printing `SKIPPED:`,
//! when `SPOT_TEST_PG_URL` is unset (`TestDb::create` panics under `SPOT_REQUIRE_PG=1`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::pool;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_pg::{
    CaseStatusFilter, LISTING_SQL, ListingCursor, ListingPage, ListingQuery, MAX_ALSO_CASES,
    PgCaseStore, PgRunDirectory,
};
use spot_postgres::testing::TestDb;
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

/// Inserts `count` Cases for `tenant`, ids `<prefix>-c00000..`, one minute apart from 2026-01-01:
/// initiators ann, bob, cat in turn; process `p_a` for even `g`, `p_b` for odd; status
/// running, completed, failed, cancelled in turn.
async fn seed(client: &Client, tenant: &str, prefix: &str, count: i32) {
    let sql = "INSERT INTO spot_bpm_case \
        (case_id, tenant, business_key, dag_key, dag_version, content_hash, status, version, \
         started_at, initiator, nodes) \
        SELECT $2::text || '-c' || lpad(g::text, 5, '0'), $1::text, 'bk-' || $2::text || '-' || g::text, \
               CASE WHEN g % 2 = 0 THEN 'p_a' ELSE 'p_b' END, 1, 'h', \
               (ARRAY['running','completed','failed','cancelled'])[g % 4 + 1], 0, \
               timestamptz '2026-01-01 00:00:00+00' + g * interval '1 minute', \
               (ARRAY['ann','bob','cat'])[g % 3 + 1], '{}'::jsonb \
        FROM generate_series(0, $3::int - 1) g";
    client
        .execute(sql, &[&tenant, &prefix, &count])
        .await
        .unwrap();
}

/// One Case with explicit fields.
async fn insert_case(
    client: &Client,
    tenant: &str,
    id: &str,
    started: &str,
    initiator: Option<&str>,
    process: &str,
) {
    client
        .execute(
            "INSERT INTO spot_bpm_case \
             (case_id, tenant, business_key, dag_key, dag_version, content_hash, status, version, \
              started_at, initiator, nodes) \
             VALUES ($1, $2, 'bk-' || $1::text, $3, 1, 'h', 'running', 0, $4::text::timestamptz, $5, '{}'::jsonb)",
            &[&id, &tenant, &process, &started, &initiator],
        )
        .await
        .unwrap();
}

fn id(prefix: &str, g: i32) -> String {
    format!("{prefix}-c{g:05}")
}

/// The ids of tenant `prefix`'s Cases with `g` in `0..count`, newest first, that satisfy `keep`.
fn expect(prefix: &str, count: i32, keep: impl Fn(i32) -> bool) -> Vec<String> {
    (0..count)
        .rev()
        .filter(|g| keep(*g))
        .map(|g| id(prefix, g))
        .collect()
}

fn query(viewer: &str) -> ListingQuery {
    ListingQuery {
        viewer: viewer.to_owned(),
        also_cases: Vec::new(),
        sees_all: false,
        process: None,
        status: None,
        mine: false,
        after: None,
        limit: 200,
    }
}

async fn directory(db: &TestDb, tenant: &str) -> PgRunDirectory {
    let pool = pool(db, 4, "bpm-run-directory");
    PgCaseStore::new(pool.clone()).prepare().await.unwrap();
    let dir = PgRunDirectory::new(pool).with_tenant(tenant);
    dir.prepare().await.unwrap();
    dir
}

/// Walks every page of `base` with `size`, returning all ids in order.
async fn walk(dir: &PgRunDirectory, base: &ListingQuery, size: u32) -> Vec<String> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = dir
            .page(&ListingQuery {
                after: after.clone(),
                limit: size,
                ..base.clone()
            })
            .await
            .unwrap();
        out.extend(page.case_ids.clone());
        match page.next {
            Some(next) => after = Some(next),
            None => return out,
        }
    }
}

macro_rules! db_or_skip {
    () => {
        match TestDb::create().await {
            Some(db) => db,
            None => {
                println!("SKIPPED: SPOT_TEST_PG_URL is not set");
                return;
            }
        }
    };
}

/// Sixty `t1` Cases and twenty `t2` Cases.
async fn two_tenants(db: &TestDb) -> (PgRunDirectory, PgRunDirectory, Client) {
    let a = directory(db, "t1").await;
    let b = directory(db, "t2").await;
    let client = db.raw_client().await;
    seed(&client, "t1", "t1", 60).await;
    seed(&client, "t2", "t2", 20).await;
    (a, b, client)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ordinary_viewer_sees_own_cases_plus_the_listed_ones_and_nothing_of_another_tenant() {
    let db = db_or_skip!();
    let (a, _b, _c) = two_tenants(&db).await;
    let own = a.page(&query("ann")).await.unwrap();
    assert_eq!(own.case_ids, expect("t1", 60, |g| g % 3 == 0));
    assert_eq!(own.next, None);

    // One of bob's Cases is visible through a task: merged into the order.
    let also = ListingQuery {
        also_cases: vec![id("t1", 1), id("t2", 3)],
        ..query("ann")
    };
    let page = a.page(&also).await.unwrap();
    assert_eq!(page.case_ids, expect("t1", 60, |g| g % 3 == 0 || g == 1));
    assert!(
        page.case_ids.iter().all(|c| c.starts_with("t1-")),
        "a listed Case of another tenant is not returned"
    );
    // A viewer with nothing sees nothing.
    assert!(a.page(&query("zed")).await.unwrap().case_ids.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sees_all_lists_the_tenant_and_mine_narrows_it() {
    let db = db_or_skip!();
    let (a, b, _c) = two_tenants(&db).await;
    let all = ListingQuery {
        sees_all: true,
        ..query("sup")
    };
    assert_eq!(
        a.page(&all).await.unwrap().case_ids,
        expect("t1", 60, |_| true)
    );
    assert_eq!(
        b.page(&all).await.unwrap().case_ids,
        expect("t2", 20, |_| true)
    );
    let mine = ListingQuery {
        sees_all: true,
        mine: true,
        ..query("ann")
    };
    assert_eq!(
        a.page(&mine).await.unwrap().case_ids,
        expect("t1", 60, |g| g % 3 == 0)
    );
    // `mine` without sees_all is the same set.
    let own = ListingQuery {
        mine: true,
        ..query("ann")
    };
    assert_eq!(
        a.page(&own).await.unwrap().case_ids,
        expect("t1", 60, |g| g % 3 == 0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paging_visits_every_visible_row_once_at_every_page_size() {
    let db = db_or_skip!();
    let (a, _b, _c) = two_tenants(&db).await;
    let all = ListingQuery {
        sees_all: true,
        ..query("sup")
    };
    let expected = expect("t1", 60, |_| true);
    for size in [1, 7, 200] {
        let walked = walk(&a, &all, size).await;
        assert_eq!(walked, expected, "page size {size}");
        let unique: BTreeSet<&String> = walked.iter().collect();
        assert_eq!(unique.len(), walked.len(), "no duplicate at {size}");
    }
    // The last page has no cursor and no empty page follows: 60 rows, pages of 20.
    let first = a
        .page(&ListingQuery {
            limit: 20,
            ..all.clone()
        })
        .await
        .unwrap();
    assert!(first.next.is_some());
    let third = a
        .page(&ListingQuery {
            limit: 20,
            after: Some(
                a.page(&ListingQuery {
                    limit: 20,
                    after: first.next.clone(),
                    ..all.clone()
                })
                .await
                .unwrap()
                .next
                .unwrap(),
            ),
            ..all.clone()
        })
        .await
        .unwrap();
    assert_eq!(third.case_ids.len(), 20);
    assert_eq!(
        third.next, None,
        "exactly the last page: no extra empty page"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_inserted_between_pages_neither_shifts_nor_duplicates_the_walk() {
    let db = db_or_skip!();
    let (a, _b, client) = two_tenants(&db).await;
    let all = ListingQuery {
        sees_all: true,
        ..query("sup")
    };
    let first = a
        .page(&ListingQuery {
            limit: 7,
            ..all.clone()
        })
        .await
        .unwrap();
    // A newer Case appears (before the cursor) and an older one (after it).
    insert_case(
        &client,
        "t1",
        "t1-new",
        "2026-02-01T00:00:00Z",
        Some("ann"),
        "p_a",
    )
    .await;
    insert_case(
        &client,
        "t1",
        "t1-old",
        "2025-12-01T00:00:00Z",
        Some("ann"),
        "p_a",
    )
    .await;
    let mut seen = first.case_ids.clone();
    let mut after = first.next.clone();
    while let Some(cursor) = after {
        let page = a
            .page(&ListingQuery {
                limit: 7,
                after: Some(cursor),
                ..all.clone()
            })
            .await
            .unwrap();
        seen.extend(page.case_ids);
        after = page.next;
    }
    let mut expected = expect("t1", 60, |_| true);
    expected.push("t1-old".to_owned());
    assert_eq!(
        seen, expected,
        "the newer row is not seen, the older one is, nothing shifts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_start_instants_order_by_case_id_and_page_across_the_tie() {
    let db = db_or_skip!();
    let dir = directory(&db, "t1").await;
    let client = db.raw_client().await;
    for name in ["tie-a", "tie-c", "tie-b"] {
        insert_case(
            &client,
            "t1",
            name,
            "2026-03-01T00:00:00Z",
            Some("ann"),
            "p_a",
        )
        .await;
    }
    insert_case(
        &client,
        "t1",
        "tie-z",
        "2026-03-02T00:00:00Z",
        Some("ann"),
        "p_a",
    )
    .await;
    let all = ListingQuery {
        sees_all: true,
        ..query("sup")
    };
    let expected = ["tie-z", "tie-c", "tie-b", "tie-a"]
        .map(str::to_owned)
        .to_vec();
    assert_eq!(walk(&dir, &all, 1).await, expected);
    assert_eq!(walk(&dir, &all, 2).await, expected);
    assert_eq!(walk(&dir, &all, 3).await, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_and_status_filters_combine() {
    let db = db_or_skip!();
    let (a, _b, _c) = two_tenants(&db).await;
    let base = ListingQuery {
        sees_all: true,
        ..query("sup")
    };
    // even g is p_a; g % 4 == 0 is running.
    let a_running = ListingQuery {
        process: Some("p_a".to_owned()),
        status: Some(CaseStatusFilter::Running),
        ..base.clone()
    };
    assert_eq!(
        a.page(&a_running).await.unwrap().case_ids,
        expect("t1", 60, |g| g % 2 == 0 && g % 4 == 0)
    );
    // odd g is p_b; g % 4 == 1 is completed.
    let b_completed = ListingQuery {
        process: Some("p_b".to_owned()),
        status: Some(CaseStatusFilter::Completed),
        ..base.clone()
    };
    assert_eq!(
        a.page(&b_completed).await.unwrap().case_ids,
        expect("t1", 60, |g| g % 2 == 1 && g % 4 == 1)
    );
    // p_b and running never meet.
    let none = ListingQuery {
        process: Some("p_b".to_owned()),
        status: Some(CaseStatusFilter::Running),
        ..base.clone()
    };
    assert!(a.page(&none).await.unwrap().case_ids.is_empty());
    // Only a status.
    let failed = ListingQuery {
        status: Some(CaseStatusFilter::Failed),
        ..base
    };
    assert_eq!(
        a.page(&failed).await.unwrap().case_ids,
        expect("t1", 60, |g| g % 4 == 2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_limit_is_clamped_and_a_huge_also_cases_list_is_refused() {
    let db = db_or_skip!();
    let (a, _b, _c) = two_tenants(&db).await;
    let all = ListingQuery {
        sees_all: true,
        ..query("sup")
    };
    let zero = a
        .page(&ListingQuery {
            limit: 0,
            ..all.clone()
        })
        .await
        .unwrap();
    assert_eq!(zero.case_ids.len(), 1, "limit 0 is clamped up to 1");
    assert!(zero.next.is_some());
    let huge = a
        .page(&ListingQuery {
            limit: 10_000,
            ..all.clone()
        })
        .await
        .unwrap();
    assert_eq!(
        huge.case_ids.len(),
        60,
        "limit 10,000 is clamped to 200: all 60 fit"
    );
    assert_eq!(huge.next, None);

    let long: Vec<String> = (0..=MAX_ALSO_CASES).map(|n| format!("x-{n}")).collect();
    assert_eq!(long.len(), 5_001);
    let refused = a
        .page(&ListingQuery {
            also_cases: long,
            ..query("ann")
        })
        .await;
    assert!(refused.is_err());
    let at_limit: Vec<String> = (0..MAX_ALSO_CASES).map(|n| format!("x-{n}")).collect();
    assert!(
        a.page(&ListingQuery {
            also_cases: at_limit,
            ..query("ann")
        })
        .await
        .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cursor_from_one_tenant_never_returns_the_others_rows() {
    let db = db_or_skip!();
    let (a, b, client) = two_tenants(&db).await;
    let all = ListingQuery {
        sees_all: true,
        ..query("sup")
    };
    let first = a
        .page(&ListingQuery {
            limit: 5,
            ..all.clone()
        })
        .await
        .unwrap();
    let cursor: ListingCursor = first.next.clone().unwrap();
    let from_b = b
        .page(&ListingQuery {
            after: Some(cursor.clone()),
            ..all.clone()
        })
        .await
        .unwrap();
    assert!(from_b.case_ids.iter().all(|c| c.starts_with("t2-")));
    let rows = client
        .query(
            "SELECT case_id FROM spot_bpm_case WHERE tenant = 't2' \
             AND (started_at, case_id) < ($1, $2) ORDER BY started_at DESC, case_id DESC",
            &[&cursor.started_at, &cursor.case_id],
        )
        .await
        .unwrap();
    let expected: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(from_b.case_ids, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_twice_is_harmless_and_creates_the_two_named_indexes() {
    let db = db_or_skip!();
    let dir = directory(&db, "t1").await;
    dir.prepare().await.unwrap();
    dir.prepare().await.unwrap();
    let client = db.raw_client().await;
    let rows = client
        .query(
            "SELECT indexname FROM pg_indexes WHERE tablename = 'spot_bpm_case' \
             AND indexname LIKE 'spot_bpm_case_%_idx' ORDER BY indexname",
            &[],
        )
        .await
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    for wanted in ["spot_bpm_case_initiator_idx", "spot_bpm_case_listing_idx"] {
        assert!(names.contains(&wanted.to_owned()), "{names:?}");
    }
}

async fn explain(client: &Client, params: &[&(dyn ToSql + Sync)]) -> String {
    client
        .query(&format!("EXPLAIN {LISTING_SQL}"), params)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_listing_shapes_are_served_by_the_new_indexes() {
    let db = db_or_skip!();
    let dir = directory(&db, "t1").await;
    let client = db.raw_client().await;
    seed(&client, "t1", "big", 5_000).await;
    client.batch_execute("ANALYZE spot_bpm_case").await.unwrap();
    let _ = dir;

    let tenant = "t1".to_owned();
    let viewer = "ann".to_owned();
    let also: Vec<String> = Vec::new();
    let none_text: Option<String> = None;
    let none_at: Option<OffsetDateTime> = None;
    let process = Some("p_a".to_owned());
    let limit = 201_i64;
    let (yes, no) = (true, false);

    let shapes: [(&str, Vec<&(dyn ToSql + Sync)>); 3] = [
        (
            "no filter",
            vec![
                &tenant, &yes, &viewer, &also, &no, &none_text, &none_text, &none_at, &none_text,
                &limit,
            ],
        ),
        (
            "mine",
            vec![
                &tenant, &yes, &viewer, &also, &yes, &none_text, &none_text, &none_at, &none_text,
                &limit,
            ],
        ),
        (
            "process",
            vec![
                &tenant, &yes, &viewer, &also, &no, &process, &none_text, &none_at, &none_text,
                &limit,
            ],
        ),
    ];
    for (name, params) in &shapes {
        let plan = explain(&client, params).await;
        println!("{name}:\n{plan}\n");
        assert!(
            plan.contains("spot_bpm_case_listing_idx")
                || plan.contains("spot_bpm_case_initiator_idx"),
            "{name} plan:\n{plan}"
        );
        assert!(
            !plan.contains("Seq Scan on spot_bpm_case"),
            "{name} plan:\n{plan}"
        );
    }
}

#[test]
fn the_page_type_is_plain_data() {
    let page = ListingPage {
        case_ids: vec!["a".to_owned()],
        next: None,
    };
    assert_eq!(page.clone(), page);
}
