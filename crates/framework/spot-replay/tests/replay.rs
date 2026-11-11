//! Behavioural replay end to end over the storage fake and a toy document engine: the seeded
//! rounding change is found, a no-change release replays clean on 2,000 documents, nothing leaves
//! the clone and nothing is transmitted, and the budget and cursor work.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use spot_effect_hold::{EffectState, LedgerStore};
use spot_replay::{
    BehaviourEntry, Budget, BudgetStop, DeclaredBehaviourList, EffectHold, JournalEntry, Replay,
    ReplayOutcome, ReplayPolicy, Window, codes,
};
use support::{
    FailingJournal, FixedJournal, LIVE_LABEL, SteppingClock, ToyEngine, ToyRelease, clone_ref,
    held_gate, journal, manual_clock, reference_at, site,
};

/// A policy over the last `documents` documents, seeded, selecting up to `per_stratum`.
fn policy(documents: u32, per_stratum: u32) -> ReplayPolicy {
    ReplayPolicy::new(Window::LastDocuments { documents }, per_stratum).with_seed(7)
}

/// Builds a replay over `entries` with `new`, on a held clone.
async fn harness(
    new: ToyRelease,
    entries: Vec<JournalEntry>,
    policy: ReplayPolicy,
    budget: Budget,
) -> (
    Arc<ToyEngine>,
    Arc<spot_effect_hold::InMemoryLedger>,
    Replay,
) {
    let (gate, ledger, site) = held_gate().await;
    let hold = EffectHold::engage(gate, site.clone(), 0)
        .await
        .expect("held");
    let engine = Arc::new(ToyEngine::new(new, clone_ref().label(), LIVE_LABEL).await);
    let source = Arc::new(FixedJournal::new(entries));
    let replay = Replay::new(
        source,
        engine.clone(),
        clone_ref(),
        site,
        manual_clock(),
        hold,
    )
    .with_policy(policy)
    .with_reference_at(reference_at())
    .with_budget(budget);
    (engine, ledger, replay)
}

/// A generous budget for the tests that are not about the budget.
fn roomy() -> Budget {
    Budget::new(10_000, Duration::from_secs(600))
}

#[tokio::test]
async fn a_seeded_rounding_change_is_detected() {
    let old = ToyRelease::old();
    let (engine, ledger, replay) = harness(
        ToyRelease::seeded_rounding_change(),
        journal(50, &old),
        policy(1000, 1000),
        roomy(),
    )
    .await;

    let report = replay.run().await.expect("run");
    assert_eq!(report.coverage.replayed, 50);
    assert!(report.coverage.complete());
    assert_eq!(report.outcome, ReplayOutcome::Unexplained);

    // Every document's tax and total differ by exactly one minor unit.
    let tax: Vec<_> = report
        .divergences
        .iter()
        .filter(|d| d.field == "totals.tax")
        .collect();
    assert_eq!(tax.len(), 50);
    assert!(
        tax.iter()
            .all(|d| d.delta.map(i128::abs) == Some(1) && d.rounding)
    );
    assert!(report.rounding().count() >= 50);
    assert!(report.render_text().contains("rounding"));
    assert!(report.to_json().contains("rounding"));

    // Nothing left the clone, and every write landed in the clone's store.
    let clone_label = clone_ref().label().to_owned();
    assert!(engine.targets().iter().all(|t| *t == clone_label));
    assert_eq!(engine.sends(), 0, "an effect escaped");
    assert_eq!(engine.held(), engine.attempts());
    assert_eq!(
        ledger
            .list_effects(&site(), &[EffectState::Held])
            .await
            .expect("list")
            .len(),
        50
    );
    for (label, record) in engine.writes() {
        assert_eq!(label, clone_label);
        assert!(engine.has_document(&clone_label, record).await);
        assert!(!engine.has_document(LIVE_LABEL, record).await);
    }
}

#[tokio::test]
async fn a_release_with_no_behaviour_change_replays_clean_on_2000_documents() {
    let old = ToyRelease::old();
    let (engine, _ledger, replay) = harness(
        ToyRelease::unchanged(),
        journal(2000, &old),
        policy(3000, 3000),
        Budget::new(5000, Duration::from_secs(600)),
    )
    .await;

    let report = replay.run().await.expect("run");
    assert_eq!(report.coverage.considered, 2000);
    assert_eq!(report.coverage.selected, 2000);
    assert_eq!(report.coverage.replayed, 2000);
    assert!(report.coverage.complete());
    assert!(
        report.divergences.is_empty(),
        "unexpected: {:?}",
        report.divergences.first()
    );
    assert_eq!(report.outcome, ReplayOutcome::Clean);
    assert_eq!(engine.writes().len(), 2000);
    assert_eq!(engine.sends(), 0);
}

#[tokio::test]
async fn replay_writes_only_to_the_clone_and_emits_nothing() {
    let old = ToyRelease::old();
    let (engine, ledger, replay) = harness(
        ToyRelease::unchanged(),
        journal(20, &old),
        policy(100, 100),
        roomy(),
    )
    .await;

    let report = replay.run().await.expect("run");
    assert_eq!(report.outcome, ReplayOutcome::Clean);

    let clone_label = clone_ref().label().to_owned();
    // Every operation was handed the clone, every write landed in it, and none in the live store.
    assert_eq!(engine.targets().len(), 20);
    assert!(engine.targets().iter().all(|t| *t == clone_label));
    assert_eq!(engine.writes().len(), 20);
    for (label, record) in engine.writes() {
        assert_eq!(label, clone_label);
        assert!(engine.has_document(&clone_label, record).await);
        assert!(!engine.has_document(LIVE_LABEL, record).await);
    }
    // Every external effect was attempted and held; none left.
    assert_eq!(engine.attempts(), 20);
    assert_eq!(engine.held(), 20);
    assert_eq!(engine.refused(), 0);
    assert_eq!(engine.sends(), 0);
    let held = ledger
        .list_effects(&site(), &[EffectState::Held])
        .await
        .expect("list");
    assert_eq!(held.len(), 20);
}

#[tokio::test]
async fn a_budget_stops_cleanly_and_a_cursor_resumes() {
    let old = ToyRelease::old();
    let (_engine, _ledger, replay) = harness(
        ToyRelease::seeded_rounding_change(),
        journal(10, &old),
        policy(20, 20),
        Budget::new(3, Duration::from_secs(600)),
    )
    .await;

    let first = replay.run().await.expect("run");
    assert_eq!(first.coverage.replayed, 3);
    assert!(!first.coverage.complete());
    assert_eq!(first.coverage.stop, Some(BudgetStop::Documents));

    let mut total = first.coverage.replayed;
    let mut report = first;
    let mut rounds = 0;
    while !report.coverage.complete() {
        let cursor = report.cursor.clone().expect("a cursor to resume from");
        report = replay.resume(cursor).await.expect("resume");
        total += report.coverage.replayed;
        rounds += 1;
        assert!(rounds < 10, "the resume loop did not terminate");
    }
    assert_eq!(total, 10, "every document was replayed exactly once");
}

#[tokio::test]
async fn the_duration_budget_stops_the_run() {
    let old = ToyRelease::old();
    let (gate, _ledger, site) = held_gate().await;
    let hold = EffectHold::engage(gate, site.clone(), 0)
        .await
        .expect("held");
    let engine =
        Arc::new(ToyEngine::new(ToyRelease::unchanged(), clone_ref().label(), LIVE_LABEL).await);
    let source = Arc::new(FixedJournal::new(journal(50, &old)));
    let clock = Arc::new(SteppingClock::new(Duration::from_secs(10)));
    let replay = Replay::new(source, engine, clone_ref(), site, clock, hold)
        .with_policy(policy(100, 100))
        .with_reference_at(reference_at())
        .with_budget(Budget::new(1000, Duration::from_secs(30)));

    let report = replay.run().await.expect("run");
    assert!(!report.coverage.complete());
    assert_eq!(report.coverage.stop, Some(BudgetStop::Duration));
    assert!(report.coverage.replayed < 50);
}

#[tokio::test]
async fn a_clone_of_another_site_is_refused() {
    let (gate, _ledger, site) = held_gate().await;
    let hold = EffectHold::engage(gate, site.clone(), 0)
        .await
        .expect("held");
    let engine = Arc::new(ToyEngine::new(ToyRelease::old(), clone_ref().label(), LIVE_LABEL).await);
    let replay = Replay::new(
        Arc::new(FixedJournal::new(Vec::new())),
        engine,
        spot_replay::CloneRef::new("other", "spot_scratch"),
        site,
        manual_clock(),
        hold,
    )
    .with_reference_at(reference_at());

    let error = replay
        .run()
        .await
        .expect_err("the clone is of another site");
    assert_eq!(error.code(), codes::NOT_CLONE);
}

#[tokio::test]
async fn a_journal_failure_aborts_the_run() {
    let (gate, _ledger, site) = held_gate().await;
    let hold = EffectHold::engage(gate, site.clone(), 0)
        .await
        .expect("held");
    let engine = Arc::new(ToyEngine::new(ToyRelease::old(), clone_ref().label(), LIVE_LABEL).await);
    let replay = Replay::new(
        Arc::new(FailingJournal),
        engine,
        clone_ref(),
        site,
        manual_clock(),
        hold,
    )
    .with_reference_at(reference_at());

    let error = replay.run().await.expect_err("the journal fails");
    assert_eq!(error.code(), codes::JOURNAL_FAILED);
}

#[tokio::test]
async fn a_declared_behaviour_entry_makes_a_divergence_expected() {
    let old = ToyRelease::old();
    let (gate, _ledger, site) = held_gate().await;
    let hold = EffectHold::engage(gate, site.clone(), 0)
        .await
        .expect("held");
    let engine = Arc::new(
        ToyEngine::new(
            ToyRelease::seeded_rounding_change(),
            clone_ref().label(),
            LIVE_LABEL,
        )
        .await,
    );
    let source = Arc::new(FixedJournal::new(journal(5, &old)));
    let declaration = Arc::new(DeclaredBehaviourList::new(vec![
        BehaviourEntry::new("behaviour/tax-rounding")
            .for_field("totals.tax")
            .within(1),
    ]));
    let replay = Replay::new(source, engine, clone_ref(), site, manual_clock(), hold)
        .with_policy(policy(100, 100))
        .with_reference_at(reference_at())
        .with_budget(roomy())
        .with_declaration(declaration);

    let report = replay.run().await.expect("run");
    let tax: Vec<_> = report
        .expected()
        .filter(|d| d.field == "totals.tax")
        .collect();
    assert_eq!(tax.len(), 5);
    assert!(
        tax.iter()
            .all(|d| { d.declared_by.as_deref() == Some("behaviour/tax-rounding") })
    );
    // The total is not declared, so it stays unexplained.
    assert!(report.unexplained().any(|d| d.field == "totals.total"));
    assert_eq!(report.outcome, ReplayOutcome::Unexplained);
}
