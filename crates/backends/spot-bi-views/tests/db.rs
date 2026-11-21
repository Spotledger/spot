//! The views, the role and the checks against a real PostgreSQL (feature `testing`).
//!
//! Set `SPOT_TEST_PG_URL` (an admin URL). Without it each test prints `SKIPPED` and passes;
//! with `SPOT_REQUIRE_PG=1` a missing URL fails. The role is cluster-wide, so these tests run
//! one at a time behind a lock.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::OnceLock;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use spot_bi_views::{BiPlan, ExclusionReason, apply, generate, grant_role_sql, retire, verify};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, EntityDef, EntityKind, FieldType, StorageBackend};
use support::*;
use tokio::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

struct Fixture {
    _serial: MutexGuard<'static, ()>,
    db: TestDb,
    backend: PgBackend,
    pool: Pool,
}

async fn fixture(catalog: &Catalog) -> Option<Fixture> {
    let serial = SERIAL.get_or_init(|| Mutex::new(())).lock().await;
    let db = TestDb::create().await?;
    uuidv7_stand_in(&db).await;
    let backend = PgBackend::connect_unchecked(db.config()).unwrap();
    backend.prepare(catalog).await.unwrap();
    let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
    let mut config: tokio_postgres::Config = url.parse().unwrap();
    config.dbname(db.name());
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool = Pool::builder(manager).max_size(4).build().unwrap();
    Some(Fixture {
        _serial: serial,
        db,
        backend,
        pool,
    })
}

/// PostgreSQL 18 has `uuidv7()` built in, and the storage change feed's trigger calls it. On an
/// older server (this sandbox runs 16) the test database gets a stand-in with the same signature,
/// so document writes can run. Only ids come from it; the BI checks never depend on their order.
async fn uuidv7_stand_in(db: &TestDb) {
    let client = db.raw_client().await;
    let present: bool = client
        .query_one("SELECT to_regprocedure('uuidv7()') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    if !present {
        client
            .batch_execute(
                "CREATE FUNCTION uuidv7() RETURNS uuid LANGUAGE sql AS 'SELECT gen_random_uuid()'",
            )
            .await
            .unwrap();
    }
}

fn plan(catalog: &Catalog, policy_text: &str) -> BiPlan {
    generate(catalog, &policy(policy_text), &fixture_protection()).unwrap_or_else(|e| panic!("{e}"))
}

async fn admin(f: &Fixture) -> Client {
    f.db.raw_client().await
}

/// Two parties' worth of data: one customer, three invoices in three states, two lines on the
/// posted one and one on the draft.
async fn seed(client: &Client) {
    client
        .batch_execute(
            "INSERT INTO party (id, version, name, tax_id) VALUES \
             ('00000000-0000-7000-8000-000000000001', 1, 'Acme', 'TAX-SECRET-1')",
        )
        .await
        .unwrap();
    let invoices = [
        ("00000000-0000-7000-8000-000000000011", 0_i64, "10.00"),
        ("00000000-0000-7000-8000-000000000012", 1_i64, "250.50"),
        ("00000000-0000-7000-8000-000000000013", 2_i64, "99.99"),
    ];
    for (id, docstatus, total) in invoices {
        client
            .execute(
                "INSERT INTO sales_invoice (id, version, docstatus, customer, posting_date, total, \
                 margin, api_token, remarks, flags, is_hold) VALUES \
                 ($1::text::uuid, 1, $2, '00000000-0000-7000-8000-000000000001'::uuid, \
                 '2026-10-01', $3::text::numeric, 42.00, 'tok-secret', 'a note', '{\"a\": 1}', false)",
                &[&id, &docstatus, &total],
            )
            .await
            .unwrap();
    }
    client
        .batch_execute(
            "INSERT INTO sales_invoice_line (id, parent, parent_field, idx, item, qty, rate) VALUES \
             (gen_random_uuid(), '00000000-0000-7000-8000-000000000012', 'lines', 1, 'Widget', 2.000, 5.00), \
             (gen_random_uuid(), '00000000-0000-7000-8000-000000000012', 'lines', 2, 'Gadget', 1.000, 240.50), \
             (gen_random_uuid(), '00000000-0000-7000-8000-000000000011', 'lines', 1, 'Draft line', 1.000, 10.00)",
        )
        .await
        .unwrap();
}

/// `(column, format_type)` of a relation, in column order.
async fn columns_of(client: &Client, schema: &str, relation: &str) -> Vec<(String, String)> {
    client
        .query(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&schema, &relation],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

/// The SQLSTATE of a statement that must fail, or `None` if it succeeded.
async fn refused(client: &Client, sql: &str) -> Option<String> {
    match client.batch_execute(sql).await {
        Ok(()) => None,
        Err(e) => Some(e.code().map_or("?".to_owned(), |c| c.code().to_owned())),
    }
}

#[tokio::test]
async fn apply_creates_the_views_and_a_second_apply_changes_nothing() {
    let catalog = fixture_catalog(7);
    let Some(f) = fixture(&catalog).await else {
        return;
    };
    let plan = plan(&catalog, FIXTURE_POLICY);
    let first = apply(&f.pool, &plan).await.unwrap();
    assert_eq!(first.created.len(), plan.views.len() + 1, "{first:?}");
    assert!(first.replaced.is_empty());
    let second = apply(&f.pool, &plan).await.unwrap();
    assert!(!second.changed(), "{second:?}");
    assert_eq!(second.unchanged.len(), plan.views.len() + 1);

    // The types the views report are the plan's types, and the base tables agree with them
    // (the mapping in the plan is the mapping spot-postgres creates).
    let client = admin(&f).await;
    for view in plan.views.iter().chain(std::iter::once(&plan.catalogue)) {
        let actual = columns_of(&client, "bi", &view.name).await;
        let wanted: Vec<(String, String)> = view
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.pg_type.clone()))
            .collect();
        assert_eq!(actual, wanted, "view {}", view.name);
    }
    for table in ["party", "sales_invoice", "sales_invoice_line"] {
        let base_columns = columns_of(&client, "public", table).await;
        for view in plan.views.iter().filter(|v| v.entity == table) {
            for column in &view.columns {
                if let Some((_, ty)) = base_columns.iter().find(|(c, _)| c == &column.name) {
                    assert_eq!(ty, &column.pg_type, "{table}.{}", column.name);
                }
            }
        }
    }
}

#[tokio::test]
async fn the_views_show_posted_rows_typed_and_no_protected_column() {
    let catalog = fixture_catalog(7);
    let Some(f) = fixture(&catalog).await else {
        return;
    };
    apply(&f.pool, &plan(&catalog, FIXTURE_POLICY))
        .await
        .unwrap();
    let client = admin(&f).await;
    seed(&client).await;

    let invoices: i64 = client
        .query_one("SELECT count(*) FROM bi.v1_sales_invoice", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(invoices, 1, "only the posted invoice is visible");
    let row = client
        .query_one(
            "SELECT customer_name, total::text, posting_date::text FROM bi.v1_sales_invoice",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "Acme");
    assert_eq!(row.get::<_, String>(1), "250.50");
    assert_eq!(row.get::<_, String>(2), "2026-10-01");
    let lines: i64 = client
        .query_one(
            "SELECT count(*) FROM bi.v1_sales_invoice__sales_invoice_line",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        lines, 2,
        "the draft invoice's line stays hidden with its parent"
    );
    let hidden: i64 = client
        .query_one(
            "SELECT count(*) FROM bi.catalogue WHERE column_name IN ('margin', 'api_token', 'tax_id', 'remarks', 'flags')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(hidden, 0);
    let exposed = columns_of(&client, "bi", "v1_party").await;
    assert!(exposed.iter().all(|(c, _)| c != "tax_id"), "{exposed:?}");
    assert!(
        plan(&catalog, FIXTURE_POLICY)
            .excluded
            .iter()
            .any(|e| e.reason == ExclusionReason::Secret)
    );
}

#[tokio::test]
async fn the_bi_role_reads_the_views_and_nothing_else() {
    let catalog = fixture_catalog(7);
    let Some(f) = fixture(&catalog).await else {
        return;
    };
    apply(&f.pool, &plan(&catalog, FIXTURE_POLICY))
        .await
        .unwrap();
    let client = admin(&f).await;
    seed(&client).await;
    client
        .batch_execute("SET ROLE spot_bi_reader")
        .await
        .unwrap();
    assert_eq!(
        refused(&client, "SELECT count(*) FROM bi.v1_sales_invoice").await,
        None
    );
    assert_eq!(
        refused(&client, "SELECT count(*) FROM bi.catalogue").await,
        None
    );
    let denied = [
        "SELECT count(*) FROM sales_invoice",
        "SELECT count(*) FROM party",
        "SELECT count(*) FROM spot_meta",
        "SELECT count(*) FROM spot_outbox",
        "INSERT INTO party (id, version, name) VALUES (gen_random_uuid(), 1, 'x')",
        "CREATE TABLE bi.spot_bi_probe (x integer)",
    ];
    for sql in denied {
        assert_eq!(
            refused(&client, sql).await.as_deref(),
            Some("42501"),
            "should be denied: {sql}"
        );
    }
    // A view may be non-updatable (SQLSTATE 55000), so the refusal is checked and the privilege
    // is checked directly.
    assert!(
        refused(&client, "DELETE FROM bi.v1_sales_invoice")
            .await
            .is_some()
    );
    let may_delete: bool = client
        .query_one(
            "SELECT has_table_privilege('spot_bi_reader', 'bi.v1_sales_invoice', 'DELETE')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!may_delete);
    // Protected and secret columns are not in the views at all.
    for column in ["tax_id"] {
        assert_eq!(
            refused(&client, &format!("SELECT {column} FROM bi.v1_party"))
                .await
                .as_deref(),
            Some("42703"),
            "{column}"
        );
    }
    for column in ["margin", "api_token", "remarks"] {
        assert_eq!(
            refused(
                &client,
                &format!("SELECT {column} FROM bi.v1_sales_invoice")
            )
            .await
            .as_deref(),
            Some("42703"),
            "{column}"
        );
    }
    client.batch_execute("RESET ROLE").await.unwrap();
}

#[tokio::test]
async fn verify_passes_and_fails_when_the_role_is_given_a_base_table() {
    let catalog = fixture_catalog(7);
    let Some(f) = fixture(&catalog).await else {
        return;
    };
    apply(&f.pool, &plan(&catalog, FIXTURE_POLICY))
        .await
        .unwrap();
    let report = verify(&f.pool).await.unwrap();
    assert!(
        report.passed(),
        "{:?}",
        report.failures().collect::<Vec<_>>()
    );
    let client = admin(&f).await;
    client
        .batch_execute("GRANT SELECT ON sales_invoice TO spot_bi_reader")
        .await
        .unwrap();
    let broken = verify(&f.pool).await.unwrap();
    assert!(!broken.passed());
    let failed: Vec<&str> = broken.failures().map(|c| c.name.as_str()).collect();
    assert!(
        failed.contains(&"no table outside bi is readable or writable"),
        "{failed:?}"
    );
    assert!(
        failed.contains(&"role cannot read public.sales_invoice"),
        "{failed:?}"
    );
    client
        .batch_execute("REVOKE SELECT ON sales_invoice FROM spot_bi_reader")
        .await
        .unwrap();
    assert!(verify(&f.pool).await.unwrap().passed());
}

#[tokio::test]
async fn adding_a_field_extends_v1_in_place() {
    let catalog = fixture_catalog(7);
    let Some(f) = fixture(&catalog).await else {
        return;
    };
    apply(&f.pool, &plan(&catalog, FIXTURE_POLICY))
        .await
        .unwrap();
    let mut entities = fixture_entities();
    entities
        .iter_mut()
        .find(|e| e.name.as_str() == "sales_invoice")
        .unwrap()
        .fields
        .push(field("discount", dec(18, 2), true));
    let grown = catalog_of(8, entities);
    f.backend.prepare(&grown).await.unwrap();
    let next = plan(&grown, FIXTURE_POLICY);
    let report = apply(&f.pool, &next).await.unwrap();
    assert!(report.created.is_empty(), "{report:?}");
    assert!(
        report.replaced.contains(&"v1_sales_invoice".to_owned()),
        "{report:?}"
    );
    assert!(
        !report.replaced.contains(&"v1_party".to_owned()),
        "{report:?}"
    );
    let client = admin(&f).await;
    let columns = columns_of(&client, "bi", "v1_sales_invoice").await;
    assert_eq!(
        columns.last().map(|c| c.0.as_str()),
        Some("discount"),
        "{columns:?}"
    );
}

#[tokio::test]
async fn removing_a_field_is_refused_and_a_new_version_leaves_v1_alone() {
    let catalog = fixture_catalog(7);
    let Some(f) = fixture(&catalog).await else {
        return;
    };
    apply(&f.pool, &plan(&catalog, FIXTURE_POLICY))
        .await
        .unwrap();

    let mut entities = fixture_entities();
    entities
        .iter_mut()
        .find(|e| e.name.as_str() == "sales_invoice")
        .unwrap()
        .fields
        .retain(|f| f.name.as_str() != "posting_date");
    let shrunk = catalog_of(9, entities);
    let refused_plan = plan(&shrunk, FIXTURE_POLICY);
    let error = apply(&f.pool, &refused_plan).await.unwrap_err();
    assert_eq!(error.code(), "bi.breaking_change", "{error}");
    let client = admin(&f).await;
    let v1 = columns_of(&client, "bi", "v1_sales_invoice").await;
    assert!(
        v1.iter().any(|(c, _)| c == "posting_date"),
        "v1 must be untouched: {v1:?}"
    );

    let bumped = plan(
        &shrunk,
        &FIXTURE_POLICY.replace(
            "display = \"Sales invoices\"",
            "display = \"Sales invoices\"\nversion = 2",
        ),
    );
    let report = apply(&f.pool, &bumped).await.unwrap();
    assert!(
        report.created.contains(&"v2_sales_invoice".to_owned()),
        "{report:?}"
    );
    assert!(
        report
            .created
            .contains(&"v2_sales_invoice__sales_invoice_line".to_owned()),
        "{report:?}"
    );
    let v1_after = columns_of(&client, "bi", "v1_sales_invoice").await;
    assert_eq!(v1_after, v1, "v1 is unchanged by the new version");
    let listed: i64 = client
        .query_one(
            "SELECT count(*) FROM bi.catalogue WHERE view_name = 'bi.v1_sales_invoice'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(listed, 0, "the catalogue lists the current plan only");

    let dropped = retire(&f.pool, &shrunk, &n("sales_invoice"), 1)
        .await
        .unwrap();
    assert_eq!(
        dropped,
        vec![
            "v1_sales_invoice".to_owned(),
            "v1_sales_invoice__sales_invoice_line".to_owned()
        ]
    );
    assert!(
        columns_of(&client, "bi", "v2_sales_invoice")
            .await
            .iter()
            .all(|(c, _)| c != "posting_date")
    );
    assert_eq!(
        columns_of(&client, "bi", "v1_party").await.len(),
        3,
        "other views are untouched by retire"
    );
}

#[tokio::test]
async fn an_object_with_a_planned_name_that_is_not_a_view_is_refused() {
    let catalog = fixture_catalog(7);
    let Some(f) = fixture(&catalog).await else {
        return;
    };
    let client = admin(&f).await;
    client
        .batch_execute("CREATE SCHEMA bi; CREATE TABLE bi.v1_party (x integer)")
        .await
        .unwrap();
    let error = apply(&f.pool, &plan(&catalog, FIXTURE_POLICY))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "bi.not_a_view", "{error}");
}

#[tokio::test]
async fn odd_names_and_literals_work_end_to_end() {
    let order = EntityDef {
        name: n("order"),
        kind: EntityKind::Document,
        fields: vec![
            field("docstatus", FieldType::Int, false),
            field("select", text(), true),
            field("group", FieldType::Int, true),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let catalog = catalog_of(1, vec![order]);
    let Some(f) = fixture(&catalog).await else {
        return;
    };
    let text = "expose = [\"order\"]\n[entity.order]\nlabels = { select = \"it's a \\\\ odd\" }\n";
    let plan = plan(&catalog, text);
    apply(&f.pool, &plan).await.unwrap();
    let client = admin(&f).await;
    client
        .batch_execute("INSERT INTO \"order\" (id, version, docstatus, \"select\", \"group\") VALUES (gen_random_uuid(), 1, 1, 'x', 3)")
        .await
        .unwrap();
    let row = client
        .query_one("SELECT \"select\", \"group\" FROM bi.\"v1_order\"", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "x");
    assert_eq!(row.get::<_, i64>(1), 3);
    let label: String = client
        .query_one(
            "SELECT description FROM bi.catalogue WHERE column_name = 'select'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(label, "it's a \\ odd");
    assert_eq!(
        grant_role_sql(&n("ana")),
        "GRANT \"spot_bi_reader\" TO \"ana\""
    );
}
