//! The tables carry the `spot_` prefix `spot_pg_migrate` requires, a schema that still holds the
//! old names is upgraded without losing a row, and `prepare` is safe to run again and from several
//! nodes at once.
//!
//! Every test here needs a server and skips cleanly without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use spot_link_exchange::{
    Inbox, InboxStore, Lifetimes, MemoryQuarantine, NoticeFeed, Outbox, OutboxStore, ReceiveStatus,
    Retention, Submit,
};
use spot_link_exchange_pg::PgExchange;
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, Collation};
use spot_tenant_link::{ExchangePayload, FieldName, FieldValue};
use tokio_postgres::Client;

/// The tables as the crate created them before the prefix: the same shape, the old names.
const OLD_DDL: &str = "\
CREATE TABLE link_cursor (\
    name text PRIMARY KEY CHECK (name IN ('notice', 'event')), \
    value bigint NOT NULL CHECK (value >= 0)); \
CREATE TABLE link_outbox (\
    id uuid PRIMARY KEY, \
    link uuid NOT NULL, \
    conversation text NOT NULL, \
    seq bigint NOT NULL CHECK (seq >= 1), \
    state text NOT NULL CHECK (state IN ('queued', 'sent', 'acknowledged', 'applied', 'rejected', 'expired')), \
    state_at bigint NOT NULL CHECK (state_at >= 0), \
    live boolean NOT NULL, \
    filler_id uuid UNIQUE, \
    version bigint NOT NULL CHECK (version >= 1), \
    purged_at bigint, \
    entry jsonb NOT NULL, \
    UNIQUE (link, conversation, seq)); \
CREATE INDEX link_outbox_live ON link_outbox (link, conversation, seq) WHERE live; \
CREATE INDEX link_outbox_expiry ON link_outbox (state_at) \
    WHERE NOT live AND purged_at IS NULL; \
CREATE TABLE link_outbox_event (\
    pos bigint PRIMARY KEY CHECK (pos >= 1), \
    event jsonb NOT NULL); \
CREATE TABLE link_inbox_conv (\
    link uuid NOT NULL, \
    conversation text NOT NULL, \
    next_seq bigint NOT NULL CHECK (next_seq >= 1), \
    PRIMARY KEY (link, conversation)); \
CREATE TABLE link_inbox (\
    id uuid PRIMARY KEY, \
    link uuid NOT NULL, \
    conversation text NOT NULL, \
    seq bigint NOT NULL CHECK (seq >= 1), \
    status text NOT NULL CHECK (status IN ('pending_apply', 'applied', 'failed')), \
    received_at bigint NOT NULL CHECK (received_at >= 0), \
    purged_at bigint, \
    entry jsonb NOT NULL, \
    UNIQUE (link, conversation, seq)); \
CREATE INDEX link_inbox_pending ON link_inbox (link, conversation, seq) \
    WHERE status = 'pending_apply'; \
CREATE INDEX link_inbox_age ON link_inbox (received_at) \
    WHERE status = 'applied' AND purged_at IS NULL; \
CREATE TABLE link_parked (\
    id uuid PRIMARY KEY, \
    link uuid NOT NULL, \
    conversation text NOT NULL, \
    seq bigint NOT NULL CHECK (seq >= 1), \
    parked_at bigint NOT NULL CHECK (parked_at >= 0), \
    entry jsonb NOT NULL, \
    UNIQUE (link, conversation, seq)); \
CREATE INDEX link_parked_age ON link_parked (parked_at); \
CREATE TABLE link_seen (\
    id uuid PRIMARY KEY, \
    link uuid NOT NULL, \
    kind text NOT NULL CHECK (kind IN ('stored', 'parked', 'rejected')), \
    detail text NOT NULL); \
CREATE TABLE link_notice (\
    cursor bigint PRIMARY KEY CHECK (cursor >= 1), \
    topic text NOT NULL, \
    message_id uuid NOT NULL, \
    conversation text NOT NULL, \
    source text NOT NULL, \
    received_at bigint NOT NULL CHECK (received_at >= 0)); \
CREATE TABLE link_notice_ack (\
    consumer text PRIMARY KEY CHECK (length(consumer) BETWEEN 1 AND 128), \
    cursor bigint NOT NULL CHECK (cursor >= 0));";

/// The (old, new) name of every table.
const PAIRS: [(&str, &str); 9] = [
    ("link_cursor", "spot_link_cursor"),
    ("link_outbox", "spot_link_outbox"),
    ("link_outbox_event", "spot_link_outbox_event"),
    ("link_inbox_conv", "spot_link_inbox_conv"),
    ("link_inbox", "spot_link_inbox"),
    ("link_parked", "spot_link_parked"),
    ("link_seen", "spot_link_seen"),
    ("link_notice", "spot_link_notice"),
    ("link_notice_ack", "spot_link_notice_ack"),
];

fn schema(n: u8) -> String {
    tenant_id(n).schema_name()
}

/// A raw client whose search path is tenant `n`'s schema.
async fn client(e: &Env, n: u8) -> Client {
    let client = e.db.raw_client().await;
    client
        .batch_execute(&format!("SET search_path TO \"{}\"", schema(n)))
        .await
        .unwrap();
    client
}

/// Every relation, index and constraint name of tenant `n`'s schema, sorted.
fn names(e: &Env, n: u8) -> Vec<String> {
    let schema = schema(n);
    e.rt.block_on(async {
        let mut out: Vec<String> =
            e.db.raw_client()
                .await
                .query(
                    "SELECT 'rel:' || c.relkind::text || ':' || c.relname::text FROM pg_class c \
                 JOIN pg_namespace s ON s.oid = c.relnamespace WHERE s.nspname = $1 \
                 UNION SELECT 'con:' || k.conname::text FROM pg_constraint k \
                 JOIN pg_namespace s ON s.oid = k.connamespace WHERE s.nspname = $1 \
                 UNION SELECT 'idx:' || ic.relname::text FROM pg_index i \
                 JOIN pg_class ic ON ic.oid = i.indexrelid \
                 JOIN pg_namespace s ON s.oid = ic.relnamespace WHERE s.nspname = $1",
                    &[&schema],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
        out.sort();
        out
    })
}

fn table_names(e: &Env, n: u8) -> Vec<String> {
    let schema = schema(n);
    e.rt.block_on(async {
        e.db.raw_client()
            .await
            .query(
                "SELECT table_name::text FROM information_schema.tables \
                 WHERE table_schema = $1 AND table_type = 'BASE TABLE' \
                 ORDER BY table_name::text COLLATE \"C\"",
                &[&schema],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect()
    })
}

/// The `link` of every row of the three link-keyed tables, as text.
fn e_links(e: &Env) -> Vec<String> {
    e.rt.block_on(async {
        let client = client(e, 1).await;
        let mut all = Vec::new();
        for table in ["spot_link_inbox", "spot_link_parked", "spot_link_seen"] {
            let rows = client
                .query(&format!("SELECT link::text FROM {table}"), &[])
                .await
                .unwrap();
            all.extend(rows.iter().map(|row| row.get::<_, String>(0)));
        }
        all
    })
}

/// The columns of `table`'s primary key, in key order.
fn key_columns(e: &Env, n: u8, table: &str) -> Vec<String> {
    e.rt.block_on(async {
        client(e, n)
            .await
            .query(
                "SELECT a.attname::text FROM pg_constraint k \
                 JOIN pg_class c ON c.oid = k.conrelid \
                 JOIN unnest(k.conkey) WITH ORDINALITY AS key(attnum, ord) ON true \
                 JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = key.attnum \
                 WHERE c.relname = $1 AND k.contype = 'p' ORDER BY key.ord",
                &[&table],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    })
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Every row of every table as text, in a fixed order: compares the stored data exactly.
fn dump(e: &Env, n: u8, new_names: bool) -> Vec<Vec<String>> {
    e.rt.block_on(async {
        let client = client(e, n).await;
        let mut all = Vec::new();
        for (old, new) in PAIRS {
            let table = if new_names { new } else { old };
            // A notice also names its link now (a column the old schema never had, checked by
            // `an_upgraded_notice_takes_the_link_of_its_inbox_row`): compare the columns it had.
            let shown = if new == "spot_link_notice" {
                "(t.cursor, t.topic, t.message_id, t.conversation, t.source, t.received_at)"
            } else {
                "t"
            };
            let rows = client
                .query(
                    &format!(
                        "SELECT {shown}::text FROM {table} t ORDER BY {shown}::text COLLATE \"C\""
                    ),
                    &[],
                )
                .await
                .unwrap();
            all.push(rows.iter().map(|r| r.get(0)).collect());
        }
        all
    })
}

fn payload(text: &str) -> ExchangePayload {
    let mut p = ExchangePayload::new();
    p.insert(
        FieldName::new("message.body").unwrap(),
        FieldValue::Text(text.to_owned()),
    );
    p
}

/// A source database whose tenant-1 schema holds a row in every table, written through the crate's
/// own API (so the rows are real): three messages queued, delivered 3 then 1 (3 parks, 1 applies),
/// a receipt back, a notice acknowledged.
fn populated_source() -> Env {
    let rt = runtime();
    let e = rt.block_on(env(rt.clone())).expect("a server");
    let w = World::new();
    let ex = e.exchange(1);
    let store = ex.outbox();
    let outbox = Outbox::new(
        &store,
        &w.audit_a,
        &w.signer_a,
        &w.ids,
        Lifetimes::new(Retention::from_days(3).unwrap()),
    );
    let conv = conversation("orders");
    let topic = message_topic();
    for n in 1..=3u64 {
        outbox
            .enqueue_message(
                &Submit {
                    link: &w.link_a,
                    conversation: &conv,
                    topic: &topic,
                    schema_version: 1,
                    now: T0 + n,
                    actor: "user:alice",
                    oracle: None,
                },
                &payload(&format!("m{n}")),
            )
            .unwrap();
    }
    let batch = outbox
        .next_batch(&w.link_a, T0 + 100, 10, "system", None)
        .unwrap();
    let inbox_store = ex.inbox();
    let quarantine = MemoryQuarantine::new();
    let inbox = Inbox::new(&inbox_store, &w.audit_b, &quarantine, &w.signer_b);
    let third = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[2], T0 + 200)
        .unwrap();
    assert_eq!(third.status, ReceiveStatus::Parked);
    let first = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], T0 + 201)
        .unwrap();
    assert_eq!(first.status, ReceiveStatus::Applied);
    for receipt in &first.receipts {
        outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), receipt, T0 + 400)
            .unwrap();
    }
    ex.notices().ack("consumer-1", 1).unwrap();
    // Every table holds at least one row.
    for (_, new) in PAIRS {
        assert!(e.count(1, new) > 0, "{new} is empty in the source");
    }
    e
}

/// Creates tenant 1's schema in `target` with the OLD names and copies every row of `source` in.
fn make_old(target: &Env, source: &Env) {
    target.provision_schema(1);
    target.rt.block_on(async {
        let dst = client(target, 1).await;
        dst.batch_execute(OLD_DDL).await.unwrap();
        let src = client(source, 1).await;
        for (old, new) in PAIRS {
            let rows = src
                .query(&format!("SELECT to_jsonb(t)::text FROM {new} t"), &[])
                .await
                .unwrap();
            for row in rows {
                let json: String = row.get(0);
                dst.execute(
                    &format!(
                        "INSERT INTO {old} SELECT * FROM jsonb_populate_record(NULL::{old}, \
                         $1::text::jsonb)"
                    ),
                    &[&json],
                )
                .await
                .unwrap();
            }
        }
    });
}

fn fresh_exchange(e: &Env, n: u8) -> PgExchange {
    PgExchange::new(e.pool.clone(), e.rt.clone(), tenant_id(n))
}

/// A fresh scratch database (`None`, skipped, without a server).
fn scratch() -> Option<Env> {
    let rt = runtime();
    rt.block_on(env(rt.clone()))
}

#[test]
fn every_object_the_crate_creates_is_spot_prefixed() {
    let Some(e) = scratch() else {
        return;
    };
    e.exchange(1);
    assert_eq!(
        table_names(&e, 1),
        sorted(PAIRS.iter().map(|p| p.1.to_owned()).collect())
    );
    for name in names(&e, 1) {
        let bare = name.rsplit(':').next().unwrap();
        assert!(bare.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
}

/// The migrator refuses every table that is not `spot_` and not in the catalogue. It reads the
/// schema of its connection, so the scratch database's default search path is the tenant's.
async fn migrator_refusals(e: &Env, n: u8) -> usize {
    e.db.admin_client()
        .await
        .batch_execute(&format!(
            "ALTER DATABASE \"{}\" SET search_path TO \"{}\"",
            e.db.name(),
            schema(n)
        ))
        .await
        .unwrap();
    let backend = PgBackend::connect(e.db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    assert!(live.tables.contains_key("spot_link_seen"), "{live:?}");
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    plan(&live, &wanted).refused.len()
}

#[test]
fn a_prepared_schema_passes_the_migrator() {
    let Some(e) = scratch() else {
        return;
    };
    e.exchange(1);
    assert_eq!(e.rt.block_on(migrator_refusals(&e, 1)), 0);
}

#[test]
fn an_upgraded_schema_passes_the_migrator() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    old.rt.block_on(fresh_exchange(&old, 1).prepare()).unwrap();
    assert_eq!(old.rt.block_on(migrator_refusals(&old, 1)), 0);
}

#[test]
fn old_named_tables_are_renamed_with_every_row_kept() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    let before = dump(&old, 1, false);
    assert!(before.iter().all(|rows| !rows.is_empty()), "{before:?}");
    assert_eq!(table_names(&old, 1).len(), 9);

    let ex = fresh_exchange(&old, 1);
    old.rt.block_on(ex.prepare()).unwrap();

    // Byte for byte the same rows under the new names, the old names gone.
    assert_eq!(dump(&old, 1, true), before);
    assert_eq!(
        table_names(&old, 1),
        sorted(PAIRS.iter().map(|p| p.1.to_owned()).collect())
    );
    // The crate's own API reads them: the replay memory, the parked message, the notice, the ack.
    let reader = fresh_exchange(&source, 1);
    assert_eq!(
        ex.notices().read_from(0, 10).unwrap(),
        reader.notices().read_from(0, 10).unwrap()
    );
    assert_eq!(ex.notices().acked("consumer-1").unwrap(), 1);
    assert_eq!(ex.inbox().parked().unwrap().len(), 1);
    assert_eq!(
        ex.outbox().live(None).unwrap().len(),
        reader.outbox().live(None).unwrap().len()
    );
    assert_eq!(ex.outbox().events_from(0, 10).unwrap().len(), 1);
}

#[test]
fn an_upgraded_schema_has_the_catalogue_of_a_fresh_one() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    old.rt.block_on(fresh_exchange(&old, 1).prepare()).unwrap();
    let fresh = scratch().unwrap();
    fresh.exchange(1);

    let upgraded = names(&old, 1);
    assert_eq!(upgraded, names(&fresh, 1));
    for expected in [
        "idx:spot_link_outbox_live",
        "idx:spot_link_outbox_expiry",
        "idx:spot_link_inbox_pending",
        "idx:spot_link_inbox_age",
        "idx:spot_link_parked_age",
        "idx:spot_link_outbox_pkey",
        "idx:spot_link_outbox_filler_id_key",
        "idx:spot_link_outbox_link_conversation_seq_key",
        "idx:spot_link_seen_pkey",
        "idx:spot_link_inbox_conv_pkey",
        "con:spot_link_cursor_name_check",
        "con:spot_link_outbox_state_check",
        "con:spot_link_notice_ack_consumer_check",
        "con:spot_link_seen_kind_check",
    ] {
        assert!(upgraded.iter().any(|n| n == expected), "missing {expected}");
    }
    assert!(
        !upgraded
            .iter()
            .any(|n| n.rsplit(':').next().unwrap().starts_with("link")),
        "an old name is left: {upgraded:?}"
    );
}

#[test]
fn preparing_again_changes_nothing() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    let ex = fresh_exchange(&old, 1);
    old.rt.block_on(ex.prepare()).unwrap();
    let before_names = names(&old, 1);
    let before_rows = dump(&old, 1, true);
    for _ in 0..3 {
        old.rt.block_on(ex.prepare()).unwrap();
    }
    assert_eq!(names(&old, 1), before_names);
    assert_eq!(dump(&old, 1, true), before_rows);
}

#[test]
fn nodes_preparing_at_once_upgrade_once_and_lose_nothing() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    let before = dump(&old, 1, false);
    let fresh = scratch().unwrap();
    fresh.exchange(1);
    old.rt.block_on(async {
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let ex = fresh_exchange(&old, 1);
            tasks.push(tokio::spawn(async move { ex.prepare().await }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
    });
    assert_eq!(dump(&old, 1, true), before);
    assert_eq!(names(&old, 1), names(&fresh, 1));
}

#[test]
fn both_names_present_is_refused_and_nothing_is_touched() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    old.rt.block_on(async {
        client(&old, 1)
            .await
            .batch_execute(
                "CREATE TABLE spot_link_seen (LIKE link_seen INCLUDING ALL); \
                 INSERT INTO spot_link_seen SELECT * FROM link_seen",
            )
            .await
            .unwrap();
    });
    let names_before = names(&old, 1);
    let old_rows = dump(&old, 1, false);

    let error = old
        .rt
        .block_on(fresh_exchange(&old, 1).prepare())
        .expect_err("no merge by guess");
    let text = error.to_string();
    assert!(
        text.contains("`link_seen`") && text.contains("`spot_link_seen`"),
        "{text}"
    );
    assert_eq!(names(&old, 1), names_before);
    assert_eq!(dump(&old, 1, false), old_rows);
}

#[test]
fn a_foreign_table_with_an_old_name_is_not_renamed() {
    let Some(e) = scratch() else {
        return;
    };
    e.provision_schema(1);
    e.rt.block_on(async {
        client(&e, 1)
            .await
            .batch_execute(
                "CREATE TABLE link_seen (id text PRIMARY KEY, secret text); \
                 INSERT INTO link_seen VALUES ('x', 'do-not-print')",
            )
            .await
            .unwrap();
    });
    let error =
        e.rt.block_on(fresh_exchange(&e, 1).prepare())
            .expect_err("a foreign table is left alone");
    let text = error.to_string();
    assert!(text.contains("`link_seen`"), "{text}");
    assert!(!text.contains("do-not-print"), "{text}");
    // The refusal rolled everything back: no new table was created either.
    assert_eq!(table_names(&e, 1), vec!["link_seen".to_owned()]);
}

#[test]
fn a_taken_target_name_is_refused_and_nothing_is_renamed() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    old.rt.block_on(async {
        client(&old, 1)
            .await
            .batch_execute("CREATE SEQUENCE spot_link_outbox_live")
            .await
            .unwrap();
    });
    let names_before = names(&old, 1);
    let error = old
        .rt
        .block_on(fresh_exchange(&old, 1).prepare())
        .expect_err("a taken name is never reused");
    assert!(
        error.to_string().contains("`spot_link_outbox_live`"),
        "{error}"
    );
    assert_eq!(names(&old, 1), names_before);
}

#[test]
fn an_old_table_of_another_schema_is_never_touched() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    // Another tenant's schema, prepared on its own: tenant 1's old tables stay as they are.
    old.provision_schema(2);
    old.rt.block_on(fresh_exchange(&old, 2).prepare()).unwrap();
    assert_eq!(table_names(&old, 2).len(), 9);
    assert_eq!(
        table_names(&old, 1),
        sorted(PAIRS.iter().map(|p| p.0.to_owned()).collect())
    );
}

#[test]
fn an_upgraded_schema_keys_the_dedupe_by_link_and_id() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    old.rt.block_on(fresh_exchange(&old, 1).prepare()).unwrap();
    // The dedupe of a message id is per link now, for the seen table and for the two tables that
    // hold the message itself; every row kept its link and its value.
    for table in ["spot_link_inbox", "spot_link_parked", "spot_link_seen"] {
        assert_eq!(key_columns(&old, 1, table), ["link", "id"], "{table}");
    }
    // Every row kept its link (the rows themselves are compared by the rename test above).
    let links: Vec<String> = e_links(&old);
    assert!(links.iter().all(|link| !link.is_empty()));
    // And the upgrade is repeatable: the keys do not move again.
    old.rt.block_on(fresh_exchange(&old, 1).prepare()).unwrap();
    assert_eq!(key_columns(&old, 1, "spot_link_seen"), ["link", "id"]);
}

#[test]
fn an_upgraded_notice_takes_the_link_of_its_inbox_row() {
    if scratch().is_none() {
        return;
    }
    let source = populated_source();
    let old = scratch().unwrap();
    make_old(&old, &source);
    // The old notice table has no `link` column at all.
    old.rt.block_on(async {
        let client = client(&old, 1).await;
        client
            .batch_execute("ALTER TABLE link_notice DROP COLUMN IF EXISTS link")
            .await
            .unwrap();
    });
    old.rt.block_on(fresh_exchange(&old, 1).prepare()).unwrap();
    let w = World::new();
    let notices = fresh_exchange(&old, 1).notices().read_from(0, 10).unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].link, w.link_b.id);
    // And repeating the upgrade changes nothing.
    old.rt.block_on(fresh_exchange(&old, 1).prepare()).unwrap();
    assert_eq!(
        fresh_exchange(&old, 1).notices().read_from(0, 10).unwrap(),
        notices
    );
}
