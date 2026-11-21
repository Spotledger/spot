//! WP-MIG-08.T3: two nodes over one ledger, no coordinator, both reading the same
//! `Held` effects before either acts. Every transition is a compare-and-set against the state
//! read, so each effect is acted on by exactly one of them: dispatched once, or discarded, never
//! both, and the loser counts `effect_hold.stale` and does nothing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{NOW, World, effect, world};
use futures::future::BoxFuture;
use spot_core::SiteId;
use spot_effect_hold::{
    Dispatcher, EffectClass, EffectId, EffectRecord, EffectState, InMemoryLedger, LedgerStore,
    ReleasePlan, Result, Transition,
};
use tokio::sync::Barrier;

const EFFECTS: usize = 4;

/// Makes both nodes read the effect list before either of them acts, the interleaving the
/// compare-and-set exists for.
struct BothReadFirst {
    inner: Arc<InMemoryLedger>,
    barrier: Barrier,
    waits_left: AtomicUsize,
}

impl LedgerStore for BothReadFirst {
    fn append_effect<'a>(
        &'a self,
        effect: spot_effect_hold::NewEffect,
    ) -> BoxFuture<'a, Result<EffectId>> {
        self.inner.append_effect(effect)
    }

    fn list_effects<'a>(
        &'a self,
        site: &'a SiteId,
        states: &'a [EffectState],
    ) -> BoxFuture<'a, Result<Vec<EffectRecord>>> {
        Box::pin(async move {
            let listed = self.inner.list_effects(site, states).await?;
            let waits = self
                .waits_left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
            if waits.is_ok() {
                self.barrier.wait().await;
            }
            Ok(listed)
        })
    }

    fn transition_effect<'a>(
        &'a self,
        id: EffectId,
        expected: EffectState,
        next: EffectState,
        note: Option<String>,
    ) -> BoxFuture<'a, Result<Transition>> {
        self.inner.transition_effect(id, expected, next, note)
    }

    fn get_hold<'a>(
        &'a self,
        site: &'a SiteId,
    ) -> BoxFuture<'a, Result<spot_effect_hold::HoldState>> {
        self.inner.get_hold(site)
    }

    fn compare_and_set_hold<'a>(
        &'a self,
        site: &'a SiteId,
        expected: &'a spot_effect_hold::HoldState,
        new: spot_effect_hold::HoldState,
    ) -> BoxFuture<'a, Result<bool>> {
        self.inner.compare_and_set_hold(site, expected, new)
    }

    fn checkpoint<'a>(
        &'a self,
        site: &'a SiteId,
        cursor: spot_effect_hold::ReleaseCursor,
    ) -> BoxFuture<'a, Result<()>> {
        self.inner.checkpoint(site, cursor)
    }

    fn get_checkpoint<'a>(
        &'a self,
        site: &'a SiteId,
        migration_id: &'a spot_effect_hold::MigrationId,
    ) -> BoxFuture<'a, Result<Option<spot_effect_hold::ReleaseCursor>>> {
        self.inner.get_checkpoint(site, migration_id)
    }
}

/// Counts every call, with no deduplication: two dispatches of one key show as two.
#[derive(Default)]
struct CountingDispatcher {
    calls: Mutex<HashMap<String, usize>>,
}

impl CountingDispatcher {
    fn count(&self, key: &str) -> usize {
        self.calls.lock().unwrap().get(key).copied().unwrap_or(0)
    }
}

impl Dispatcher for CountingDispatcher {
    fn dispatch<'a>(&'a self, effect: &'a EffectRecord) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *self
                .calls
                .lock()
                .unwrap()
                .entry(effect.key.clone())
                .or_default() += 1;
            // Let the other node run between this effect's claim and its acknowledgement.
            tokio::task::yield_now().await;
            Ok(())
        })
    }
}

struct Nodes {
    world: World,
    a: spot_effect_hold::HoldService,
    b: spot_effect_hold::HoldService,
    dispatcher: Arc<CountingDispatcher>,
    ledger: Arc<BothReadFirst>,
}

fn key(index: usize) -> String {
    format!("k{index}")
}

async fn nodes(name: &str) -> Nodes {
    let world = world(name);
    let ledger = Arc::new(BothReadFirst {
        inner: Arc::clone(&world.ledger),
        barrier: Barrier::new(2),
        waits_left: AtomicUsize::new(0),
    });
    let dispatcher = Arc::new(CountingDispatcher::default());
    let build = || {
        spot_effect_hold::HoldService::new(
            world.site.clone(),
            Arc::clone(&ledger) as Arc<dyn LedgerStore>,
            Arc::clone(&world.policies),
            Arc::clone(&world.audit) as Arc<dyn spot_effect_hold::AuditSink>,
            Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
            Arc::clone(&world.notifier) as Arc<dyn spot_effect_hold::Notifier>,
        )
    };
    let (a, b) = (build(), build());
    a.begin("mig-1", "verify", NOW, None).await.unwrap();
    for index in 0..EFFECTS {
        let admission = a
            .gate()
            .admit(
                &effect(&world.site, EffectClass::Mail, &key(index), "node:a"),
                NOW,
            )
            .await;
        drop(admission);
    }
    Nodes {
        world,
        a,
        b,
        dispatcher,
        ledger,
    }
}

async fn states(nodes: &Nodes) -> Vec<(String, EffectState)> {
    let all = [
        EffectState::Held,
        EffectState::Releasing,
        EffectState::Dispatched,
        EffectState::Discarded,
        EffectState::NeedsConfirmation,
    ];
    nodes
        .world
        .ledger
        .list_effects(&nodes.world.site, &all)
        .await
        .unwrap()
        .into_iter()
        .map(|record| (record.key, record.state))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_releasing_one_hold_dispatch_each_effect_once() {
    let nodes = nodes("two-nodes-release.example").await;
    assert_eq!(states(&nodes).await.len(), EFFECTS, "the effects were held");
    nodes.ledger.waits_left.store(2, Ordering::SeqCst);

    let (first, second) = tokio::join!(
        nodes.a.release("mig-1", ReleasePlan::new(), NOW),
        nodes.b.release("mig-1", ReleasePlan::new(), NOW)
    );
    let (first, second) = (first.unwrap(), second.unwrap());

    for index in 0..EFFECTS {
        assert_eq!(
            nodes.dispatcher.count(&key(index)),
            1,
            "{} went out more than once or never",
            key(index)
        );
    }
    assert_eq!(first.dispatched.len() + second.dispatched.len(), EFFECTS);
    assert_eq!(first.stale.len() + second.stale.len(), EFFECTS);
    assert_eq!(
        nodes.a.stale_count() + nodes.b.stale_count(),
        EFFECTS as u64,
        "each lost transition is counted"
    );
    assert!(
        states(&nodes)
            .await
            .iter()
            .all(|(_, state)| *state == EffectState::Dispatched)
    );
    // The hold stays held until a release finds nothing left to contest, then completes.
    let finished = nodes
        .a
        .release("mig-1", ReleasePlan::new(), NOW)
        .await
        .unwrap();
    assert!(finished.completed && finished.stale.is_empty());
    assert_eq!(nodes.dispatcher.count(&key(0)), 1, "nothing was re-sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_racing_a_discard_leaves_each_effect_with_one_winner() {
    let nodes = nodes("two-nodes-discard.example").await;
    nodes.ledger.waits_left.store(2, Ordering::SeqCst);

    let (released, discarded) = tokio::join!(
        nodes.a.release("mig-1", ReleasePlan::new(), NOW),
        nodes.b.discard("mig-1", "rolled back", NOW)
    );
    // One of them may lose the hold-state compare-and-set with `NotHeld`; neither may panic or
    // act on an effect the other took.
    let released = released.ok();
    let discarded = discarded.ok();

    let final_states = states(&nodes).await;
    let mut sent = 0;
    let mut dropped = 0;
    for (key, state) in &final_states {
        match state {
            EffectState::Dispatched => {
                sent += 1;
                assert_eq!(nodes.dispatcher.count(key), 1, "{key} sent once");
            }
            EffectState::Discarded => {
                dropped += 1;
                assert_eq!(
                    nodes.dispatcher.count(key),
                    0,
                    "{key} was discarded and sent"
                );
            }
            other => panic!("{key} is neither sent nor dropped: {other:?}"),
        }
    }
    assert_eq!(sent + dropped, EFFECTS);
    if let Some(report) = released {
        assert_eq!(report.dispatched.len(), sent);
    }
    if let Some(report) = discarded {
        assert_eq!(usize::try_from(report.discarded).unwrap(), dropped);
    }
}
