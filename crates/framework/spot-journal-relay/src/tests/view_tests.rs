//! The names-only journal projection (WP-SEC-04.T9, `Channel::Journal`): hidden names removed,
//! masked fields named but never valued, a guard error fails closed, no value in any text.

use spot_field_guard::Channel;
use spot_journal_store::MemoryStore;
use spot_permissions::Principal;

use super::support::{FixedSigner, SOURCE, TENANT, cfg, keys};
use super::view_support::*;
use crate::{Batch, Durability, JournalView, JournalViewer, Relay, RelayError, VIEW_CHANNEL};
use spot_events_contract::envelope::Position;

const PLANTED: [&str; 4] = [SALARY, CARD, IBAN, TITLE];

fn view_as(world: &World, who: &Principal, line: &[u8]) -> JournalView {
    JournalViewer::new(&world.guard, &*world.source)
        .view_line(who, line)
        .expect("a view")
}

/// Everything a view can be turned into: its JSON, its `Debug` text and its accessors.
fn texts(view: &JournalView) -> Vec<String> {
    vec![
        view.to_json().to_string(),
        format!("{view:?}"),
        format!("{}|{}|{}", view.entity(), view.id(), view.kind()),
        view.changed().join(","),
    ]
}

fn assert_no_value(view: &JournalView) {
    for text in texts(view) {
        for secret in PLANTED {
            assert!(!text.contains(secret), "`{secret}` leaked into {text}");
        }
    }
}

#[test]
fn the_channel_is_the_names_only_one() {
    assert_eq!(VIEW_CHANNEL, Channel::Journal);
    assert!(!VIEW_CHANNEL.carries_values());
}

#[test]
fn a_view_names_the_entry_and_never_a_value() {
    let w = world();
    let view = view_as(&w, &analyst(), &state_line(DOCTYPE, 7, "update"));
    assert_eq!(view.entity(), DOCTYPE);
    assert_eq!(view.id(), "0192f1c0-0000-7000-8000-000000000001");
    assert_eq!(view.kind(), "update");
    assert_eq!(
        view.position(),
        Position {
            generation: 1,
            seq: 7
        }
    );
    assert_eq!(view.version(), Some(3));
    assert!(!view.names_withheld());
    assert_eq!(
        view.changed(),
        [
            "card_number",
            "id",
            "lines",
            "lines.iban",
            "lines.memo",
            "salary",
            "title"
        ]
    );
    assert_no_value(&view);
}

#[test]
fn a_hidden_field_is_not_named_and_a_masked_one_is_named_without_its_value() {
    let w = world();
    let line = state_line(DOCTYPE, 1, "insert");

    // No field grant: the protected names, and the protected child column, are not told.
    let clerk_view = view_as(&w, &clerk(), &line);
    assert_eq!(clerk_view.changed(), ["id", "lines", "lines.memo", "title"]);
    assert_no_value(&clerk_view);

    // read_masked sees that the fields exist, never what they hold.
    let masked_view = view_as(&w, &masked(), &line);
    for name in ["salary", "card_number", "lines.iban"] {
        assert!(masked_view.changed().iter().any(|n| n == name), "{name}");
    }
    assert_no_value(&masked_view);
}

#[test]
fn every_kind_of_change_is_named_and_an_unknown_op_is_not_echoed() {
    let w = world();
    for op in ["insert", "update", "delete"] {
        let view = view_as(&w, &analyst(), &state_line(DOCTYPE, 1, op));
        assert_eq!(view.kind(), op);
    }
    let odd = state_line(DOCTYPE, 1, "SALARY-4815162342");
    let view = view_as(&w, &analyst(), &odd);
    assert_eq!(view.kind(), "unknown");
    assert_no_value(&view);
}

#[test]
fn a_guard_error_fails_closed() {
    let w = world();
    // The DocType is unknown to the protection source: the entry is named, no field is.
    let view = view_as(&w, &analyst(), &state_line("ghost_doctype", 1, "insert"));
    assert_eq!(view.entity(), "ghost_doctype");
    assert!(view.changed().is_empty());
    assert!(view.names_withheld());
    assert_no_value(&view);
}

#[test]
fn a_line_that_is_not_an_envelope_is_refused_with_a_fixed_text() {
    let w = world();
    let viewer = JournalViewer::new(&w.guard, &*w.source);
    // A parse error would quote the offending text; the refusal must not.
    let junk = format!("{{\"salary\": \"{SALARY}\", broken").into_bytes();
    let error = viewer.view_line(&analyst(), &junk).unwrap_err();
    assert_eq!(error.code(), "journal_relay.entry_invalid");
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains(SALARY), "{text}");
    }
    let not_utf8 = viewer.view_line(&analyst(), &[0xff, 0xfe, 0x00]);
    assert!(matches!(not_utf8, Err(RelayError::EntryInvalid(_))));
}

#[tokio::test]
async fn the_relay_shows_its_open_entries_only_as_views() {
    let w = world();
    let store = MemoryStore::new();
    let mut config = cfg(Durability::Local);
    config.limits = spot_journal_segment::SealLimits {
        max_bytes: u64::MAX,
        max_age_millis: u64::MAX,
    };
    let mut relay = Relay::open(store, Box::new(FixedSigner), keys(), config, 1_000)
        .await
        .expect("open");
    let batch = Batch {
        batch_id: 1,
        entries: (1..=2)
            .map(|seq| {
                (
                    Position { generation: 1, seq },
                    state_line(DOCTYPE, seq, "update"),
                )
            })
            .collect(),
    };
    relay.append_batch(batch, 2_000).await.expect("append");
    let viewer = JournalViewer::new(&w.guard, &*w.source);
    let views = relay.open_views(&viewer, &clerk());
    assert_eq!(views.len(), 2);
    for view in &views {
        let view = view.as_ref().expect("a view");
        assert_eq!(view.changed(), ["id", "lines", "lines.memo", "title"]);
        assert_no_value(view);
    }
    assert_eq!((relay.tenant(), relay.source()), (TENANT, SOURCE));
}

#[tokio::test]
async fn a_refused_batch_never_quotes_an_entry() {
    let store = MemoryStore::new();
    let mut relay = Relay::open(
        store,
        Box::new(FixedSigner),
        keys(),
        cfg(Durability::Local),
        1_000,
    )
    .await
    .expect("open");
    let line = state_line(DOCTYPE, 5, "insert");
    // Out of order (a gap), and a line that is not UTF-8 around the secret.
    let gap = Batch {
        batch_id: 1,
        entries: vec![(
            Position {
                generation: 1,
                seq: 5,
            },
            line,
        )],
    };
    let mut bad = SALARY.as_bytes().to_vec();
    bad.push(0xff);
    let invalid = Batch {
        batch_id: 2,
        entries: vec![(
            Position {
                generation: 1,
                seq: 1,
            },
            bad,
        )],
    };
    for batch in [gap, invalid] {
        let error = relay.append_batch(batch, 2_000).await.unwrap_err();
        for text in [error.to_string(), format!("{error:?}")] {
            for secret in PLANTED {
                assert!(!text.contains(secret), "{text}");
            }
        }
    }
}
