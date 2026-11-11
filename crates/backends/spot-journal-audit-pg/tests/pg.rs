//! The PostgreSQL sides against a real server, gated on `SPOT_TEST_PG_URL` like the sibling
//! crates (`SPOT_REQUIRE_PG=1` makes a missing URL a failure).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::{
    Fixture, OTHER_TENANT, TENANT, entry_body, fixture, image, invoice_body, party_body, put_entry,
    put_invoice, put_party, uuid,
};
use serde_json::Map;
use serde_json::Value as Json;
use spot_journal_audit::{Amount, DerivedEntities, Position, compare, compare_fingerprints};
use spot_journal_audit_pg::{AmountForm, FingerprintQuery, PgLiveSide, PgScratchSide, body_digest};
use spot_journal_audit_run::{LiveSide, ScratchSide};
use spot_journal_rebuild::ApplySink;

/// The page size of most tests: three rows, so a table of ten is four pages.
const PAGE: i64 = 3;

fn live(fx: &Fixture) -> PgLiveSide {
    PgLiveSide::open_with(
        fx.bridge.clone(),
        TENANT,
        common::catalog(),
        PAGE,
        Duration::from_secs(60),
    )
    .expect("the live side")
}

fn trial_balance() -> Vec<FingerprintQuery> {
    vec![FingerprintQuery {
        name: "trial_balance".to_owned(),
        entity: "gl_entry".to_owned(),
        keys: vec!["account".to_owned()],
        amount: "amount".to_owned(),
        form: AmountForm::Decimal,
    }]
}

/// Applies the images of the records `records` describes to `scratch`.
fn apply(scratch: &mut PgScratchSide, records: &[(String, u64, u64, Map<String, Json>)]) {
    for (entity, n, version, body) in records {
        wait(scratch.apply_image(image(entity, *n, *version, body.clone())))
            .expect("the image is stored");
    }
}

/// Drives a sink future to its answer: the sides' futures never pend, so one poll loop without a
/// runtime is enough.
fn wait(
    mut future: spot_journal_rebuild::SinkFuture<'_>,
) -> Result<(), spot_journal_rebuild::SinkError> {
    loop {
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        if let std::task::Poll::Ready(answer) = future.as_mut().poll(&mut context) {
            return answer;
        }
        std::thread::yield_now();
    }
}

#[test]
fn the_live_digest_equals_the_in_memory_digest_for_the_same_image() {
    let Some(fx) = fixture() else {
        return;
    };
    let client = fx.client();
    put_party(&fx, &client, 1, "Acme");
    put_invoice(&fx, &client, 1, 1);
    fx.set_head(&client, 1, 1);

    let side = live(&fx);
    let summaries: Vec<_> = side.summaries(side.head()).collect();
    let invoice = summaries
        .iter()
        .find(|summary| summary.entity == "invoice")
        .expect("the invoice is read");
    assert_eq!(invoice.id, uuid(1));
    assert_eq!(invoice.version, 1);
    // The digest of the body the in-memory side would compute for the same image.
    let want = body_digest(&invoice_body(1, 1)).expect("the body digests");
    assert_eq!(invoice.content_digest, want);
    assert_eq!(side.failure(), None);
}

#[test]
fn a_live_write_after_open_does_not_count() {
    let Some(fx) = fixture() else {
        return;
    };
    let client = fx.client();
    put_party(&fx, &client, 1, "Acme");
    put_invoice(&fx, &client, 1, 1);
    fx.set_head(&client, 1, 1);

    let side = live(&fx);
    assert_eq!(side.head(), Position::new(1, 1));
    // A write that lands after the snapshot was taken: another record and a moved head.
    put_invoice(&fx, &client, 2, 1);
    fx.set_head(&client, 1, 2);

    let summaries: Vec<_> = side.summaries(Position::new(1, 1)).collect();
    assert_eq!(
        summaries.iter().filter(|s| s.entity == "invoice").count(),
        1,
        "the write after `open` is not in the snapshot"
    );
    assert!(summaries.iter().any(|s| s.id == uuid(1)));
    assert_eq!(side.head(), Position::new(1, 1));
}

#[test]
fn a_scratch_and_a_live_side_compare_equal() {
    let Some(fx) = fixture() else {
        return;
    };
    let client = fx.client();
    put_party(&fx, &client, 1, "Acme");
    put_invoice(&fx, &client, 1, 1);
    put_invoice(&fx, &client, 2, 1);
    put_entry(&fx, &client, 1, "cash", "100.00");
    put_entry(&fx, &client, 2, "bank", "250.25");
    fx.set_head(&client, 1, 4);

    let live = live(&fx).with_fingerprints(trial_balance()).expect("live");
    let mut scratch =
        PgScratchSide::create(fx.bridge.clone(), TENANT, common::catalog(), "run1", 1)
            .expect("the scratch side")
            .with_fingerprints(trial_balance())
            .expect("scratch");
    apply(
        &mut scratch,
        &[
            ("party".to_owned(), 1, 1, party_body("Acme")),
            ("invoice".to_owned(), 1, 1, invoice_body(1, 1)),
            ("invoice".to_owned(), 2, 1, invoice_body(2, 1)),
            ("gl_entry".to_owned(), 1, 1, entry_body("cash", "100.00")),
            ("gl_entry".to_owned(), 2, 1, entry_body("bank", "250.25")),
        ],
    );

    let result = compare(
        live.summaries(live.head()),
        scratch.summaries(),
        &DerivedEntities::none(),
    )
    .expect("both streams are sorted");
    assert!(result.is_match(), "{:?}", result.outcome);

    let live_fingerprints = live.fingerprints(live.head());
    let rebuilt = scratch.fingerprints();
    assert_eq!(
        live_fingerprints
            .iter()
            .find(|f| f.name == "trial_balance")
            .and_then(|f| f.value("cash")),
        Some(&Amount::decimal("100.00").expect("a decimal"))
    );
    let fingerprints =
        compare_fingerprints(&live_fingerprints, &rebuilt).expect("fingerprints are named once");
    assert!(fingerprints.is_match(), "{:?}", fingerprints.differing);
    scratch.discard();
}

#[test]
fn the_live_side_refuses_every_write() {
    let Some(fx) = fixture() else {
        return;
    };
    let client = fx.client();
    put_party(&fx, &client, 1, "Acme");
    fx.set_head(&client, 1, 1);

    let side = live(&fx);
    // The probe is a `CREATE TEMP TABLE`: a read-only transaction refuses it with 25006.
    assert_eq!(side.probe_write().expect("the probe runs"), "25006");
}

#[test]
fn a_wrong_tenant_and_another_position_are_refused() {
    let Some(fx) = fixture() else {
        return;
    };
    let client = fx.client();
    put_party(&fx, &client, 1, "Acme");
    fx.set_head(&client, 1, 1);

    let side = live(&fx);
    let head = side.head();
    let error = match side.summaries_for(OTHER_TENANT, head) {
        Ok(_) => panic!("another tenant is refused"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "journal_audit_pg.tenant_mismatch");
    let error = side
        .fingerprints_for(OTHER_TENANT, head)
        .expect_err("another tenant is refused");
    assert_eq!(error.code(), "journal_audit_pg.tenant_mismatch");

    let error = match side.summaries_for(TENANT, Position::new(head.generation, head.seq + 1)) {
        Ok(_) => panic!("another position is refused"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "journal_audit_pg.as_of_not_head");

    // The trait's stream cannot carry the refusal, so it yields nothing and says why.
    let past = side.summaries(Position::new(head.generation + 1, head.seq));
    assert_eq!(past.count(), 0);
    assert_eq!(
        side.failure().map(|failure| failure.code()),
        Some("journal_audit_pg.as_of_not_head")
    );

    // A scratch side refuses another tenant too.
    let scratch = PgScratchSide::create(fx.bridge.clone(), TENANT, common::catalog(), "run2", 1)
        .expect("the scratch side");
    assert_eq!(
        scratch
            .summaries_for(OTHER_TENANT)
            .err()
            .map(|error| error.code()),
        Some("journal_audit_pg.tenant_mismatch")
    );
}

#[test]
fn a_scratch_schema_is_gone_after_discard_and_after_drop() {
    let Some(fx) = fixture() else {
        return;
    };
    let mut discarded =
        PgScratchSide::create(fx.bridge.clone(), TENANT, common::catalog(), "gone", 1)
            .expect("the scratch side");
    let name = discarded.schema().to_owned();
    assert!(
        PgScratchSide::schema_exists(fx.bridge.clone(), &name).expect("the probe"),
        "the schema exists after create"
    );
    discarded.discard();
    assert!(discarded.is_discarded());
    assert!(
        !PgScratchSide::schema_exists(fx.bridge.clone(), &name).expect("the probe"),
        "the schema is gone after discard"
    );

    let dropped = PgScratchSide::create(fx.bridge.clone(), TENANT, common::catalog(), "dropped", 1)
        .expect("the scratch side");
    let name = dropped.schema().to_owned();
    drop(dropped);
    assert!(
        !PgScratchSide::schema_exists(fx.bridge.clone(), &name).expect("the probe"),
        "the schema is gone after Drop"
    );
}

#[test]
fn a_panic_in_the_rebuild_leaves_no_scratch_schema() {
    let Some(fx) = fixture() else {
        return;
    };
    let bridge = fx.bridge.clone();
    let schema = PgScratchSide::create(bridge.clone(), TENANT, common::catalog(), "panics", 1)
        .expect("the scratch side")
        .schema()
        .to_owned();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let side = PgScratchSide::create(bridge.clone(), TENANT, common::catalog(), "panics", 1)
            .expect("the scratch side");
        assert_eq!(side.schema(), schema);
        panic!("the rebuild died");
    }));
    assert!(outcome.is_err(), "the rebuild panicked");
    assert!(
        !PgScratchSide::schema_exists(bridge, &schema).expect("the probe"),
        "the guard dropped the schema during the unwind"
    );
}

#[test]
fn the_sweep_removes_a_crashed_runs_leftover() {
    let Some(fx) = fixture() else {
        return;
    };
    let crashed = PgScratchSide::create(fx.bridge.clone(), TENANT, common::catalog(), "crashed", 1)
        .expect("the scratch side");
    let crashed_name = crashed.schema().to_owned();
    // A process that died before `Drop` could run leaves its schema behind.
    std::mem::forget(crashed);
    let running = PgScratchSide::create(fx.bridge.clone(), TENANT, common::catalog(), "running", 5)
        .expect("the scratch side");
    let running_name = running.schema().to_owned();

    let dropped = PgScratchSide::sweep(fx.bridge.clone(), TENANT, 3).expect("the sweep");
    assert_eq!(dropped, vec![crashed_name.clone()]);
    assert!(!PgScratchSide::schema_exists(fx.bridge.clone(), &crashed_name).expect("the probe"));
    assert!(
        PgScratchSide::schema_exists(fx.bridge.clone(), &running_name).expect("the probe"),
        "a run created after the cutoff is untouched"
    );
    drop(running);
}

#[test]
fn pages_are_bounded_and_the_stream_is_sorted() {
    let Some(fx) = fixture() else {
        return;
    };
    let client = fx.client();
    put_party(&fx, &client, 1, "Acme");
    for n in 1..=10 {
        put_invoice(&fx, &client, n, 1);
    }
    fx.set_head(&client, 1, 11);

    let side = live(&fx);
    let mut seen = Vec::new();
    let mut pages = side.pages_read();
    let mut since_page = 0_usize;
    for summary in side.summaries(side.head()) {
        // A new page is asked for only when the page in hand is exhausted, so the iterator never
        // holds more than one page: at most `PAGE` rows arrive between two page reads.
        if side.pages_read() != pages {
            pages = side.pages_read();
            since_page = 0;
        }
        since_page += 1;
        assert!(
            since_page <= usize::try_from(PAGE).unwrap_or(usize::MAX),
            "more rows than a page arrived without a new page"
        );
        seen.push(summary);
    }
    assert_eq!(seen.len(), 11, "ten invoices and one party");
    let mut sorted = seen.clone();
    sorted.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    assert_eq!(seen, sorted, "entity then id");
    // `gl_entry` is empty (one page); the ten invoices are 3+3+3+1 rows and then an empty page;
    // the party is one row and then an empty page: eight pages in all, and no record is read twice.
    assert_eq!(side.pages_read(), 8);
    assert_eq!(side.failure(), None);
}

#[test]
fn a_deleted_record_leaves_the_scratch_and_the_streams_agree() {
    let Some(fx) = fixture() else {
        return;
    };
    let client = fx.client();
    put_party(&fx, &client, 1, "Acme");
    put_invoice(&fx, &client, 1, 1);
    fx.set_head(&client, 1, 2);

    let live = live(&fx);
    let mut scratch = PgScratchSide::create(fx.bridge.clone(), TENANT, common::catalog(), "del", 1)
        .expect("the scratch side");
    apply(
        &mut scratch,
        &[
            ("party".to_owned(), 1, 1, party_body("Acme")),
            ("invoice".to_owned(), 1, 1, invoice_body(1, 1)),
        ],
    );
    let before = compare(
        live.summaries(live.head()),
        scratch.summaries(),
        &DerivedEntities::none(),
    )
    .expect("sorted");
    assert!(before.is_match());

    wait(scratch.apply_delete(spot_journal_rebuild::AppliedDelete {
        entity: "invoice".to_owned(),
        id: uuid(1),
        version: 2,
        position: spot_events_contract::envelope::Position {
            generation: 1,
            seq: 3,
        },
    }))
    .expect("the tombstone applies");
    let after: Vec<_> = scratch.summaries().collect();
    assert!(after.iter().all(|summary| summary.entity != "invoice"));
    assert_eq!(after.len(), 1, "only the party is left");
    // The live database still holds the record, so the compare now reports the difference.
    let result = compare(
        live.summaries(live.head()),
        scratch.summaries(),
        &DerivedEntities::none(),
    )
    .expect("sorted");
    assert!(!result.is_match());
    scratch.discard();
}

#[test]
fn a_body_the_scratch_cannot_store_is_refused_by_name() {
    let Some(fx) = fixture() else {
        return;
    };
    let mut scratch = PgScratchSide::create(fx.bridge.clone(), TENANT, common::catalog(), "bad", 1)
        .expect("the scratch side");
    // A child row without an id cannot be stored: the scratch table's key is the row's id.
    let mut body = invoice_body(1, 1);
    body.insert("lines".to_owned(), serde_json::json!([{"qty": 1}]));
    let answer =
        wait(scratch.apply_image(image("invoice", 1, 1, body))).expect_err("the image is refused");
    assert!(
        answer.reason.contains("journal_audit_pg.image_invalid"),
        "{}",
        answer.reason
    );
}
