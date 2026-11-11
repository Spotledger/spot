//! The engine-backed secret-field case of the durable Idempotency-Key store (WP-PLT-05.T10,
//! Recorded security finding, WP-PLT-05.T7 item 9, hardened by WP-PLT-05.T13): an account with sealed (`Secret`)
//! fields behind the real router, the real engine with the sealed-field vault and the real
//! `PgIdempotencyStore`.
//!
//! The property proven is: **no secret value, in any form, is in any stored answer, in any table
//! of the database, or in the refusal that is stored**, and a replay returns the same bytes. It
//! is not "a secret posted through the router leaves nothing at rest": a `Secret` field cannot be
//! written through the router at all (a value for one in a POST is answered `422`,
//! `document.validation_failed`, "the secrets service holds it"), so no stored POST carries a
//! secret. The secrets are set the way the secrets service sets them (`create_with_secrets` on
//! the engine) and the keyed requests are the ones that answer with the document: create,
//! `post`, `rename`, `cancel`, `correct`. A failure message states a boolean, a length, or a
//! table and column name, never a value.
//!
//! The scan is searched in the wire forms too (text, hex, base64 standard and URL-safe, padded
//! and not), and `the_scan_finds_a_planted_secret_in_a_table_and_column` shows it fires on a
//! planted value, so a zero-hit scan can be believed. `a_reader_never_gets_a_masked_field...`
//! uses an `Authorizer` double ([`MaskNote`](engine::MaskNote)) that hides `internal_note` from
//! the `reader` class.
#![allow(missing_docs, dead_code, clippy::unwrap_used, clippy::expect_used)]

mod common;
#[path = "common/engine.rs"]
mod engine;

use base64::Engine as _;
use engine::*;
use serde_json::json;

fn b64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

fn path_of(id: spot_app_api::prelude::RecordId) -> String {
    format!("/api/v1/resource/account/{id}")
}

/// Asserts `bytes` hold none of the secrets of the seeded account, in any form.
async fn assert_clean(rig: &Rig, id: spot_app_api::prelude::RecordId, bytes: &[u8], what: &str) {
    for plain in [PASSWORD, OTHER_PASSWORD, API_KEY] {
        assert!(!leaks(bytes, plain), "{what} holds a plaintext secret");
    }
    for column in ["password__sealed", "api_key__sealed", "api_key__bidx"] {
        let stored = rig.bytes_column(id, column).await;
        let stored = stored.unwrap_or_else(|| panic!("the engine wrote no {column}"));
        assert!(
            !leaks_bytes(bytes, &stored),
            "{what} holds the engine's {column}"
        );
    }
}

/// The allow-listed header names a stored row may carry.
fn assert_allowed_headers(headers: &str) {
    let pairs: Vec<Vec<String>> = serde_json::from_str(headers).unwrap();
    for pair in pairs {
        assert!(
            ["etag", "location", "content-type", "retry-after"].contains(&pair[0].as_str()),
            "a stored header outside the allow-list: {}",
            pair[0]
        );
    }
}

/// No secret in any table of the database: the plaintext and every wire form of it
/// (`forms_of`: hex, base64 standard and URL-safe, padded and not), in any column of any table,
/// `spot_idempotency_key` included. Reports table and column names, never a value.
async fn assert_nowhere(rig: &Rig) {
    for plain in [PASSWORD, OTHER_PASSWORD, API_KEY] {
        assert!(
            rig.hits_anywhere(plain).await.is_empty(),
            "a secret value is in a table of the database"
        );
        let wire = rig.wire_hits(plain).await;
        assert!(
            wire.is_empty(),
            "a secret value, in a wire form, is in these (table, column): {wire:?}"
        );
    }
}

/// A keyed lifecycle call by the writer.
async fn act(
    rig: &Rig,
    id: spot_app_api::prelude::RecordId,
    action: &str,
    key: &str,
    etag: &str,
    body: Option<serde_json::Value>,
) -> Reply {
    rig.call(
        "POST",
        &format!("{}/{action}", path_of(id)),
        &[bearer(TOKEN_WRITER), idem_key(key), if_match(etag)],
        body,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stored_answer_holds_no_secret_and_replays_the_same_bytes() {
    let Some(rig) = Rig::new().await else { return };
    let (id, etag) = rig.seed("masked").await;
    let sealed_before = rig.bytes_column(id, "password__sealed").await.unwrap();

    let first = act(&rig, id, "post", "k1", &etag, None).await;
    assert_eq!(first.status, 200);
    assert!(first.header("idempotency-replayed").is_none());
    // The observed 200 body carries no secret field at all (absent, not masked).
    assert!(first.json["data"].get("password").is_none());
    assert!(first.json["data"].get("api_key").is_none());
    assert_clean(&rig, id, &first.raw, "the response").await;

    let row = rig.row_of("k1").await;
    assert_eq!(row.state, "done");
    assert_eq!(row.status, Some(200));
    assert_eq!(row.body, first.raw, "the stored bytes are the sent bytes");
    assert_clean(&rig, id, &row.body, "the stored body").await;
    assert_allowed_headers(&row.headers);
    assert_nowhere(&rig).await;

    // Replay: the same bytes, the engine ran once.
    let second = act(&rig, id, "post", "k1", &etag, None).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.header("idempotency-replayed"), Some("true"));
    assert_eq!(
        second.raw, first.raw,
        "the replay is byte for byte the first answer"
    );
    assert_eq!(rig.rows().await, 1);
    let loaded = rig
        .call("GET", &path_of(id), &[bearer(TOKEN_WRITER)], None)
        .await;
    assert_eq!(loaded.json["data"]["version"], 2, "the post ran once");
    assert_eq!(loaded.json["data"]["_state"], "posted");
    assert_eq!(
        rig.bytes_column(id, "password__sealed").await.unwrap(),
        sealed_before,
        "one sealed value, untouched"
    );
    assert_eq!(rig.accounts().await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_node_replays_the_same_masked_bytes() {
    let Some(mut rig) = Rig::new().await else {
        return;
    };
    let (id, etag) = rig.seed("restart").await;
    let first = act(&rig, id, "post", "k1", &etag, None).await;
    assert_eq!(first.status, 200);
    rig.restart("idem_secret_b").await;

    let replay = act(&rig, id, "post", "k1", &etag, None).await;
    assert_eq!(replay.status, 200);
    assert_eq!(replay.header("idempotency-replayed"), Some("true"));
    assert_eq!(replay.raw, first.raw);
    let row = rig.row_of("k1").await;
    assert_eq!(row.state, "done");
    assert_clean(&rig, id, &row.body, "the stored body after the restart").await;
    assert_nowhere(&rig).await;
    let loaded = rig
        .call("GET", &path_of(id), &[bearer(TOKEN_WRITER)], None)
        .await;
    assert_eq!(
        loaded.json["data"]["version"], 2,
        "the post did not run again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_secret_in_a_post_is_refused_and_the_refusal_is_stored_without_it() {
    let Some(rig) = Rig::new().await else { return };
    let (id, _etag) = rig.seed("refused").await;
    let body = json!({"label": "x", "password": b64(PASSWORD), "api_key": b64(API_KEY)});
    let first = rig.create("k1", body.clone()).await;
    assert_eq!(first.status, 422);
    assert_eq!(first.error_code(), "document.validation_failed");
    assert_eq!(rig.accounts().await, 1, "nothing was created");
    for what in [PASSWORD, API_KEY] {
        assert!(!leaks(&first.raw, what), "the refusal echoes a secret");
        assert!(
            !leaks(&first.raw, &b64(what)),
            "the refusal echoes a secret"
        );
    }
    let row = rig.row_of("k1").await;
    assert_eq!(row.state, "done");
    assert_eq!(row.status, Some(422));
    assert_eq!(row.body, first.raw);
    assert_clean(&rig, id, &row.body, "the stored refusal").await;
    assert_nowhere(&rig).await;

    // The same key and body replays the refusal byte for byte.
    let again = rig.create("k1", body).await;
    assert_eq!(again.header("idempotency-replayed"), Some("true"));
    assert_eq!(again.raw, first.raw);

    // The same key with another secret value is a conflict; neither value is in its answer or
    // the row.
    let other = json!({"label": "x", "password": b64(OTHER_PASSWORD), "api_key": b64(API_KEY)});
    let conflict = rig.create("k1", other).await;
    assert_eq!(conflict.status, 409);
    assert_eq!(conflict.error_code(), "api.idempotency_conflict");
    for what in [PASSWORD, OTHER_PASSWORD] {
        assert!(!leaks(&conflict.raw, what));
        assert!(!leaks(&conflict.raw, &b64(what)));
    }
    let row = rig.row_of("k1").await;
    for what in [PASSWORD, OTHER_PASSWORD, API_KEY] {
        assert!(!leaks(&row.body, &b64(what)), "the row holds a secret");
    }
    assert_clean(&rig, id, &row.body, "the row after the conflict").await;
    assert_eq!(rig.rows().await, 1);
    assert_nowhere(&rig).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lookup_digest_and_ciphertext_are_not_stored() {
    let Some(rig) = Rig::new().await else { return };
    let (id, etag) = rig.seed("digest").await;
    let digest = rig.bytes_column(id, "api_key__bidx").await.unwrap();
    let sealed = rig.bytes_column(id, "api_key__sealed").await.unwrap();
    assert!(
        digest.len() >= 16 && sealed.len() >= 16,
        "the engine wrote both"
    );
    let renamed = act(
        &rig,
        id,
        "rename",
        "k1",
        &etag,
        Some(json!({"name": "digest renamed"})),
    )
    .await;
    assert_eq!(renamed.status, 200, "the rename ran");
    let row = rig.row_of("k1").await;
    assert!(
        !leaks_bytes(&row.body, &digest),
        "the body holds the blind-index digest"
    );
    assert!(
        !leaks_bytes(&row.body, &sealed),
        "the body holds the ciphertext"
    );
    assert!(!leaks_bytes(row.headers.as_bytes(), &digest));
    assert_clean(&rig, id, &row.body, "the stored body").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_document_route_stores_no_secret() {
    let Some(rig) = Rig::new().await else { return };
    let (id, etag) = rig.seed("routes").await;

    // The create route: a document without a secret, then the secrets set on it behind the
    // router. The created document's answer is stored; it has none.
    let created = rig.create("k-create", json!({"label": "created"})).await;
    assert_eq!(created.status, 201);
    assert_eq!(rig.row_of("k-create").await.body, created.raw);

    let renamed = act(
        &rig,
        id,
        "rename",
        "k-rename",
        &etag,
        Some(json!({"name": "renamed"})),
    )
    .await;
    assert_eq!(renamed.status, 200, "rename");
    let etag = renamed.etag();
    let posted = act(&rig, id, "post", "k-post", &etag, None).await;
    assert_eq!(posted.status, 200, "post");
    let etag = posted.etag();
    let corrected = act(&rig, id, "correct", "k-correct", &etag, None).await;
    assert_eq!(corrected.status, 201, "correct");
    // Cancel on a second account, posted first.
    let (other, etag) = rig.seed("cancel").await;
    let posted2 = act(&rig, other, "post", "k-post2", &etag, None).await;
    assert_eq!(posted2.status, 200);
    let cancelled = act(&rig, other, "cancel", "k-cancel", &posted2.etag(), None).await;
    assert_eq!(cancelled.status, 200, "cancel");

    for (key, reply) in [
        ("k-rename", &renamed),
        ("k-post", &posted),
        ("k-correct", &corrected),
    ] {
        let row = rig.row_of(key).await;
        assert_eq!(row.state, "done");
        assert_eq!(
            row.body, reply.raw,
            "{key}: stored bytes are the sent bytes"
        );
        assert_clean(&rig, id, &row.body, key).await;
        assert_allowed_headers(&row.headers);
    }
    let row = rig.row_of("k-cancel").await;
    assert_eq!(row.body, cancelled.raw);
    assert_clean(&rig, other, &row.body, "k-cancel").await;
    assert_nowhere(&rig).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reveal_is_not_behind_the_store() {
    let Some(rig) = Rig::new().await else { return };
    let (id, etag) = rig.seed("reveal").await;
    act(&rig, id, "post", "k1", &etag, None).await;
    assert_eq!(rig.rows().await, 1);

    let reveal = format!("{}/reveal", path_of(id));
    let body = json!({"field": "password", "reason": "support case 17"});
    // The caller without the right is refused.
    let denied = rig
        .call(
            "POST",
            &reveal,
            &[bearer(TOKEN_WRITER), idem_key("r1")],
            Some(body.clone()),
        )
        .await;
    assert_eq!(denied.status, 403);
    assert!(!leaks(&denied.raw, PASSWORD));
    // The holder gets the value, with an Idempotency-Key that is ignored.
    let shown = rig
        .call(
            "POST",
            &reveal,
            &[bearer(TOKEN_REVEALER), idem_key("r2")],
            Some(body),
        )
        .await;
    assert_eq!(shown.status, 200);
    assert_eq!(
        shown.json["data"]["value"], PASSWORD,
        "the holder reads the value"
    );
    assert_eq!(rig.rows().await, 1, "no row was made for the reveal");
    let row = rig.row_of("k1").await;
    assert!(!leaks(&row.body, PASSWORD));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_store_down_the_create_is_refused_and_nothing_ran() {
    let Some(rig) = Rig::new().await else { return };
    rig.seed("down").await;
    assert_eq!(rig.accounts().await, 1);
    rig.node.pool.close();
    let refused = rig.create("k1", json!({"label": "never"})).await;
    assert_eq!(refused.status, 503);
    assert_eq!(refused.error_code(), "server.unavailable");
    assert!(refused.header("retry-after").is_some());
    assert_eq!(rig.accounts().await, 1, "the engine did not run");
}

/// The detectors find what they look for: a suite that cannot fail proves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_detectors_see_a_leak() {
    let Some(rig) = Rig::new().await else { return };
    let (id, _) = rig.seed("detect").await;
    let sealed = rig.bytes_column(id, "password__sealed").await.unwrap();
    let embedded = format!("x{}y", b64(PASSWORD));
    assert!(leaks(embedded.as_bytes(), PASSWORD));
    assert!(
        leaks(&PASSWORD.as_bytes()[5..20], PASSWORD),
        "a run of 8 or more bytes"
    );
    assert!(leaks_bytes(&[b"..".as_slice(), &sealed].concat(), &sealed));
    assert!(!leaks(b"nothing here", PASSWORD));
    assert!(
        !rig.hits_anywhere("detect").await.is_empty(),
        "the table search finds a value that is stored"
    );
}

/// Asserts the scan for `API_KEY`, in every form, reports a hit in exactly `(table, column)`.
async fn assert_only_hit(rig: &Rig, table_has: &str, column: &str, what: &str) {
    let hits = rig.wire_hits(API_KEY).await;
    assert_eq!(hits.len(), 1, "{what}: exactly one hit, found {hits:?}");
    assert!(
        hits[0].0.contains(table_has) && hits[0].1 == column,
        "{what}: the hit is in {:?}, expected a table with `{table_has}` and column `{column}`",
        hits[0]
    );
}

/// The positive control that plants a secret (WP-PLT-05.T13 item 3): a known secret value, in
/// plaintext and in base64, is planted in a column of `account` (written through the engine)
/// and in the `body` of `spot_idempotency_key`; the scan must report a hit in exactly that table
/// and column, and be clean once the plant is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_scan_finds_a_planted_secret_in_a_table_and_column() {
    let Some(rig) = Rig::new().await else { return };
    assert_nowhere(&rig).await;

    // A row of `account` written through storage (the engine's own write), its `label` column
    // then set to the secret behind the engine's back. (The engine's outbox keeps every value a
    // write carried, so planting through a second engine write could not be taken back.)
    let (_id, _etag) = rig.seed("plant host").await;
    assert_nowhere(&rig).await;
    for planted in [API_KEY.to_owned(), b64(API_KEY)] {
        let client = rig.db.raw_client().await;
        let set = client
            .execute("UPDATE account SET label = $1::text", &[&planted])
            .await
            .unwrap();
        assert_eq!(set, 1, "one account row planted");
        assert_only_hit(&rig, "account", "label", "an account label").await;
        client
            .execute("UPDATE account SET label = 'plant host'", &[])
            .await
            .unwrap();
        let left = rig.wire_hits(API_KEY).await;
        assert!(left.is_empty(), "the plant is gone, yet: {left:?}");
    }

    // A plant in the stored answer of the idempotency table.
    let (id, etag) = rig.seed("body plant").await;
    let first = act(&rig, id, "post", "k1", &etag, None).await;
    assert_eq!(first.status, 200);
    assert_nowhere(&rig).await;
    for planted in [API_KEY.to_owned(), b64(API_KEY)] {
        rig.db
            .raw_client()
            .await
            .execute(
                "UPDATE spot_idempotency_key SET body = convert_to($1::text, 'UTF8') \
                 WHERE key = 'k1'",
                &[&planted],
            )
            .await
            .unwrap();
        assert_only_hit(&rig, "spot_idempotency_key", "body", "a stored body").await;
        // And the per-test detector sees it too.
        assert!(leaks(&rig.row_of("k1").await.body, API_KEY));
        rig.db
            .raw_client()
            .await
            .execute(
                "UPDATE spot_idempotency_key SET body = $1 WHERE key = 'k1'",
                &[&first.raw],
            )
            .await
            .unwrap();
        let left = rig.wire_hits(API_KEY).await;
        assert!(left.is_empty(), "the plant is gone, yet: {left:?}");
    }
    assert_nowhere(&rig).await;
}

/// A field masked by permission is absent from every stored answer (WP-PLT-05.T13 item 4):
/// the `admin` creates an account with `internal_note`; the `reader` (for whom the
/// `MaskNote` authorizer drops the field) acts on it with keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_never_gets_a_masked_field_from_the_replay_cache() {
    let Some(rig) = Rig::new().await else { return };
    let admin = |key: &str| vec![bearer(TOKEN_WRITER), idem_key(key)];
    let reader = |key: &str| vec![bearer(TOKEN_READER), idem_key(key)];

    let body = json!({"label": "masked", "internal_note": NOTE});
    let made = rig
        .call(
            "POST",
            "/api/v1/resource/account",
            &admin("adm-create"),
            Some(body.clone()),
        )
        .await;
    assert_eq!(made.status, 201);
    assert_eq!(
        made.json["data"]["internal_note"], NOTE,
        "the admin sees the field"
    );
    let id = made.id();
    // The control: the admin's stored answer does hold it, so the check below can fire.
    assert!(contains(
        &rig.row_of("adm-create").await.body,
        NOTE.as_bytes()
    ));

    // A GET is not stored, with or without a key, and the reader's read is masked.
    let rows = rig.rows().await;
    let read = rig.call("GET", &path_of(id), &reader("rd-get"), None).await;
    assert_eq!(read.status, 200);
    assert_eq!(rig.rows().await, rows, "a GET stores nothing");
    assert!(read.json["data"].get("internal_note").is_none());
    assert!(!contains(&read.raw, NOTE.as_bytes()));
    let admin_read = rig
        .call("GET", &path_of(id), &[bearer(TOKEN_WRITER)], None)
        .await;
    assert_eq!(admin_read.json["data"]["internal_note"], NOTE);

    // The reader's keyed lifecycle calls are stored masked.
    let etag = read.etag();
    let mut rename_headers = reader("rd-rename");
    rename_headers.push(if_match(&etag));
    let renamed = rig
        .call(
            "POST",
            &format!("{}/rename", path_of(id)),
            &rename_headers,
            Some(json!({"name": "by reader"})),
        )
        .await;
    assert_eq!(renamed.status, 200, "the reader renames");
    let mut post_headers = reader("rd-post");
    post_headers.push(if_match(&renamed.etag()));
    let posted = rig
        .call(
            "POST",
            &format!("{}/post", path_of(id)),
            &post_headers,
            None,
        )
        .await;
    assert_eq!(posted.status, 200, "the reader posts");
    for (key, reply) in [("rd-rename", &renamed), ("rd-post", &posted)] {
        assert!(reply.json["data"].get("internal_note").is_none(), "{key}");
        let row = rig.row_of(key).await;
        assert_eq!(
            row.body, reply.raw,
            "{key}: stored bytes are the sent bytes"
        );
        assert_eq!(row.state, "done");
        for form in forms_of(NOTE) {
            assert!(
                !contains(&row.body, &form),
                "{key}: the stored body holds the note"
            );
        }
        assert!(
            !contains(&row.body, b"internal_note"),
            "{key}: the stored body names the masked field"
        );
        assert!(!contains(row.headers.as_bytes(), NOTE.as_bytes()));
    }

    // The converse: the same key and request from the same reader replays the same masked bytes.
    let mut again_headers = reader("rd-rename");
    again_headers.push(if_match(&etag));
    let again = rig
        .call(
            "POST",
            &format!("{}/rename", path_of(id)),
            &again_headers,
            Some(json!({"name": "by reader"})),
        )
        .await;
    assert_eq!(again.header("idempotency-replayed"), Some("true"));
    assert_eq!(
        again.raw, renamed.raw,
        "the reader's replay is byte for byte its first answer"
    );

    // A key is scoped by the credential (ADR-055): the reader replaying the admin's key and body
    // is a fresh execution under its own scope, never the admin's stored bytes, and neither
    // answer is a 409.
    let stolen = rig
        .call(
            "POST",
            "/api/v1/resource/account",
            &reader("adm-create"),
            Some(body),
        )
        .await;
    assert_eq!(
        stolen.status, 201,
        "a fresh execution, not a replay and not a 409"
    );
    assert!(stolen.header("idempotency-replayed").is_none());
    assert_ne!(stolen.raw, made.raw, "never the admin's stored bytes");
    assert!(stolen.json["data"].get("internal_note").is_none());
    assert!(!contains(&stolen.raw, NOTE.as_bytes()));
    let bodies = rig
        .db
        .raw_client()
        .await
        .query(
            "SELECT body FROM spot_idempotency_key WHERE key = 'adm-create'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(bodies.len(), 2, "one row per credential for the one key");
    let holding = bodies
        .iter()
        .filter(|r| {
            contains(
                &r.get::<_, Option<Vec<u8>>>(0).unwrap_or_default(),
                NOTE.as_bytes(),
            )
        })
        .count();
    assert_eq!(holding, 1, "only the admin's own row holds the note");
}
