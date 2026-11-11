//! Shared setup for the real-database tests: one scratch database per test with the catalogue's
//! tables, the bridge the sides run on, and the rows to compare.
//!
//! `SPOT_TEST_PG_URL` is the admin connection URL; without it every test prints one `SKIPPED:` line
//! and returns, unless `SPOT_REQUIRE_PG=1`, which turns the absence into a failure (the same rule
//! as `spot-postgres`'s `testing` module).
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value as Json, json};
use spot_image_upcast::Image;
use spot_journal_rebuild::AppliedImage;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, ChildTableDef, Collation, EntityDef, EntityKind, FieldDef, FieldType, LinkCheck,
    LinkTarget, Name, StorageBackend,
};

/// The tenant both sides are opened for.
pub const TENANT: &str = "acme";

/// Another tenant, for the refusal tests.
pub const OTHER_TENANT: &str = "othercorp";

/// Connections the test pool opens.
pub const POOL: usize = 8;

/// A storage name.
pub fn n(name: &str) -> Name {
    Name::new(name).unwrap_or_else(|e| panic!("{e}"))
}

fn field(name: &str, ty: FieldType, nullable: bool) -> FieldDef {
    FieldDef {
        name: n(name),
        ty,
        nullable,
    }
}

fn text(max_chars: Option<u32>) -> FieldType {
    FieldType::Text { max_chars }
}

fn decimal() -> FieldType {
    FieldType::Decimal {
        precision: 18,
        scale: 2,
    }
}

fn link() -> FieldType {
    FieldType::Link {
        target: LinkTarget::Entity(n("party")),
        check: LinkCheck::Database,
    }
}

/// The catalogue the tests read and write: a document with every storage type and one child
/// table, the entity its link points at, and one ledger-like entity for the fingerprints.
pub fn catalog() -> Arc<Catalog> {
    let entities = vec![
        EntityDef {
            name: n("invoice"),
            kind: EntityKind::Document,
            fields: vec![
                field("code", text(Some(32)), false),
                field("total", decimal(), true),
                field("issued", FieldType::Date, true),
                field("at", FieldType::Time, true),
                field("posted_at", FieldType::DateTime, true),
                field("party", link(), true),
                field("notes", FieldType::Json, true),
                field("blob", FieldType::Bytes, true),
                field("archived", FieldType::Bool, true),
                field("stamp", FieldType::Int, true),
            ],
            children: vec![ChildTableDef {
                field: n("lines"),
                entity: n("invoice_line"),
            }],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        },
        EntityDef {
            name: n("invoice_line"),
            kind: EntityKind::Child {
                parent: n("invoice"),
            },
            fields: vec![
                field("qty", FieldType::Int, false),
                field("label", text(Some(64)), true),
            ],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        },
        EntityDef {
            name: n("party"),
            kind: EntityKind::Document,
            fields: vec![field("name", text(Some(80)), false)],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        },
        EntityDef {
            name: n("gl_entry"),
            kind: EntityKind::Document,
            fields: vec![
                field("account", text(Some(32)), false),
                field("amount", decimal(), false),
            ],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        },
    ];
    Arc::new(
        Catalog::new(
            1,
            Collation::new("en").unwrap_or_else(|e| panic!("{e}")),
            entities,
        )
        .unwrap_or_else(|e| panic!("{e}")),
    )
}

/// One scratch database, its tables from the catalogue, and the bridge.
pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
    pub bridge: Arc<Bridge>,
}

/// A fixture, or `None` (with one printed line) when no server is configured.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("the test runtime");
    let db = rt.block_on(TestDb::create())?;
    let mut config = db.config();
    config.pool_max_size = POOL;
    let backend = PgBackend::connect_unchecked(config).expect("the backend");
    rt.block_on(async {
        backend
            .prepare(&catalog())
            .await
            .expect("the catalogue's tables");
    });
    let bridge = Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "journal_audit_pg_test".to_owned(),
                workers: 2,
                max_in_flight: 4,
                queue_capacity: 8,
                default_deadline: Duration::from_secs(10),
                // The live side's session spans a whole test, so the ceiling must allow it.
                max_deadline: Duration::from_secs(120),
            },
            &backend,
        )
        .expect("the bridge"),
    );
    Some(Fixture { rt, db, bridge })
}

impl Fixture {
    /// A plain driver client on the scratch database.
    pub fn client(&self) -> tokio_postgres::Client {
        self.rt.block_on(self.db.raw_client())
    }

    /// Runs one statement and ignores its row count.
    pub fn run(&self, client: &tokio_postgres::Client, sql: &str) {
        self.rt
            .block_on(client.batch_execute(sql))
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// Moves the change feed's head: its generation and its last published position.
    pub fn set_head(&self, client: &tokio_postgres::Client, generation: i64, seq: i64) {
        self.run(
            client,
            &format!("UPDATE spot_meta SET value = '{generation}' WHERE key = 'feed_generation'"),
        );
        self.run(
            client,
            &format!("UPDATE spot_feed_state SET last_seq = {seq} WHERE log = 'changes'"),
        );
    }

    /// Inserts one row: each column with the cast its text value needs.
    pub fn insert(
        &self,
        client: &tokio_postgres::Client,
        table: &str,
        columns: &[(&str, &str, Option<&str>)],
    ) {
        let names: Vec<String> = columns
            .iter()
            .map(|(name, _, _)| format!("\"{name}\""))
            .collect();
        let values: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(at, (_, cast, _))| {
                if *cast == "base64" {
                    format!("decode(${}::text, 'base64')", at + 1)
                } else {
                    format!("${}::text::{cast}", at + 1)
                }
            })
            .collect();
        let sql = format!(
            "INSERT INTO \"{table}\" ({}) VALUES ({})",
            names.join(", "),
            values.join(", ")
        );
        let params: Vec<Option<String>> = columns
            .iter()
            .map(|(_, _, value)| value.map(str::to_owned))
            .collect();
        let bound: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
            .iter()
            .map(|value| value as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        self.rt
            .block_on(client.execute(&sql, &bound))
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A canonical record id.
pub fn uuid(n: u64) -> String {
    format!("01900000-0000-7000-8000-{n:012x}")
}

/// The body of invoice `n`, in the ADR-055 encoding: what both sides must digest.
pub fn invoice_body(n: u64, party: u64) -> Map<String, Json> {
    let mut body = Map::new();
    body.insert("code".to_owned(), json!(format!("INV-{n}")));
    body.insert("total".to_owned(), json!("1234.50"));
    body.insert("issued".to_owned(), json!("2024-01-02"));
    body.insert("at".to_owned(), json!("12:30:00.5"));
    body.insert("posted_at".to_owned(), json!("2024-01-02T03:04:05Z"));
    body.insert("party".to_owned(), json!(uuid(party)));
    body.insert("notes".to_owned(), json!({"k": [1, 2]}));
    // "\0probe" in padded standard base64.
    body.insert("blob".to_owned(), json!("AHByb2Jl"));
    body.insert("archived".to_owned(), json!(true));
    body.insert("stamp".to_owned(), json!(7));
    body.insert(
        "lines".to_owned(),
        json!([{"id": uuid(9_000 + n), "idx": 1, "qty": 2, "label": "a"}]),
    );
    body
}

/// The body of a party.
pub fn party_body(name: &str) -> Map<String, Json> {
    let mut body = Map::new();
    body.insert("name".to_owned(), json!(name));
    body
}

/// The body of a ledger entry.
pub fn entry_body(account: &str, amount: &str) -> Map<String, Json> {
    let mut body = Map::new();
    body.insert("account".to_owned(), json!(account));
    body.insert("amount".to_owned(), json!(amount));
    body
}

/// Writes one party into the live database.
pub fn put_party(fx: &Fixture, client: &tokio_postgres::Client, n: u64, name: &str) {
    fx.insert(
        client,
        "party",
        &[
            ("id", "uuid", Some(&uuid(n))),
            ("version", "bigint", Some("1")),
            ("name", "text", Some(name)),
        ],
    );
}

/// Writes one invoice, with its child row, into the live database.
pub fn put_invoice(fx: &Fixture, client: &tokio_postgres::Client, n: u64, party: u64) {
    let id = uuid(n);
    let version = "1";
    let code = format!("INV-{n}");
    let party_id = uuid(party);
    let cid = uuid(9_000 + n);
    fx.insert(
        client,
        "invoice",
        &[
            ("id", "uuid", Some(&id)),
            ("version", "bigint", Some(version)),
            ("code", "text", Some(&code)),
            ("total", "numeric(18,2)", Some("1234.50")),
            ("issued", "date", Some("2024-01-02")),
            ("at", "time", Some("12:30:00.5")),
            ("posted_at", "timestamptz", Some("2024-01-02T03:04:05Z")),
            ("party", "uuid", Some(&party_id)),
            ("notes", "jsonb", Some(r#"{"k": [1, 2]}"#)),
            ("blob", "base64", Some("AHByb2Jl")),
            ("archived", "boolean", Some("true")),
            ("stamp", "bigint", Some("7")),
        ],
    );
    fx.insert(
        client,
        "invoice_line",
        &[
            ("id", "uuid", Some(&cid)),
            ("parent", "uuid", Some(&id)),
            ("parent_field", "text", Some("lines")),
            ("idx", "integer", Some("1")),
            ("qty", "bigint", Some("2")),
            ("label", "text", Some("a")),
        ],
    );
}

/// Writes one ledger entry into the live database.
pub fn put_entry(
    fx: &Fixture,
    client: &tokio_postgres::Client,
    n: u64,
    account: &str,
    amount: &str,
) {
    fx.insert(
        client,
        "gl_entry",
        &[
            ("id", "uuid", Some(&uuid(n))),
            ("version", "bigint", Some("1")),
            ("account", "text", Some(account)),
            ("amount", "numeric(18,2)", Some(amount)),
        ],
    );
}

/// The image the rebuild would hand a sink for one stored record.
pub fn image(entity: &str, n: u64, version: u64, body: Map<String, Json>) -> AppliedImage {
    AppliedImage {
        entity: entity.to_owned(),
        id: uuid(n),
        version,
        position: spot_events_contract::envelope::Position {
            generation: 1,
            seq: n,
        },
        image: Image::new(entity, 1, body),
    }
}
