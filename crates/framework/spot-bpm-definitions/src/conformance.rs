//! The conformance suite every [`DefinitionStore`] runs. It asserts (it panics on a failure), so
//! a store's test calls [`run`] with an empty store and a fixed instant.
//!
//! It covers publishing and numbering, the optimistic check, immutability, listing, retiring, the
//! name, size and text refusals, and that a refused write leaves nothing behind.

// A test-support module: a failed expectation is the failure it reports.
#![allow(clippy::expect_used)]

use spot_app_api::prelude::OffsetDateTime;

use crate::error::{DefinitionError, codes};
use crate::store::DefinitionStore;
use crate::types::{MAX_TEXT_BYTES, VersionState, content_hash};

/// A valid process text named `name`, with `marker` in the node's doctype so different markers
/// give different content hashes.
#[must_use]
pub fn sample_text(name: &str, marker: &str) -> String {
    format!(
        r#"
name = "{name}"
app = "defs_test"
version = 1
status = "draft"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "work"
kind = "document"
action = "create"
doctype = "{marker}"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "work"

[[edges]]
from = "work"
to = "done"
"#
    )
}

fn code_of<T: std::fmt::Debug>(result: Result<T, DefinitionError>) -> &'static str {
    match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(error) => error.code(),
    }
}

/// Runs the whole suite against an empty store. `now` is the instant used for every write.
///
/// # Panics
/// On the first behaviour that does not hold.
#[allow(clippy::too_many_lines)]
pub async fn run<S: DefinitionStore + ?Sized>(store: &S, now: OffsetDateTime) {
    let later = now + std::time::Duration::from_secs(60);
    assert!(store.names().await.expect("names").is_empty());
    assert!(store.get("order", 1).await.expect("get").is_none());
    assert!(
        store
            .latest_published("order")
            .await
            .expect("latest")
            .is_none()
    );

    // Numbering starts at 1 and follows the caller's expectation.
    let text1 = sample_text("order", "Sales Order");
    let v1 = store
        .publish("order", &text1, None, "alice", Some("first"), now)
        .await
        .expect("publish 1");
    assert_eq!(v1.version, 1);
    assert_eq!(v1.state, VersionState::Published);
    assert_eq!(v1.text, text1);
    assert_eq!(v1.published_by, "alice");
    assert_eq!(v1.published_at, now);
    assert_eq!(v1.note.as_deref(), Some("first"));
    assert_eq!(v1.retired_at, None);
    assert_eq!(v1.retired_by, None);
    assert_eq!(v1.content_hash, content_hash(&text1).expect("hash"));

    // The text's own version and status are ignored; the number is the store's.
    let text2 = sample_text("order", "Purchase Order");
    let v2 = store
        .publish("order", &text2, Some(1), "bob", None, later)
        .await
        .expect("publish 2");
    assert_eq!(v2.version, 2);
    assert_ne!(v2.content_hash, v1.content_hash);
    assert_eq!(v2.note, None);

    // A stale or wrong expectation is a conflict that names what the store holds.
    for expected in [None, Some(1), Some(7)] {
        let refused = store
            .publish("order", &text2, expected, "bob", None, later)
            .await;
        match refused {
            Err(DefinitionError::VersionConflict {
                expected: e, found, ..
            }) => {
                assert_eq!(e, expected);
                assert_eq!(found, Some(2));
            }
            other => panic!("expected a version conflict, got {other:?}"),
        }
    }
    assert_eq!(store.list("order", None, 10).await.expect("list").len(), 2);

    // Republishing the same text is a new version only when asked, with the same hash.
    let v3 = store
        .publish("order", &text1, Some(2), "alice", Some("again"), later)
        .await
        .expect("publish 3");
    assert_eq!(v3.version, 3);
    assert_eq!(v3.content_hash, v1.content_hash);

    // Reads.
    assert_eq!(store.get("order", 2).await.expect("get"), Some(v2.clone()));
    assert!(store.get("order", 0).await.expect("get 0").is_none());
    assert!(store.get("order", 4).await.expect("get 4").is_none());
    assert_eq!(
        store.latest_published("order").await.expect("latest"),
        Some(v3.clone())
    );
    store
        .publish(
            "invoice",
            &sample_text("invoice", "Sales Invoice"),
            None,
            "alice",
            None,
            now,
        )
        .await
        .expect("publish other name");
    assert_eq!(
        store.names().await.expect("names"),
        vec!["invoice".to_owned(), "order".to_owned()]
    );

    // Paging is ascending and bounded.
    let first = store.list("order", None, 2).await.expect("page 1");
    assert_eq!(first.iter().map(|v| v.version).collect::<Vec<_>>(), [1, 2]);
    let next = store.list("order", Some(2), 2).await.expect("page 2");
    assert_eq!(next.iter().map(|v| v.version).collect::<Vec<_>>(), [3]);
    assert!(
        store
            .list("order", Some(3), 2)
            .await
            .expect("end")
            .is_empty()
    );
    assert!(store.list("order", None, 0).await.expect("zero").is_empty());
    assert!(
        store
            .list("nothing", None, 5)
            .await
            .expect("none")
            .is_empty()
    );

    // Retiring changes the state only; text and hash are untouched; the first retire is kept.
    let retired = store
        .retire("order", 3, "carol", later)
        .await
        .expect("retire");
    assert_eq!(retired.state, VersionState::Retired);
    assert_eq!(retired.retired_at, Some(later));
    assert_eq!(retired.retired_by.as_deref(), Some("carol"));
    assert_eq!(retired.text, v3.text);
    assert_eq!(retired.content_hash, v3.content_hash);
    assert_eq!(retired.published_by, v3.published_by);
    let again = store
        .retire(
            "order",
            3,
            "dave",
            later + std::time::Duration::from_secs(5),
        )
        .await
        .expect("retire twice");
    assert_eq!(again, retired);
    assert_eq!(
        store.latest_published("order").await.expect("latest"),
        Some(v2.clone())
    );
    assert_eq!(store.get("order", 3).await.expect("get"), Some(retired));
    assert_eq!(
        code_of(store.retire("order", 9, "carol", later).await),
        codes::NOT_FOUND
    );
    assert_eq!(
        code_of(store.retire("order", 0, "carol", later).await),
        codes::NOT_FOUND
    );
    // Numbers continue after a retire; none is reused.
    let v4 = store
        .publish("order", &text2, Some(3), "alice", None, later)
        .await
        .expect("publish 4");
    assert_eq!(v4.version, 4);

    // Refusals, each leaving the store as it was.
    let before = store.list("order", None, 100).await.expect("before");
    let names_before = store.names().await.expect("names");
    let too_big = format!(
        "{}\n{}",
        sample_text("order", "X"),
        "#".repeat(MAX_TEXT_BYTES)
    );
    assert_eq!(
        code_of(
            store
                .publish("order", &too_big, Some(4), "a", None, now)
                .await
        ),
        codes::TOO_LARGE
    );
    let at_cap = format!(
        "{}\n{}",
        sample_text("order", "X"),
        "#".repeat(MAX_TEXT_BYTES - sample_text("order", "X").len() - 1)
    );
    assert_eq!(at_cap.len(), MAX_TEXT_BYTES);
    let capped = store
        .publish("order", &at_cap, Some(4), "a", None, now)
        .await
        .expect("a text exactly at the cap is accepted");
    assert_eq!(capped.version, 5);
    let before = {
        let mut b = before;
        b.push(capped);
        b
    };
    for bad in [
        "",
        "Order",
        "1order",
        "or-der",
        "../order",
        "order/x",
        "or der",
        "o'rder",
        "a\0b",
        &"a".repeat(65),
    ] {
        assert_eq!(
            code_of(
                store
                    .publish(bad, &sample_text("order", "X"), None, "a", None, now)
                    .await
            ),
            codes::NAME_INVALID,
            "publish accepted a bad name"
        );
        assert_eq!(code_of(store.get(bad, 1).await), codes::NAME_INVALID);
        assert_eq!(
            code_of(store.latest_published(bad).await),
            codes::NAME_INVALID
        );
        assert_eq!(code_of(store.list(bad, None, 1).await), codes::NAME_INVALID);
        assert_eq!(
            code_of(store.retire(bad, 1, "a", now).await),
            codes::NAME_INVALID
        );
        assert_eq!(code_of(store.running_pins(bad).await), codes::NAME_INVALID);
    }
    for (text, publisher) in [
        ("not a process", "a"),
        ("", "a"),
        (&sample_text("other", "X"), "a"),
        (&sample_text("order", "X"), ""),
    ] {
        assert_eq!(
            code_of(
                store
                    .publish("order", text, Some(5), publisher, None, now)
                    .await
            ),
            codes::INPUT_INVALID
        );
    }
    let long_note = "n".repeat(2000);
    assert_eq!(
        code_of(
            store
                .publish(
                    "order",
                    &sample_text("order", "X"),
                    Some(5),
                    "a",
                    Some(&long_note),
                    now
                )
                .await
        ),
        codes::TOO_LARGE
    );
    assert_eq!(store.list("order", None, 100).await.expect("after"), before);
    assert_eq!(store.names().await.expect("names"), names_before);
    // A refused first publish creates no name.
    assert_eq!(
        code_of(
            store
                .publish("fresh", "not a process", None, "a", None, now)
                .await
        ),
        codes::INPUT_INVALID
    );
    assert_eq!(
        code_of(
            store
                .publish("fresh", &sample_text("fresh", "X"), Some(1), "a", None, now)
                .await
        ),
        codes::VERSION_CONFLICT
    );
    assert_eq!(store.names().await.expect("names"), names_before);
    assert!(
        store
            .latest_published("fresh")
            .await
            .expect("fresh")
            .is_none()
    );
}
