//! The store against a real PostgreSQL (`SPOT_TEST_PG_URL`; skipped with a message when unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::thread;

use common::{bundle, fixture, node, requested, service, target_site, tenant};
use futures::executor::block_on;
use spot_migration_audit::RunId;
use spot_promotion::{MemoryPromotionStore, PromotionState, PromotionStore};

/// The cases every `PromotionStore` must show; run against both stores.
fn behaviour_table(store: &dyn PromotionStore, to: spot_tenancy::TenantId) {
    let alpha = RunId::new("alpha").unwrap();
    // Empty.
    assert!(store.list().unwrap().is_empty());
    assert!(store.load(&alpha).unwrap().is_none());
    // Insert, then a second insert of the id is refused and changes nothing.
    store.insert(requested("alpha", to)).unwrap();
    assert_eq!(
        store.insert(requested("alpha", to)).unwrap_err().code(),
        "promotion.duplicate"
    );
    let first = store.load(&alpha).unwrap().unwrap();
    assert_eq!(first.revision(), 0);
    assert_eq!(first.state(), PromotionState::Requested);
    // Save is a compare-and-swap on the revision.
    let stale = store.load(&alpha).unwrap().unwrap();
    store.save(first).unwrap();
    assert_eq!(store.load(&alpha).unwrap().unwrap().revision(), 1);
    assert_eq!(
        store.save(stale).unwrap_err().code(),
        "promotion_pg.stale_save"
    );
    assert_eq!(store.load(&alpha).unwrap().unwrap().revision(), 1);
    // Saving what was never inserted is `not_found`.
    assert_eq!(
        store.save(requested("beta", to)).unwrap_err().code(),
        "promotion.not_found"
    );
    // List is ordered by id.
    store.insert(requested("zulu", to)).unwrap();
    store.insert(requested("bravo", to)).unwrap();
    let ids: Vec<String> = store
        .list()
        .unwrap()
        .iter()
        .map(|p| p.id().to_string())
        .collect();
    assert_eq!(ids, ["alpha", "bravo", "zulu"]);
}

#[test]
fn the_memory_store_shows_the_behaviour_table() {
    behaviour_table(&MemoryPromotionStore::new(), tenant(1));
}

#[test]
fn the_pg_store_shows_the_same_behaviour_table() {
    let Some(fx) = fixture() else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    behaviour_table(&node(&fx, "promo_table", tenant(1)), tenant(1));
}

#[test]
fn a_promotion_saved_by_one_process_resumes_in_another() {
    let Some(fx) = fixture() else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let to = tenant(1);
    let bundle = bundle();
    let id = RunId::new("resume-1").unwrap();

    // Process one: request and plan, then it is gone.
    {
        let first = service(Arc::new(node(&fx, "promo_p1", to)));
        first
            .request(
                "resume-1",
                &bundle,
                common::env(spot_environments::Role::Dev, tenant(9)),
                common::env(spot_environments::Role::Test, to),
                "user:req",
                1_000,
            )
            .unwrap();
        let planned = block_on(first.plan(
            &id,
            &bundle,
            &target_site(to),
            Vec::new(),
            "user:req",
            1_100,
        ))
        .unwrap();
        // The test policy needs no approval for an additive bundle.
        assert_eq!(planned.state(), PromotionState::Approved);
    }

    // Process two: a new bridge, a new service; the promotion is read back and carried on.
    let second_store = Arc::new(node(&fx, "promo_p2", to));
    let held = second_store.load(&id).unwrap().unwrap();
    assert_eq!(held.state(), PromotionState::Approved);
    assert!(held.plan().is_some());
    assert!(held.plan().unwrap().apply_plan().is_none());
    let second = service(second_store.clone());
    let mut target = target_site(to);
    block_on(second.apply(&id, &bundle, &mut target, "user:req", 1_200)).unwrap();
    let applied = second_store.load(&id).unwrap().unwrap();
    assert_eq!(applied.state(), PromotionState::Applied);
    assert!(applied.pre_image().is_some());

    // Process three: verification from the stored state alone.
    let third_store = Arc::new(node(&fx, "promo_p3", to));
    let third = service(third_store.clone());
    block_on(third.verify(&id, &bundle, &mut target, "user:req", 1_300)).unwrap();
    let verified = third_store.load(&id).unwrap().unwrap();
    assert_eq!(verified.state(), PromotionState::Verified);
    assert_eq!(verified.journal().len(), 5);
}

#[test]
fn a_stale_save_from_a_second_process_is_refused() {
    let Some(fx) = fixture() else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let to = tenant(1);
    let one = node(&fx, "promo_s1", to);
    let two = node(&fx, "promo_s2", to);
    one.insert(requested("race", to)).unwrap();
    let id = RunId::new("race").unwrap();
    let from_one = one.load(&id).unwrap().unwrap();
    let from_two = two.load(&id).unwrap().unwrap();
    one.save(from_one).unwrap();
    assert_eq!(
        two.save(from_two).unwrap_err().code(),
        "promotion_pg.stale_save"
    );
    assert_eq!(two.load(&id).unwrap().unwrap().revision(), 1);
}

#[test]
fn fifty_concurrent_saves_of_one_revision_give_one_winner() {
    let Some(fx) = fixture() else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let to = tenant(1);
    let store = Arc::new(node(&fx, "promo_cas", to));
    store.insert(requested("cas", to)).unwrap();
    let id = RunId::new("cas").unwrap();
    // Every copy is loaded before any thread starts: a copy loaded while another thread is
    // already saving would hold the next revision, and rightly win.
    let copies: Vec<_> = (0..50).map(|_| store.load(&id).unwrap().unwrap()).collect();
    let handles: Vec<_> = copies
        .into_iter()
        .map(|promotion| {
            let store = Arc::clone(&store);
            thread::spawn(move || store.save(promotion))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for error in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(error.code(), "promotion_pg.stale_save");
    }
    assert_eq!(store.load(&id).unwrap().unwrap().revision(), 1);
}

#[test]
fn tenants_never_see_each_other() {
    let Some(fx) = fixture() else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let (a, b) = (tenant(1), tenant(2));
    let store_a = node(&fx, "promo_ta", a);
    let store_b = node(&fx, "promo_tb", b);
    // The same id in two tenants is two promotions.
    store_a.insert(requested("shared", a)).unwrap();
    store_b.insert(requested("shared", b)).unwrap();
    store_a.insert(requested("only-a", a)).unwrap();
    let shared = RunId::new("shared").unwrap();
    let only_a = RunId::new("only-a").unwrap();
    assert!(store_b.load(&only_a).unwrap().is_none());
    assert_eq!(store_a.list().unwrap().len(), 2);
    assert_eq!(store_b.list().unwrap().len(), 1);
    // Saving through one tenant moves only that tenant's revision.
    let held = store_a.load(&shared).unwrap().unwrap();
    store_a.save(held).unwrap();
    assert_eq!(store_a.load(&shared).unwrap().unwrap().revision(), 1);
    assert_eq!(store_b.load(&shared).unwrap().unwrap().revision(), 0);
    // A promotion for another tenant is refused outright and nothing is written.
    assert_eq!(
        store_b.insert(requested("alien", a)).unwrap_err().code(),
        "promotion.environment_mismatch"
    );
    assert!(
        store_b
            .load(&RunId::new("alien").unwrap())
            .unwrap()
            .is_none()
    );
    // A row planted under the wrong tenant is refused on read, not served.
    let raw = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(raw.batch_execute(&format!(
            "UPDATE spot_control.spot_promotion SET tenant = '{b}' \
             WHERE tenant = '{a}' AND id = 'only-a'"
        )))
        .unwrap();
    assert_eq!(
        store_b.load(&only_a).unwrap_err().code(),
        "promotion_pg.snapshot_invalid"
    );
}

#[test]
fn an_unknown_version_and_a_damaged_row_are_refused_not_repaired() {
    let Some(fx) = fixture() else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let to = tenant(1);
    let store = node(&fx, "promo_bad", to);
    store.insert(requested("bad", to)).unwrap();
    let id = RunId::new("bad").unwrap();
    let raw = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(raw.batch_execute(
            "UPDATE spot_control.spot_promotion \
             SET doc = jsonb_set(doc, '{version}', '99') WHERE id = 'bad'",
        ))
        .unwrap();
    assert_eq!(
        store.load(&id).unwrap_err().code(),
        "promotion_pg.snapshot_version"
    );
    assert_eq!(
        store.list().unwrap_err().code(),
        "promotion_pg.snapshot_version"
    );
    fx.rt
        .block_on(raw.batch_execute(
            "UPDATE spot_control.spot_promotion \
             SET doc = jsonb_set(doc, '{version}', '1') || '{\"surprise\": true}' WHERE id = 'bad'",
        ))
        .unwrap();
    assert_eq!(
        store.load(&id).unwrap_err().code(),
        "promotion_pg.snapshot_invalid"
    );
    // A renamed document no longer matches its row.
    fx.rt
        .block_on(raw.batch_execute(
            "UPDATE spot_control.spot_promotion \
             SET doc = (doc - 'surprise') || '{\"id\": \"other\"}' WHERE id = 'bad'",
        ))
        .unwrap();
    assert_eq!(
        store.load(&id).unwrap_err().code(),
        "promotion_pg.snapshot_invalid"
    );
}
