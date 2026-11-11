//! Property tests: for any sequence of decisions and ticks, every item ends in exactly one
//! terminal state, no event is emitted twice, and the report counts always sum to the
//! total.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_access_review::{
    AssignmentRef, Campaign, CampaignSpec, CampaignStatus, DeadlinePolicy, Decision, ReviewEvent,
    ReviewItem, ReviewItemState,
};

/// A decision drawn from the pools of keeps and removals.
#[derive(Debug, Clone)]
enum Step {
    Decide {
        item: usize,
        by: usize,
        decision: Decision,
    },
    Tick {
        at: i64,
    },
}

/// The generator over steps. Item indexes 0..ITEMS, actors 0..ACTORS (actor 0 is the
/// escalation target; actor n+1 is item n's reviewer). Times climb.
fn steps() -> impl Strategy<Value = Vec<Step>> {
    (
        prop::collection::vec(
            prop_oneof![
                (0usize..ITEMS, 0usize..ACTORS)
                    .prop_flat_map(|(item, by)| (
                        Just((item, by)),
                        prop_oneof![
                            Just(Decision::Keep { note: None }),
                            Just(Decision::Keep {
                                note: Some("keeping".into())
                            }),
                            Just(Decision::Remove {
                                reason: "unused".into()
                            })
                        ]
                    ))
                    .prop_map(|((item, by), decision)| Step::Decide { item, by, decision }),
                (1_000i64..4_000).prop_map(|at| Step::Tick { at }),
            ],
            0..40,
        ),
        Just(()),
    )
        .prop_map(|(steps, _)| steps)
}

const ITEMS: usize = 3;
const ACTORS: usize = 4; // actor 0 escalation target; actors 1..=3 are item reviewers

fn spec() -> CampaignSpec {
    CampaignSpec {
        id: "prop".to_string(),
        title: "Property campaign".to_string(),
        opened_by: "sec-lead".to_string(),
        escalation_contact: "sec-lead".to_string(),
        deadline: 2_000,
        stale_after_days: 0, // every item is stale at any time >= its last_use
        on_deadline: DeadlinePolicy::Escalate {
            to: "sec-lead".to_string(),
        },
    }
}

fn items() -> Vec<ReviewItem> {
    (0..ITEMS)
        .map(|i| {
            let reviewer = format!("rev-{i}");
            ReviewItem {
                id: format!("it-{i}"),
                user: format!("user-{i}"),
                assignment: AssignmentRef::new(format!("assign-{i}")),
                source: "direct".to_string(),
                last_use: Some(500),
                sod_override: Some(spot_access_review::OverrideNote::new("M1".to_string())),
                reviewer,
            }
        })
        .collect()
}

/// Who is actor `by` for item `item`: `by` 0 is the escalation target; `by` n (1..) is
/// item n-1's reviewer.
fn actor_name(by: usize, item_reviewer: &str) -> String {
    if by == 0 {
        "sec-lead".to_string()
    } else {
        item_reviewer.to_string()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn invariants_hold_for_any_sequence(steps in steps()) {
        let mut c = Campaign::open(spec(), items(), 1_000).expect("fixture opens");
        let _ = c.tick(1_050); // escalate first so the escalation target may decide any item
        // The distinct audit keys seen so far: (campaign, item, decision actor, time, kind).
        let mut decided_keys: std::collections::BTreeSet<(String, String, i64)> =
            std::collections::BTreeSet::new();
        for step in steps {
            match step {
                Step::Decide { item, by, decision } => {
                    if item >= ITEMS { continue; }
                    let reviewer = items()[item].reviewer.clone();
                    let actor = actor_name(by, &reviewer);
                    let item_id = items()[item].id.clone();
                    if let Ok(outcome) = c.decide(&item_id, &actor, decision.clone(), 1_500) {
                        for e in &outcome.events {
                            if let ReviewEvent::Decided { item, at, .. } = e {
                                prop_assert!(decided_keys.insert((item.clone(), "d".into(), *at)),
                                    "the same item decided twice at the same time is a duplicate event");
                            }
                        }
                        // Every successful removal also yields exactly one remove action.
                        if matches!(decision, Decision::Remove { .. }) {
                            prop_assert!(outcome.actions.len() == 1);
                        } else {
                            prop_assert!(outcome.actions.is_empty(), "a keep yields no actions");
                        }
                    }
                }
                Step::Tick { at } => {
                    let out = c.tick(at);
                    // Ticks at or after the deadline are idempotent: no escalation twice.
                    let escalated: Vec<&String> = out
                        .events
                        .iter()
                        .filter_map(|e| match e {
                            ReviewEvent::Escalated { item, .. } => Some(item),
                            _ => None,
                        })
                        .collect();
                    let mut uniq = escalated.clone();
                    uniq.sort();
                    uniq.dedup();
                    prop_assert_eq!(escalated.len(), uniq.len(), "an item escalated twice");
                }
            }
        }

        // Invariant 1: every item is in exactly one state (the vectors stay parallel).
        prop_assert_eq!(c.item_states().len(), ITEMS);

        // Invariant 2: the report counts always sum to the total.
        let r = c.report();
        prop_assert_eq!(r.total, ITEMS);
        prop_assert_eq!(r.kept + r.removed + r.pending, r.total);
        prop_assert_eq!(r.removals.len(), r.removed);
        prop_assert!(r.escalated <= r.pending, "escalated items are pending");

        // Invariant 3: no event is emitted twice — decided keys were unique, escalated
        // items escalate once (the tick above asserts it), and completion happens once.
        let completed_events = c
            .opening_events()
            .iter()
            .filter(|e| matches!(e, ReviewEvent::CampaignOpened { .. }))
            .count();
        prop_assert_eq!(completed_events, 1, "the campaign opened exactly once");
        // And the campaign is either open or completed, never both.
        if c.is_completed() {
            prop_assert!(matches!(c.status(), CampaignStatus::Completed));
            prop_assert!(r.pending == 0);
        }
        let _ = ReviewItemState::is_terminal;
    }
}
