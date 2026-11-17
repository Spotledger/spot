//! Channel `Notification`: `spot-realtime-bridge` through `FeedPump::pump_once`, the real public
//! entry point that turns committed document events and `sys_user_notice` outbox events into room
//! messages, and `recipient_names` for a payload built for one recipient. The channel carries
//! field **names** only, never a value, and never the name of a field the audience may not know.
//! Every captured event is serialised and handed to the harness.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

// The bridge's own fake feed, position store and bus helpers, reused as they are.
#[path = "../../../../crates/framework/spot-realtime-bridge/tests/support/mod.rs"]
mod support;

use std::sync::Arc;

use common::*;
use spot_field_guard::leakcheck::{Output, assert_no_leak, find_leaks};
use spot_realtime::{AllowAll, Room, Subscription};
use spot_realtime_bridge::{FeedPump, TOPIC_NOTICE, names::recipient_names};
use spot_storage_contract::ChangeOp;
use support::{
    FakeFeed, FakeStore, FakeTelemetry, bus_with, changed_fields_payload, doc_id_text,
    payload_with_data, pump, rx_now, site, user_text, watcher,
};

const FIELDS: [&str; 7] = [
    "number",
    "notes",
    "card_number",
    "holder",
    "profile_json",
    "salary",
    "annual_cost",
];
const PROTECTED: [&str; 5] = [
    "card_number",
    "holder",
    "profile_json",
    "salary",
    "annual_cost",
];

struct Stand {
    pump: FeedPump<FakeFeed, FakeTelemetry>,
    w_doc: Subscription,
    w_user: Subscription,
}

/// Feeds a document change, a posted event naming every field, a notice whose text
/// interpolates the planted `salary` value and a names-only notice; `guarded` installs the
/// field guard on the pump.
async fn stand(core: &Core, guarded: bool) -> Stand {
    let corpus = corpus();
    let secret = corpus.get("salary").unwrap().value().to_owned();
    let bus = bus_with(64, Arc::new(AllowAll));
    let site = site("leak.example");
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());
    let mut pump = pump(&site, &feed, &bus, &store, &telemetry, 100, false);
    if guarded {
        pump = pump.with_notification_guard(core.protection.clone());
    }
    let user = user_text(9);
    feed.push_change(1, DOCTYPE, 42, ChangeOp::Insert, 3);
    feed.push_record(
        1,
        "doc_posted",
        DOCTYPE,
        42,
        Some(4),
        &changed_fields_payload(&FIELDS),
    );
    let interpolated = serde_json::json!({
        "doctype": DOCTYPE,
        "text": format!("Salary is {secret}"),
    });
    feed.push_record(
        2,
        TOPIC_NOTICE,
        DOCTYPE,
        42,
        None,
        &payload_with_data(&format!(r#"{{"user":"{user}","payload":{interpolated}}}"#)),
    );
    let names_only = serde_json::json!({"doctype": DOCTYPE, "fields": ["number"]});
    feed.push_record(
        3,
        TOPIC_NOTICE,
        DOCTYPE,
        42,
        None,
        &payload_with_data(&format!(r#"{{"user":"{user}","payload":{names_only}}}"#)),
    );
    let mut w_doc = watcher(&bus, &site, "w-doc");
    w_doc
        .join(&Room::document(DOCTYPE, &doc_id_text(42)).unwrap(), None)
        .await
        .unwrap();
    w_doc
        .join(&Room::doctype(DOCTYPE).unwrap(), None)
        .await
        .unwrap();
    let mut w_user = watcher(&bus, &site, "w-user");
    w_user
        .join(&Room::user(&user).unwrap(), None)
        .await
        .unwrap();
    Stand {
        pump,
        w_doc,
        w_user,
    }
}

async fn captured(sub: &mut Subscription) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(result) = rx_now(sub).await {
        let event = result.unwrap();
        out.push(format!(
            "{} {} {}",
            event.room.as_str(),
            event.kind,
            event.payload
        ));
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_never_leaks() {
    let core = core();
    let corpus = corpus();
    let mut s = stand(&core, true).await;
    let outcome = s.pump.pump_once().await.unwrap();
    // The notice that interpolated a protected value is refused, the names-only one is sent.
    assert_eq!(outcome.mapping.outbox_notices_rejected, 1);
    let mut all = captured(&mut s.w_doc).await;
    all.extend(captured(&mut s.w_user).await);
    let text = all.join("\n");
    // Positive control: plain names and the names-only notice are there.
    assert!(
        text.contains("\"number\""),
        "plain names should be sent: {text}"
    );
    assert!(
        text.contains("user_notice"),
        "the names-only notice is sent: {text}"
    );
    // No value, and no protected field name, in anything that left.
    for name in PROTECTED {
        assert!(
            !text.contains(name),
            "the name `{name}` was broadcast: {text}"
        );
    }
    assert_no_leak(&corpus, &[Output::Text(text)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_guard_the_interpolated_notice_would_leak() {
    // The negative control: the same feed through a pump with no guard sends the value.
    let core = core();
    let corpus = corpus();
    let mut s = stand(&core, false).await;
    s.pump.pump_once().await.unwrap();
    let mut all = captured(&mut s.w_doc).await;
    all.extend(captured(&mut s.w_user).await);
    let found = find_leaks(&corpus.only(&["salary"]), &[Output::Text(all.join("\n"))]);
    assert!(
        !found.is_empty(),
        "the ungarded pump sends the planted value"
    );
}

#[test]
fn a_payload_for_one_recipient_names_only_what_the_recipient_may_see() {
    let core = core();
    let names: Vec<String> = FIELDS.iter().map(|f| (*f).to_owned()).collect();
    for who in PRINCIPALS {
        let told = recipient_names(&core.guard, &principal(who), DOCTYPE, &doc_facts(), &names);
        match who {
            "nobody" => assert!(told.is_empty(), "{who}: {told:?}"),
            "analyst" | "admin" => assert_eq!(told, names, "{who}"),
            "no_cards" => {
                for hidden in ["card_number", "holder", "profile_json"] {
                    assert!(!told.iter().any(|n| n == hidden), "{who} told `{hidden}`");
                }
                assert!(told.iter().any(|n| n == "salary"), "{who}");
            }
            _ => {}
        }
        // Whatever the principal, a name it is told is never a value of the corpus.
        assert_no_leak(&corpus(), &[Output::Text(told.join(","))]);
    }
}
