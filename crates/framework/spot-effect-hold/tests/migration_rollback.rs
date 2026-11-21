//! The acceptance line made literal: no external call leaves during a migration that is later
//! rolled back.
//!
//! Five scenarios: discard (A), release (B), crash and resume (C), the fail-closed edges (D), and
//! sixty-four concurrent dispatchers racing a `begin` (E).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use common::{CrashyLedger, DAY_MICROS, NOW, World, effect, mixed_class, world};
use spot_effect_hold::class::ClassPolicyConfig;
use spot_effect_hold::{
    Admission, EdgeSyncGate, EffectClass, EffectRef, EffectState, Error, Gate, HoldState,
    LedgerStore, MigrationId, PolicyConfig, PolicyTable, QueueGate, ReleasePlan, UnavailableLedger,
    codes,
};

const COUNT: usize = 100;
const FISCAL: usize = 5;

/// The ordering source of an effect: a job uses its queue name, everything else its class key.
fn source_of(class: &EffectClass) -> String {
    match class {
        EffectClass::JobQueue(name) => name.clone(),
        other => other.key(),
    }
}

fn class_of(index: usize) -> EffectClass {
    if index < FISCAL {
        EffectClass::FiscalTransmission
    } else {
        mixed_class(index)
    }
}

/// Runs the 100 effects through the gate and returns how many fiscal transmissions proceeded.
async fn admit_all(world: &World) -> usize {
    let mut fiscal_proceeds = 0;
    for index in 0..COUNT {
        let class = class_of(index);
        let reference = effect(
            &world.site,
            class.clone(),
            &format!("k{index}"),
            &source_of(&class),
        );
        match world.service.gate().admit(&reference, NOW).await {
            Admission::Proceed => {
                assert!(
                    class.is_fiscal(),
                    "a non-fiscal effect proceeded while held: {class:?}"
                );
                fiscal_proceeds += 1;
            }
            Admission::Held(ticket) => {
                assert!(!class.is_fiscal(), "a fiscal effect was held: {class:?}");
                assert!(
                    ticket.durable,
                    "a held effect was not recorded in the ledger"
                );
            }
        }
    }
    fiscal_proceeds
}

/// Scenario A: begin, run 100 mixed effects, discard. Nothing external was called.
#[tokio::test]
async fn scenario_a_discard_calls_nothing() {
    let world = world("scenario-a.example");
    world.service.load(NOW).await;
    world
        .service
        .begin("mig-a", "verify upgrade", NOW, Some(NOW + DAY_MICROS))
        .await
        .unwrap();

    // While held: jobs are not claimable, the edge neither sends nor applies.
    let queue_gate = QueueGate::new(world.service.gate());
    let edge_gate = EdgeSyncGate::new(world.service.gate());
    assert!(!queue_gate.is_claimable("default"));
    assert!(!edge_gate.may_send("edge-1"));
    assert!(!edge_gate.may_apply("edge-1"));

    let fiscal_proceeds = admit_all(&world).await;
    assert_eq!(
        fiscal_proceeds, FISCAL,
        "fiscal transmissions must never be held"
    );

    let report = world
        .service
        .discard("mig-a", "verification failed; rolled back", NOW + 1000)
        .await
        .unwrap();
    assert_eq!(report.discarded, (COUNT - FISCAL) as u64);
    assert_eq!(report.notified, 19, "every bank instruction raises a task");

    // No external call left.
    assert_eq!(world.dispatcher.unique_count(), 0);
    let discarded = world
        .ledger
        .list_effects(&world.site, &[EffectState::Discarded])
        .await
        .unwrap();
    assert_eq!(discarded.len(), COUNT - FISCAL);
    let held = world
        .ledger
        .list_effects(&world.site, &[EffectState::Held])
        .await
        .unwrap();
    assert!(held.is_empty());

    // The audit chain recorded the begin and the discard.
    use spot_effect_hold::AuditKind;
    assert_eq!(world.audit.events_of_kind(AuditKind::Begin).len(), 1);
    assert_eq!(world.audit.events_of_kind(AuditKind::Discard).len(), 1);
}

/// Scenario B: begin, run 100 mixed effects, release. Every held effect is dispatched exactly
/// once, in the original order per source, with its original idempotency key.
#[tokio::test]
async fn scenario_b_release_dispatches_in_order() {
    let world = world("scenario-b.example");
    world.service.load(NOW).await;
    world
        .service
        .begin("mig-b", "verify upgrade", NOW, None)
        .await
        .unwrap();
    let fiscal_proceeds = admit_all(&world).await;
    assert_eq!(fiscal_proceeds, FISCAL);

    let report = world
        .service
        .release("mig-b", ReleasePlan::new(), NOW + 1000)
        .await
        .unwrap();
    assert!(report.completed);
    assert_eq!(report.dispatched.len(), COUNT - FISCAL);
    assert_eq!(world.dispatcher.unique_count(), COUNT - FISCAL);

    // Per source: the original keys, in the original order, once each.
    let mut expected: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for index in FISCAL..COUNT {
        let class = class_of(index);
        expected
            .entry(source_of(&class))
            .or_default()
            .push(format!("k{index}"));
    }
    for (source, keys) in expected {
        let calls = world.dispatcher.calls_for_source(&source);
        let got: Vec<String> = calls.iter().map(|call| call.key.clone()).collect();
        assert_eq!(got, keys, "source {source} was not dispatched in order");
        let sequences: Vec<u64> = calls.iter().map(|call| call.sequence.0).collect();
        let mut sorted = sequences.clone();
        sorted.sort_unstable();
        assert_eq!(
            sequences, sorted,
            "source {source} is out of sequence order"
        );
    }

    // Fiscal transmissions were never in the ledger.
    let dispatched = world
        .ledger
        .list_effects(&world.site, &[EffectState::Dispatched])
        .await
        .unwrap();
    assert_eq!(dispatched.len(), COUNT - FISCAL);
    assert!(dispatched.iter().all(|record| !record.class.is_fiscal()));

    // The hold is released and a second release is refused.
    assert!(matches!(
        world.ledger.get_hold(&world.site).await.unwrap(),
        HoldState::Released { .. }
    ));
    assert!(matches!(
        world
            .service
            .release("mig-b", ReleasePlan::new(), NOW + 2000)
            .await,
        Err(Error::NotHeld { .. })
    ));
}

/// Scenario C: crash (a ledger that fails) between every two ledger operations, restart from the
/// ledger, finish. Still exactly once, still ordered.
#[tokio::test]
async fn scenario_c_crash_and_resume_release() {
    let world = world("scenario-c.example");
    world.service.load(NOW).await;
    world
        .service
        .begin("mig-c", "verify upgrade", NOW, None)
        .await
        .unwrap();
    admit_all(&world).await;

    let mut rounds = 0usize;
    loop {
        rounds += 1;
        assert!(rounds < 600, "the resuming release never completed");
        let ledger = CrashyLedger::new(Arc::clone(&world.ledger), rounds * 2);
        let service = world.service_over(ledger as Arc<dyn LedgerStore>);
        match service
            .release("mig-c", ReleasePlan::new(), NOW + 1000)
            .await
        {
            Ok(report) => {
                assert!(
                    report.completed,
                    "release reported incomplete without confirmation needs"
                );
                break;
            }
            Err(Error::LedgerUnavailable { .. }) => continue,
            Err(other) => panic!("unexpected release error: {other}"),
        }
    }

    assert_eq!(world.dispatcher.unique_count(), COUNT - FISCAL);
    for source in ["mail", "bank_api", "webhook", "default", "edge_sync"] {
        let sequences: Vec<u64> = world
            .dispatcher
            .calls_for_source(source)
            .iter()
            .map(|call| call.sequence.0)
            .collect();
        let mut sorted = sequences.clone();
        sorted.sort_unstable();
        assert_eq!(
            sequences, sorted,
            "source {source} lost its order across restarts"
        );
    }
    let terminal = world
        .ledger
        .list_effects(&world.site, &[EffectState::Dispatched])
        .await
        .unwrap();
    assert_eq!(terminal.len(), COUNT - FISCAL);
}

/// Scenario C for discard: crash between every two operations, resume, finish with nothing sent.
#[tokio::test]
async fn scenario_c_crash_and_resume_discard() {
    let world = world("scenario-c2.example");
    world.service.load(NOW).await;
    world
        .service
        .begin("mig-c2", "verify upgrade", NOW, None)
        .await
        .unwrap();
    admit_all(&world).await;

    let mut rounds = 0usize;
    loop {
        rounds += 1;
        assert!(rounds < 600, "the resuming discard never completed");
        let ledger = CrashyLedger::new(Arc::clone(&world.ledger), rounds * 2);
        let service = world.service_over(ledger as Arc<dyn LedgerStore>);
        match service
            .discard("mig-c2", "verification failed", NOW + 1000)
            .await
        {
            Ok(_) => break,
            Err(Error::LedgerUnavailable { .. }) => continue,
            Err(other) => panic!("unexpected discard error: {other}"),
        }
    }
    assert_eq!(world.dispatcher.unique_count(), 0);
    assert_eq!(world.notifier.notices().len(), 19);
    let discarded = world
        .ledger
        .list_effects(&world.site, &[EffectState::Discarded])
        .await
        .unwrap();
    assert_eq!(discarded.len(), COUNT - FISCAL);
}

/// Scenario D: the fail-closed edges.
#[tokio::test]
async fn scenario_d_fails_closed() {
    let world = world("scenario-d.example");
    world.service.load(NOW).await;
    world
        .service
        .begin("mig-d", "verify upgrade", NOW, None)
        .await
        .unwrap();

    // A second begin is refused.
    match world.service.begin("mig-d-2", "again", NOW, None).await {
        Err(Error::AlreadyHeld { .. }) => {}
        other => panic!("a second begin was not refused: {other:?}"),
    }

    // A wrong migration id on release is refused.
    match world
        .service
        .release("mig-other", ReleasePlan::new(), NOW)
        .await
    {
        Err(Error::WrongMigration { .. }) => {}
        other => panic!("a wrong migration was not refused: {other:?}"),
    }

    // An unreadable ledger on admit: held, and not recorded (fail closed).
    let gate = Gate::new(
        world.site.clone(),
        Arc::new(UnavailableLedger) as Arc<dyn LedgerStore>,
        Arc::clone(&world.policies),
    );
    let reference = effect(&world.site, EffectClass::Mail, "unavailable", "mail");
    match gate.admit(&reference, NOW).await {
        Admission::Held(ticket) => {
            assert!(
                !ticket.durable,
                "an unreadable ledger must not claim a durable ticket"
            );
            assert_eq!(ticket.migration_id, MigrationId::unverified());
        }
        Admission::Proceed => panic!("an unreadable ledger admitted an effect"),
    }

    // An unknown class is held by default.
    let reference = effect(
        &world.site,
        EffectClass::Other("mystery".to_owned()),
        "unknown",
        "mystery",
    );
    match world.service.gate().admit(&reference, NOW).await {
        Admission::Held(_) => {}
        Admission::Proceed => panic!("an unknown class proceeded while held"),
    }

    // A stale snapshot errs towards held: the same open snapshot proceeds when fresh and is held
    // once it is older than the gate's staleness window.
    let open = common::world("scenario-d-open.example");
    open.service.load(NOW).await;
    let fresh = effect(&open.site, EffectClass::Mail, "fresh", "mail");
    assert_eq!(
        open.service.gate().admit(&fresh, NOW).await,
        Admission::Proceed
    );
    let stale = effect(&open.site, EffectClass::Mail, "stale", "mail");
    assert!(matches!(
        open.service.gate().admit(&stale, NOW + DAY_MICROS).await,
        Admission::Held(_)
    ));
}

/// A policy that tries to hold fiscal transmissions is refused.
#[test]
fn fiscal_can_never_be_configured_holdable() {
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert(
        "fiscal_transmission".to_owned(),
        ClassPolicyConfig {
            hold: Some(true),
            never_hold: false,
            notify_on_discard: false,
            max_hold_seconds: None,
        },
    );
    let config = PolicyConfig {
        overrides,
        never_hold_allow: Vec::new(),
    };
    match PolicyTable::load(config) {
        Err(Error::FiscalCannotBeHeld { .. }) => {}
        other => panic!("fiscal holdability was not refused: {other:?}"),
    }
}

/// A non-fiscal class may not be never-hold without a lead-approved allow-list entry.
#[test]
fn never_hold_requires_allow_list() {
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert(
        "webhook".to_owned(),
        ClassPolicyConfig {
            hold: None,
            never_hold: true,
            notify_on_discard: false,
            max_hold_seconds: None,
        },
    );
    let config = PolicyConfig {
        overrides,
        never_hold_allow: Vec::new(),
    };
    match PolicyTable::load(config) {
        Err(Error::NeverHoldNotAllowed { .. }) => {}
        other => panic!("never_hold without an allow-list entry was not refused: {other:?}"),
    }
}

/// Scenario D (stale bank instruction): an effect older than its class' `max_hold` is not
/// auto-released; it waits for confirmation.
#[tokio::test]
async fn stale_effect_needs_confirmation() {
    let world = world("scenario-d-stale.example");
    world.service.load(NOW).await;
    world
        .service
        .begin("mig-stale", "verify", NOW, None)
        .await
        .unwrap();
    let bank = effect(&world.site, EffectClass::BankApi, "bank-1", "bank_api");
    assert!(matches!(
        world.service.gate().admit(&bank, NOW).await,
        Admission::Held(_)
    ));

    // Release two days later: the bank instruction is stale and needs confirmation.
    let report = world
        .service
        .release("mig-stale", ReleasePlan::new(), NOW + 2 * DAY_MICROS)
        .await
        .unwrap();
    assert!(!report.completed);
    assert_eq!(report.needs_confirmation.len(), 1);
    assert_eq!(world.dispatcher.unique_count(), 0);

    // Confirming the key releases it.
    let report = world
        .service
        .release(
            "mig-stale",
            ReleasePlan::new().confirm("bank-1"),
            NOW + 2 * DAY_MICROS + 1,
        )
        .await
        .unwrap();
    assert!(report.completed);
    assert_eq!(world.dispatcher.unique_count(), 1);
}

/// Scenario E: sixty-four dispatchers race a `begin`. After it returns, no held-class effect is
/// ever admitted to proceed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn scenario_e_begin_drains_in_flight_admits() {
    let world = world("scenario-e.example");
    world.service.load(NOW).await;

    let gate = world.service.gate();
    let begin_done = Arc::new(AtomicBool::new(false));
    let violation = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let proceeds = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for task in 0..64u64 {
        let gate = Arc::clone(&gate);
        let begin_done = Arc::clone(&begin_done);
        let violation = Arc::clone(&violation);
        let stop = Arc::clone(&stop);
        let proceeds = Arc::clone(&proceeds);
        let site = world.site.clone();
        handles.push(tokio::spawn(async move {
            let mut index = 0u64;
            while !stop.load(Ordering::SeqCst) {
                // An admit *initiated* after `begin` returned must never proceed. An admit
                // already in flight when `begin` ran may proceed: `begin` drains those before it
                // returns, which is exactly the guarantee this checks.
                let started_after = begin_done.load(Ordering::SeqCst);
                let reference = EffectRef::new(
                    site.clone(),
                    EffectClass::Mail,
                    "mail.invoice",
                    format!("e{task}-{index}"),
                    NOW,
                )
                .with_source("mail");
                index += 1;
                if let Admission::Proceed = gate.admit(&reference, NOW).await {
                    proceeds.fetch_add(1, Ordering::SeqCst);
                    if started_after {
                        violation.store(true, Ordering::SeqCst);
                    }
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    // Let the dispatchers get going, then begin while they race.
    tokio::time::sleep(Duration::from_millis(25)).await;
    world
        .service
        .begin("mig-e", "verify upgrade", NOW, None)
        .await
        .unwrap();
    begin_done.store(true, Ordering::SeqCst);

    // Give them time to observe the held state, then stop.
    tokio::time::sleep(Duration::from_millis(25)).await;
    stop.store(true, Ordering::SeqCst);
    for handle in handles {
        handle.await.unwrap();
    }

    assert!(
        !violation.load(Ordering::SeqCst),
        "an effect initiated after begin returned was admitted to proceed"
    );

    // And a fresh admit once begin has returned is always held.
    let after = EffectRef::new(
        world.site.clone(),
        EffectClass::Mail,
        "mail.invoice",
        "after-begin",
        NOW,
    )
    .with_source("mail");
    assert!(matches!(gate.admit(&after, NOW).await, Admission::Held(_)));
    assert!(
        proceeds.load(Ordering::SeqCst) > 0,
        "the tasks did not race begin"
    );
    inspect_codes();
}

/// Uses the crate's codes so this file also demonstrates the catalogue wiring.
fn inspect_codes() {
    assert_eq!(codes::ALREADY_HELD, "effect_hold.already_held");
}

/// The audit adapter appends real, hash-chained entries.
#[tokio::test]
async fn audit_chain_records_transitions() {
    use spot_effect_hold::{AuditEvent, AuditKind, AuditSink, ChainAudit};

    let site = spot_core::SiteId::new("audit-chain.example").unwrap();
    let signer = Arc::new(spot_audit_chain::LocalSigner::from_seed([7u8; 32]));
    let audit: ChainAudit<spot_audit_chain::InMemoryStore> = ChainAudit::new(signer);

    audit
        .record(AuditEvent::new(AuditKind::Begin, site.clone(), None, NOW))
        .await
        .unwrap();
    audit
        .record(
            AuditEvent::new(AuditKind::Discard, site.clone(), None, NOW + 1)
                .with_count("discarded", 3),
        )
        .await
        .unwrap();

    let head = audit.head(&site).await.unwrap().expect("a head exists");
    assert_eq!(head.seq, 2);
    assert_eq!(head.record.kind, "hold.discard");
}

/// A policy configuration can be loaded from JSON.
#[test]
fn policy_config_loads_from_json() {
    let config = PolicyConfig::from_json(
        r#"{"overrides":{"mail":{"notify_on_discard":true}},"never_hold_allow":[]}"#,
    )
    .unwrap();
    let table = PolicyTable::load(config).unwrap();
    assert_eq!(
        table.policy_for(&EffectClass::Mail).on_discard,
        spot_effect_hold::OnDiscard::DropAndNotify
    );
}
