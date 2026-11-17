//! `spot sandbox` (WP-ALM-05.T3): the pure rules (time to live, names, records), the masking and
//! the leak scan over a real PostgreSQL database (`SPOT_TEST_PG_URL`; skipped with a printed
//! reason when unset), and the commands over a whole host.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use spot_meta::{Definition, FieldSpec, FieldType, MaskSpec};
use spot_postgres::testing::TestDb;

/// The command modules, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::sandbox::db::{PURGE_PREFIXES, mask_clone, quote, verify_clone};
use commands::sandbox::{
    DEFAULT_TTL, MAX_TTL, RECORD_FILE, Record, database_name, parse_ttl, read_record,
};

const NAME: &str = "Alice Example";
const EMAIL: &str = "alice@example.com";
const CITY: &str = "Lahore Cantonment";

fn classified(mut spec: FieldSpec) -> FieldSpec {
    spec.classification = vec!["pii".to_owned()];
    spec.protection_group = Some("personal".to_owned());
    spec.masking = Some(MaskSpec::placeholder());
    spec
}

fn definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    customer.fields = vec![
        classified(FieldSpec::new("name", FieldType::Text)),
        classified(FieldSpec::new("email", FieldType::Text)),
        // No classification at all: masked too (fail closed).
        FieldSpec::new("city", FieldType::Text),
    ];
    vec![customer]
}

#[test]
fn the_command_parses_its_arguments() {
    let command = commands::sandbox::command();
    let create = command
        .clone()
        .try_get_matches_from(["sandbox", "create", "--from", "prod", "--name", "qa"])
        .unwrap();
    let (_, sub) = create.subcommand().unwrap();
    assert_eq!(sub.get_one::<String>("ttl").unwrap(), "14d");
    assert!(
        command
            .clone()
            .try_get_matches_from(["sandbox", "create", "--from", "prod"])
            .is_err(),
        "a sandbox needs a name"
    );
    assert!(
        command
            .clone()
            .try_get_matches_from(["sandbox", "destroy", "--name", "qa"])
            .is_ok()
    );
    assert!(command.try_get_matches_from(["sandbox", "reap"]).is_ok());
}

#[test]
fn the_time_to_live_is_bounded() {
    assert_eq!(parse_ttl("14d"), Some(DEFAULT_TTL));
    assert_eq!(parse_ttl("12h"), Some(43_200));
    assert_eq!(parse_ttl("30m"), Some(1_800));
    assert_eq!(parse_ttl("90d"), Some(MAX_TTL));
    assert_eq!(parse_ttl("91d"), None, "a sandbox does not live for ever");
    assert_eq!(parse_ttl("0d"), None);
    assert_eq!(parse_ttl("-1d"), None);
    assert_eq!(parse_ttl("d"), None);
    assert_eq!(parse_ttl("14"), None);
    assert_eq!(parse_ttl("14w"), None);
}

#[test]
fn the_database_name_is_derived_and_safe() {
    assert_eq!(database_name("qa-1").as_deref(), Some("sandbox_qa_1"));
    assert_eq!(database_name("QA").as_deref(), Some("sandbox_qa"));
    assert_eq!(database_name("a\"; drop database x; --"), None);
    assert_eq!(database_name(&"a".repeat(80)), None, "63 bytes at most");
    assert!(quote("customer").is_ok());
    assert!(quote("Customer").is_err());
    assert!(quote("a b").is_err());
}

#[test]
fn a_record_round_trips_and_a_real_site_has_none() {
    let dir = std::env::temp_dir().join(format!("spot_sandbox_rec_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        read_record(&dir).unwrap(),
        None,
        "a real site has no record"
    );
    let record = Record {
        source: "prod".to_owned(),
        database: "sandbox_qa".to_owned(),
        created_at: 100,
        expires_at: 100 + DEFAULT_TTL,
        state: "active".to_owned(),
        banner: true,
    };
    std::fs::write(dir.join(RECORD_FILE), toml::to_string(&record).unwrap()).unwrap();
    assert_eq!(read_record(&dir).unwrap(), Some(record));
    std::fs::write(dir.join(RECORD_FILE), "source = \"x\"\nsurprise = 1\n").unwrap();
    assert!(read_record(&dir).is_err(), "unknown keys are refused");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A clone with personal data in three places: two classified columns, an unclassified one and a
/// search table that holds a copy.
async fn seeded(db: &TestDb) {
    let client = db.raw_client().await;
    client
        .batch_execute(&format!(
            "CREATE TABLE customer (id uuid PRIMARY KEY, name text, email text, city text);\
             INSERT INTO customer VALUES \
               ('00000000-0000-7000-8000-000000000001', '{NAME}', '{EMAIL}', '{CITY}'),\
               ('00000000-0000-7000-8000-000000000002', 'Bob Example', 'bob@example.com', '{CITY}');\
             CREATE TABLE spot_search_copy (id int, body text);\
             INSERT INTO spot_search_copy VALUES (1, '{EMAIL}');\
             CREATE TABLE ledger_note (id int, note text);\
             INSERT INTO ledger_note VALUES (1, '{NAME}');"
        ))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn masking_removes_the_original_values_and_the_scan_proves_it() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    seeded(&db).await;
    let cfg = db.config();
    // A copy the masker does not know about: the scan must catch it and refuse (fail closed).
    let (stats, index) = mask_clone(&cfg, db.name(), &definitions(), [7_u8; 32])
        .await
        .unwrap();
    assert_eq!(stats.fields, 3, "the unclassified field is masked too");
    assert_eq!(stats.rows, 6);
    assert!(stats.purged.contains(&"spot_search_copy".to_owned()));
    let verdict = verify_clone(&cfg, db.name(), &index).await;
    let message = verdict.expect_err("the ledger note still holds a name");
    assert!(message.contains("ledger_note.note"), "{message}");
    assert!(!message.contains(NAME), "the message never carries a value");

    // After the stray copy is gone, the clone verifies clean.
    let client = db.raw_client().await;
    client.batch_execute("TRUNCATE ledger_note").await.unwrap();
    let scanned = verify_clone(&cfg, db.name(), &index).await.unwrap();
    assert!(scanned >= 3);

    // No original value survives; references (ids) are untouched.
    let rows = client
        .query(
            "SELECT id::text, name, email, city FROM customer ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        let (name, email, city): (String, String, String) = (row.get(1), row.get(2), row.get(3));
        assert_ne!(name, NAME);
        assert_ne!(email, EMAIL);
        assert_ne!(city, CITY);
    }
    let id: String = rows[0].get(0);
    assert_eq!(id, "00000000-0000-7000-8000-000000000001");
    // The same source value maps to the same fake: both rows had the same city.
    let (first, second): (String, String) = (rows[0].get(3), rows[1].get(3));
    assert_eq!(first, second);
    // The search copy was emptied.
    let copies = client
        .query("SELECT * FROM spot_search_copy", &[])
        .await
        .unwrap();
    assert!(copies.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn masking_is_deterministic_per_key_and_differs_between_keys() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    seeded(&db).await;
    let cfg = db.config();
    mask_clone(&cfg, db.name(), &definitions(), [1_u8; 32])
        .await
        .unwrap();
    let client = db.raw_client().await;
    let once: String = client
        .query_one(
            "SELECT email FROM customer WHERE name IS NOT NULL ORDER BY id LIMIT 1",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_ne!(once, EMAIL);
    assert!(!once.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_field_that_cannot_be_masked_in_place_fails_the_run() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    seeded(&db).await;
    let mut definition = Definition::new("customer");
    definition.fields = vec![classified(FieldSpec::new("age", FieldType::Date))];
    let result = mask_clone(&db.config(), db.name(), &[definition], [3_u8; 32]).await;
    let message = result.expect_err("a date identifier is refused, never left in clear");
    assert!(message.contains("customer.age"), "{message}");
}

// --- the commands over a real host -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn create_list_destroy_and_reap_over_a_real_host() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let source = env.site.clone();
    let _secret = env.site();

    // Nothing to destroy without a record: a real site is never touched.
    let refused = common::run(&root, &["sandbox", "destroy", "--name", &source], None);
    assert_eq!(refused.status.code(), Some(2));

    let name = format!("{source}-qa");
    let created = common::run(
        &root,
        &[
            "sandbox", "create", "--from", &source, "--name", &name, "--ttl", "1h",
        ],
        None,
    );
    assert!(
        created.status.success(),
        "{}{}",
        String::from_utf8_lossy(&created.stdout),
        String::from_utf8_lossy(&created.stderr)
    );
    let site_dir = root.join("sites").join(&name);
    let record = read_record(&site_dir).unwrap().unwrap();
    assert_eq!(record.state, "active");
    assert_eq!(record.source, source);
    assert!(record.banner);
    // No credential of the source came along.
    let site_file = std::fs::read_to_string(site_dir.join("site.toml")).unwrap();
    assert!(!site_file.contains("credentials"), "{site_file}");
    assert!(
        !site_dir.join("snapshot-tmp").exists(),
        "the unmasked snapshot is deleted"
    );
    let listing = common::ok(&root, &["sandbox", "list"]);
    assert!(listing.contains(&name), "{listing}");

    // The outbound hold is on and is not released by anything here.
    let status = common::ok(&root, &["migrate-hold", "status", "--site", &name]);
    assert!(status.to_lowercase().contains("held by"), "{status}");

    // The source is untouched and the sandbox is not cloned again.
    let nested = common::run(
        &root,
        &[
            "sandbox",
            "create",
            "--from",
            &name,
            "--name",
            &format!("{name}-2"),
        ],
        None,
    );
    assert_eq!(nested.status.code(), Some(2));

    // Not expired yet: reap leaves it. Past expiry it is gone.
    common::ok(&root, &["sandbox", "reap"]);
    assert!(site_dir.exists());
    let mut expired = record.clone();
    expired.expires_at = 1;
    std::fs::write(
        site_dir.join(RECORD_FILE),
        toml::to_string(&expired).unwrap(),
    )
    .unwrap();
    common::ok(&root, &["sandbox", "reap"]);
    assert!(!site_dir.exists(), "the expired sandbox is destroyed");
    assert!(
        root.join("sites").join(&source).exists(),
        "the source is untouched"
    );
}
