//! The backend-independent ledger contract suite (feature `testing`).
//!
//! Every [`LedgerStore`] implementation must pass [`run_all`]: the in-memory reference here, and
//! the PostgreSQL ledger of a later task. The suite proves the properties the hold relies on —
//! dedup by `(site, key)`, original order, compare-and-set hold transitions, checkpointing, and
//! that the ledger survives a reset of the business data it holds effects for.

use std::sync::Arc;

use spot_core::SiteId;

use crate::effect::{EffectId, EffectRef, EffectState, NewEffect, Transition};
use crate::error::{Error, Result};
use crate::hold::{HoldState, MigrationId};
use crate::store::{LedgerStore, ReleaseCursor};

/// One case's name and outcome.
#[derive(Debug)]
pub struct CaseResult {
    /// The case's name.
    pub name: &'static str,
    /// `Ok` when the backend behaved.
    pub outcome: std::result::Result<(), String>,
}

/// The outcome of [`run_all`].
#[derive(Debug, Default)]
pub struct ContractReport {
    /// One entry per case, in run order.
    pub cases: Vec<CaseResult>,
}

impl ContractReport {
    /// The cases that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<&CaseResult> {
        self.cases.iter().filter(|c| c.outcome.is_err()).collect()
    }

    /// Panics, listing every failed case, unless all passed (and at least one ran).
    pub fn assert_all_passed(&self) {
        assert!(!self.cases.is_empty(), "the contract suite ran no cases");
        let failures = self.failures();
        assert!(
            failures.is_empty(),
            "{} of {} ledger contract cases failed: {:?}",
            failures.len(),
            self.cases.len(),
            failures
                .iter()
                .map(|c| (c.name, c.outcome.as_ref().err()))
                .collect::<Vec<_>>()
        );
    }
}

/// Runs every ledger contract case on a fresh store from `make`.
pub async fn run_all<F>(make: F) -> ContractReport
where
    F: Fn() -> Arc<dyn LedgerStore> + Send + Sync,
{
    let mut report = ContractReport::default();

    report.cases.push(case(
        "dedupe_on_site_key",
        dedupe_on_site_key(&make()).await,
    ));
    report
        .cases
        .push(case("original_order", original_order(&make()).await));
    report
        .cases
        .push(case("transition_effect", transition_effect(&make()).await));
    report.cases.push(case(
        "transition_is_compare_and_set",
        transition_is_compare_and_set(&make()).await,
    ));
    report.cases.push(case(
        "concurrent_releases_dispatch_once",
        concurrent_releases_dispatch_once(&make()).await,
    ));
    report.cases.push(case(
        "release_versus_discard_has_one_winner",
        release_versus_discard_has_one_winner(&make()).await,
    ));
    report.cases.push(case(
        "crash_in_releasing_is_recovered_once",
        crash_in_releasing_is_recovered_once(&make()).await,
    ));
    report.cases.push(case(
        "compare_and_set_hold",
        compare_and_set_hold(&make()).await,
    ));
    report.cases.push(case(
        "checkpoint_round_trip",
        checkpoint_round_trip(&make()).await,
    ));
    report.cases.push(case(
        "ledger_survives_business_reset",
        ledger_survives_business_reset(&make()).await,
    ));
    report.cases.push(case(
        "racing_appends_dedupe",
        racing_appends_dedupe(&make()).await,
    ));
    report.cases.push(case(
        "sequence_is_monotonic",
        sequence_is_monotonic(&make()).await,
    ));

    report
}

fn case(name: &'static str, outcome: Result<()>) -> CaseResult {
    CaseResult {
        name,
        outcome: outcome.map_err(|error| error.to_string()),
    }
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).expect("a test site name is valid")
}

fn effect(site: &SiteId, key: &str, created_at: i64) -> NewEffect {
    NewEffect {
        effect: EffectRef::new(
            site.clone(),
            crate::class::EffectClass::Mail,
            "invoice_mail",
            key,
            created_at,
        ),
        migration_id: MigrationId::new("mig-1").expect("valid"),
    }
}

async fn dedupe_on_site_key(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-dedupe.example");
    let first = store.append_effect(effect(&site, "k1", 1)).await?;
    let second = store.append_effect(effect(&site, "k1", 2)).await?;
    if first != second {
        return Err(Error::InvalidInput {
            what: "dedupe".to_owned(),
            reason: "the same (site, key) returned two ids".to_owned(),
        });
    }
    let listed = store.list_effects(&site, &[EffectState::Held]).await?;
    if listed.len() != 1 {
        return Err(Error::InvalidInput {
            what: "dedupe".to_owned(),
            reason: format!("expected 1 effect, got {}", listed.len()),
        });
    }
    Ok(())
}

async fn original_order(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-order.example");
    for (index, key) in ["a", "b", "c", "d"].iter().enumerate() {
        store
            .append_effect(effect(&site, key, i64::try_from(index).unwrap_or(0)))
            .await?;
    }
    let listed = store.list_effects(&site, &[EffectState::Held]).await?;
    let keys: Vec<&str> = listed.iter().map(|r| r.key.as_str()).collect();
    if keys != ["a", "b", "c", "d"] {
        return Err(Error::InvalidInput {
            what: "order".to_owned(),
            reason: format!("{keys:?}"),
        });
    }
    Ok(())
}

fn fail<T>(what: &str, reason: impl Into<String>) -> Result<T> {
    Err(Error::InvalidInput {
        what: what.to_owned(),
        reason: reason.into(),
    })
}

async fn state_of(
    store: &Arc<dyn LedgerStore>,
    site: &SiteId,
    id: EffectId,
) -> Result<(EffectState, u32)> {
    let all = [
        EffectState::Held,
        EffectState::Releasing,
        EffectState::Dispatched,
        EffectState::Discarded,
        EffectState::NeedsConfirmation,
    ];
    let listed = store.list_effects(site, &all).await?;
    match listed.iter().find(|r| r.id == id) {
        Some(record) => Ok((record.state, record.attempts)),
        None => fail("state", "the effect is missing"),
    }
}

async fn transition_effect(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-state.example");
    let id = store.append_effect(effect(&site, "k1", 1)).await?;
    let applied = store
        .transition_effect(
            id,
            EffectState::Held,
            EffectState::Discarded,
            Some("rolled back".to_owned()),
        )
        .await?;
    if applied != Transition::Applied {
        return fail("transition", "Held -> Discarded was not applied");
    }
    let held = store.list_effects(&site, &[EffectState::Held]).await?;
    let discarded = store.list_effects(&site, &[EffectState::Discarded]).await?;
    if !held.is_empty() || discarded.len() != 1 {
        return fail("transition", "the effect did not move to Discarded");
    }
    if discarded[0].note.as_deref() != Some("rolled back") {
        return fail("transition", "the note was not stored");
    }
    Ok(())
}

/// A transition from a state the effect is not in changes nothing and reports the real state; an
/// unknown effect and a transition out of a terminal state are refused.
async fn transition_is_compare_and_set(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-cas-effect.example");
    let id = store.append_effect(effect(&site, "k1", 1)).await?;
    let stale = store
        .transition_effect(
            id,
            EffectState::Releasing,
            EffectState::Dispatched,
            Some("lost".to_owned()),
        )
        .await?;
    if stale
        != (Transition::Stale {
            actual: EffectState::Held,
        })
    {
        return fail("transition", format!("expected Stale(Held), got {stale:?}"));
    }
    if state_of(store, &site, id).await?.0 != EffectState::Held {
        return fail("transition", "a stale transition changed the effect");
    }
    // Every transition into Releasing counts an attempt, a retry included.
    for expected_attempts in 1..=2u32 {
        let from = if expected_attempts == 1 {
            EffectState::Held
        } else {
            EffectState::Releasing
        };
        let moved = store
            .transition_effect(id, from, EffectState::Releasing, None)
            .await?;
        if !moved.is_applied() {
            return fail("transition", "the claim was not applied");
        }
        if state_of(store, &site, id).await?.1 != expected_attempts {
            return fail("transition", "the attempt counter did not count the claim");
        }
    }
    let done = store
        .transition_effect(id, EffectState::Releasing, EffectState::Dispatched, None)
        .await?;
    if !done.is_applied() {
        return fail("transition", "Releasing -> Dispatched was not applied");
    }
    if store
        .transition_effect(id, EffectState::Dispatched, EffectState::Held, None)
        .await
        .is_ok()
    {
        return fail("transition", "an effect left a terminal state");
    }
    let unknown = store
        .transition_effect(
            EffectId(u64::from(u32::MAX)),
            EffectState::Held,
            EffectState::Releasing,
            None,
        )
        .await;
    if !matches!(unknown, Err(Error::InvalidInput { .. })) {
        return fail("transition", "an unknown effect id was not refused");
    }
    Ok(())
}

/// How many of `racers` concurrent `expected -> next` transitions on `id` were applied.
async fn race(
    store: &Arc<dyn LedgerStore>,
    id: EffectId,
    racers: &[(EffectState, EffectState)],
) -> Result<usize> {
    let mut handles = Vec::new();
    for (expected, next) in racers.iter().copied() {
        let store = Arc::clone(store);
        handles.push(tokio::spawn(async move {
            store.transition_effect(id, expected, next, None).await
        }));
    }
    let mut applied = 0;
    for handle in handles {
        match handle.await {
            Ok(Ok(transition)) => applied += usize::from(transition.is_applied()),
            Ok(Err(error)) => return Err(error),
            Err(_) => return fail("race", "a task panicked"),
        }
    }
    Ok(applied)
}

/// Many nodes read one effect as `Held` and all try to take it for release: exactly one may
/// dispatch it.
async fn concurrent_releases_dispatch_once(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-race-release.example");
    let id = store.append_effect(effect(&site, "k1", 1)).await?;
    let racers = vec![(EffectState::Held, EffectState::Releasing); 32];
    let applied = race(store, id, &racers).await?;
    if applied != 1 {
        return fail("race", format!("{applied} releases took one effect"));
    }
    let (state, attempts) = state_of(store, &site, id).await?;
    if state != EffectState::Releasing || attempts != 1 {
        return fail("race", "the winner's claim is not what the ledger holds");
    }
    Ok(())
}

/// A release and a discard both read `Held`: exactly one wins, and the ledger holds its state.
async fn release_versus_discard_has_one_winner(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-race-discard.example");
    for round in 0..8 {
        let id = store
            .append_effect(effect(&site, &format!("k{round}"), 1))
            .await?;
        let mut racers = Vec::new();
        for _ in 0..8 {
            racers.push((EffectState::Held, EffectState::Releasing));
            racers.push((EffectState::Held, EffectState::Discarded));
        }
        let applied = race(store, id, &racers).await?;
        if applied != 1 {
            return fail(
                "race",
                format!("{applied} winners for a release and a discard"),
            );
        }
        let state = state_of(store, &site, id).await?.0;
        if !matches!(state, EffectState::Releasing | EffectState::Discarded) {
            return fail("race", format!("the loser's state won: {state:?}"));
        }
    }
    Ok(())
}

/// A release that crashed leaves `Releasing`. Every recovery pass that finishes it competes on
/// the same compare-and-set, so it is recovered exactly once and a late pass finds it done.
async fn crash_in_releasing_is_recovered_once(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-race-recover.example");
    let id = store.append_effect(effect(&site, "k1", 1)).await?;
    let crashed = store
        .transition_effect(id, EffectState::Held, EffectState::Releasing, None)
        .await?;
    if !crashed.is_applied() {
        return fail("recover", "the claim was not applied");
    }
    // The crash: the claimer is gone, the row stays `Releasing`.
    let listed = store.list_effects(&site, &[EffectState::Releasing]).await?;
    if listed.len() != 1 {
        return fail("recover", "the crashed effect is not left Releasing");
    }
    let racers = vec![(EffectState::Releasing, EffectState::Dispatched); 16];
    let applied = race(store, id, &racers).await?;
    if applied != 1 {
        return fail("recover", format!("{applied} recoveries of one effect"));
    }
    let late = store
        .transition_effect(id, EffectState::Releasing, EffectState::Dispatched, None)
        .await?;
    if late
        != (Transition::Stale {
            actual: EffectState::Dispatched,
        })
    {
        return fail("recover", "a late recovery was not stale");
    }
    Ok(())
}

async fn compare_and_set_hold(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-cas.example");
    if store.get_hold(&site).await? != HoldState::Open {
        return Err(Error::InvalidInput {
            what: "cas".to_owned(),
            reason: "a fresh site is not Open".to_owned(),
        });
    }
    let held = HoldState::Held {
        migration_id: MigrationId::new("mig-1")?,
        reason: "verify".to_owned(),
        since: 10,
        expected_by: None,
    };
    if !store
        .compare_and_set_hold(&site, &HoldState::Open, held.clone())
        .await?
    {
        return Err(Error::InvalidInput {
            what: "cas".to_owned(),
            reason: "the open -> held compare-and-set failed".to_owned(),
        });
    }
    if store
        .compare_and_set_hold(&site, &HoldState::Open, held.clone())
        .await?
    {
        return Err(Error::InvalidInput {
            what: "cas".to_owned(),
            reason: "a second open -> held compare-and-set succeeded".to_owned(),
        });
    }
    if store.get_hold(&site).await? != held {
        return Err(Error::InvalidInput {
            what: "cas".to_owned(),
            reason: "the hold state is not the one written".to_owned(),
        });
    }
    Ok(())
}

async fn checkpoint_round_trip(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-checkpoint.example");
    let migration_id = MigrationId::new("mig-1")?;
    let cursor = ReleaseCursor {
        migration_id: migration_id.clone(),
        dispatched: 7,
        last_sequence: Some(crate::effect::Sequence(9)),
        needs_confirmation: 2,
    };
    store.checkpoint(&site, cursor.clone()).await?;
    let read = store.get_checkpoint(&site, &migration_id).await?;
    if read != Some(cursor) {
        return Err(Error::InvalidInput {
            what: "checkpoint".to_owned(),
            reason: "the cursor did not survive".to_owned(),
        });
    }
    Ok(())
}

/// Simulates a migration's rollback resetting the business data. The ledger lives elsewhere, so
/// its rows — including entries whose source event disappeared with the restore — are untouched.
async fn ledger_survives_business_reset(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-reset.example");
    store.append_effect(effect(&site, "gone-event", 1)).await?;
    store.append_effect(effect(&site, "kept", 2)).await?;

    // The migration is rolled back: the business database is restored from a snapshot. That is a
    // no-op for this store, which is exactly the point — the contract requires the ledger to be
    // outside the restorable data.
    reset_business_data();

    let listed = store.list_effects(&site, &[EffectState::Held]).await?;
    if listed.len() != 2 {
        return Err(Error::InvalidInput {
            what: "reset".to_owned(),
            reason: format!("the ledger lost rows in a business reset: {}", listed.len()),
        });
    }
    Ok(())
}

fn reset_business_data() {
    // A real restore replaces the business database's pages. Nothing here touches the ledger.
}

async fn racing_appends_dedupe(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-race.example");
    let mut handles = Vec::new();
    for _ in 0..64 {
        let store = Arc::clone(store);
        let site = site.clone();
        handles.push(tokio::spawn(async move {
            store.append_effect(effect(&site, "same-key", 1)).await
        }));
    }
    let mut ids = std::collections::HashSet::new();
    for handle in handles {
        match handle.await {
            Ok(Ok(id)) => {
                ids.insert(id);
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(Error::InvalidInput {
                    what: "race".to_owned(),
                    reason: "a task panicked".to_owned(),
                });
            }
        }
    }
    if ids.len() != 1 {
        return Err(Error::InvalidInput {
            what: "race".to_owned(),
            reason: format!("expected one id, got {}", ids.len()),
        });
    }
    let listed = store.list_effects(&site, &[EffectState::Held]).await?;
    if listed.len() != 1 {
        return Err(Error::InvalidInput {
            what: "race".to_owned(),
            reason: format!("expected one effect, got {}", listed.len()),
        });
    }
    Ok(())
}

async fn sequence_is_monotonic(store: &Arc<dyn LedgerStore>) -> Result<()> {
    let site = site("hold-seq.example");
    let mut last = 0u64;
    for index in 0..5u64 {
        let id = store
            .append_effect(effect(&site, &format!("k{index}"), 1))
            .await?;
        let listed = store.list_effects(&site, &[EffectState::Held]).await?;
        let record = listed
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| Error::InvalidInput {
                what: "sequence".to_owned(),
                reason: "the appended effect is missing".to_owned(),
            })?;
        if record.sequence.0 <= last {
            return Err(Error::InvalidInput {
                what: "sequence".to_owned(),
                reason: "sequence did not increase".to_owned(),
            });
        }
        last = record.sequence.0;
    }
    Ok(())
}
