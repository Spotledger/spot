//! The body cap is the one PostgreSQL measures: `octet_length(body::text)`, the text of the
//! stored jsonb (a space after every `:` and `,`). A body exactly at the cap is stored; one byte
//! over, or over it only in jsonb text, is `TooLong` with nothing stored and nothing leaked.
//! Needs a PostgreSQL server (`SPOT_REQUIRE_PG=1` turns a missing one into a failure).

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::{Value as Json, json};
use spot_bpm_conversations::{ConversationError, ConversationRecord, ConversationStore};
use spot_bpm_conversations_pg::MAX_BODY_BYTES;

/// A record whose last event is a received one carrying `payload`.
fn with_payload(name: &str, payload: Json) -> ConversationRecord {
    let c = contract();
    let mut r = open_record(link(1), name, 1);
    r.append_received(&c, "approve", mid(2), 2_000, payload)
        .unwrap();
    r
}

fn blob(n: usize) -> Json {
    json!({ "blob": "x".repeat(n) })
}

/// The database's measure of the stored body of `(tenant, name)`.
async fn measure(fx: &Fixture, tenant: &str, name: &str) -> usize {
    let client = fx.db.raw_client().await;
    let row = client
        .query_one(
            "SELECT octet_length(body::text) FROM spot_bpm_conversation \
             WHERE tenant = $1 AND conversation = $2",
            &[&tenant, &name],
        )
        .await
        .unwrap();
    usize::try_from(row.get::<_, i32>(0)).unwrap()
}

async fn rows(fx: &Fixture) -> i64 {
    let client = fx.db.raw_client().await;
    client
        .query_one("SELECT count(*) FROM spot_bpm_conversation", &[])
        .await
        .unwrap()
        .get(0)
}

/// No PostgreSQL text, constraint name, code or body fragment in a refusal.
fn assert_clean(err: &ConversationError) {
    let shown = format!("{err} {err:?}");
    for leak in [
        "xxxx",
        "violates",
        "check",
        "23514",
        "octet_length",
        "spot_bpm_conversation",
    ] {
        assert!(!shown.contains(leak), "{leak} in {shown}");
    }
}

#[test]
fn a_body_is_capped_at_the_byte_postgres_counts() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_store(fx.db.config(), "acme");
    let other = sibling(&bridge, fx.db.config(), "globex");

    // Probe the fixed part (conversation names all have five letters: the name is in the body) of the body in a tenant of its own: the body grows by exactly one byte
    // per payload character, which the second probe proves.
    let (_pb, probe) = open_store(fx.db.config(), "probe");
    probe.create(with_payload("probe", blob(0))).unwrap();
    probe.create(with_payload("prob1", blob(1000))).unwrap();
    let base = fx.rt.block_on(measure(&fx, "probe", "probe"));
    assert_eq!(fx.rt.block_on(measure(&fx, "probe", "prob1")), base + 1000);
    let fit = MAX_BODY_BYTES - base;

    // Exactly the cap: stored, and read back equal.
    let exact = with_payload("exact", blob(fit));
    let stored = store.create(exact).unwrap();
    assert_eq!(
        fx.rt.block_on(measure(&fx, "acme", "exact")),
        MAX_BODY_BYTES
    );
    assert_eq!(
        store.load(&link(1), &conv("exact")).unwrap().unwrap(),
        stored
    );
    let before = fx.rt.block_on(rows(&fx));

    // One byte more in the database's measure: refused, nothing stored. The cap is per record: a
    // second tenant holds the same-size record too.
    let over = with_payload("over0", blob(fit + 1));
    let err = store.create(over.clone()).unwrap_err();
    assert_eq!(err, ConversationError::TooLong);
    assert_clean(&err);
    assert!(store.load(&link(1), &conv("over0")).unwrap().is_none());
    assert_eq!(fx.rt.block_on(rows(&fx)), before);
    other.create(with_payload("exact", blob(fit))).unwrap();
    assert_eq!(other.create(over), Err(ConversationError::TooLong));

    // A save that grows a stored record over the cap is refused and the record is unchanged.
    let mut grown = stored.clone();
    grown.events.last_mut().unwrap().payload = Some(blob(fit + 1));
    assert_eq!(store.save(grown), Err(ConversationError::TooLong));
    assert_eq!(
        store.load(&link(1), &conv("exact")).unwrap().unwrap(),
        stored
    );

    // Far over the cap: refused before any statement runs.
    let err = store
        .create(with_payload("huge0", blob(MAX_BODY_BYTES * 2)))
        .unwrap_err();
    assert_eq!(err, ConversationError::TooLong);
    assert_clean(&err);
    assert!(store.load(&link(1), &conv("huge0")).unwrap().is_none());
}

/// An object of `n` small keys: many separators, so jsonb text is `2 * n` bytes longer than the
/// compact text.
fn keys(n: usize) -> Json {
    Json::Object(
        (0..n)
            .map(|i| (format!("k{i:06}"), json!(1)))
            .collect::<serde_json::Map<_, _>>(),
    )
}

#[test]
fn a_body_over_the_cap_only_in_jsonb_text_is_too_long() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let (_pb, probe) = open_store(fx.db.config(), "probe");
    probe.create(with_payload("p0", keys(0))).unwrap();
    let base = fx.rt.block_on(measure(&fx, "probe", "p0"));

    // Each entry is `"k000000":1` (11 bytes) plus a comma compact, and 2 more in jsonb text.
    let per_entry_compact = 12;
    let n = (MAX_BODY_BYTES - base) / per_entry_compact - 10;
    let compact = base + n * per_entry_compact;
    assert!(compact < MAX_BODY_BYTES, "compact text is under the cap");
    assert!(
        compact + 2 * n > MAX_BODY_BYTES,
        "jsonb text is over it: the case that used to fail in the CHECK"
    );
    let before = fx.rt.block_on(rows(&fx));
    let err = store.create(with_payload("spread", keys(n))).unwrap_err();
    assert_eq!(err, ConversationError::TooLong);
    assert_clean(&err);
    assert_eq!(fx.rt.block_on(rows(&fx)), before);

    // Trim until the database's measure fits: stored, and the measure is at most the cap.
    let fits = n - (compact + 2 * n - MAX_BODY_BYTES) / (per_entry_compact + 2) - 2;
    let stored = store.create(with_payload("fits", keys(fits))).unwrap();
    let measured = fx.rt.block_on(measure(&fx, "acme", "fits"));
    assert!(measured <= MAX_BODY_BYTES && measured > MAX_BODY_BYTES - 100);
    assert_eq!(
        store.load(&link(1), &conv("fits")).unwrap().unwrap(),
        stored
    );
}
