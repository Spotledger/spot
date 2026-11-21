//! Restoring an undo record into a second registry: the framework half of a durable undo log.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use serde_json::json;
use spot_remedies::{
    FakeWorld, JobLife, JobOps, JobRef, PlanHash, QuarantineState, Registry, RemedyError, RemedyId,
    RemedySettings, RestoreRefusal, UndoKind, UndoRecord,
};

fn job_world() -> FakeWorld {
    let mut world = FakeWorld::new();
    world.jobs.seed("j1", JobLife::Failed, 3, None);
    world
}

/// Dry-runs and applies `job.retry_failed` on `j1`, returning the undo record the registry holds.
fn apply_retry(registry: &Registry, world: &mut FakeWorld) -> UndoRecord {
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("job.retry_failed");
    let gate = common::gate();
    let plan = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry
            .dry_run(&agent, &id, &json!({"job_id": "j1"}), &gate, &mut ctx)
            .unwrap()
    };
    let applied = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        registry.apply(&agent, &id, &plan, &gate, &mut ctx).unwrap()
    };
    registry.undo_record(&applied.undo_token).unwrap()
}

fn undo(
    registry: &Registry,
    world: &mut FakeWorld,
    id: &str,
    record: &UndoRecord,
    now: i64,
) -> Result<(), RemedyError> {
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let mut ctx = world.ctx("agent-deep", now, &settings);
    registry
        .undo(
            &agent,
            &RemedyId::new(id),
            &record.token,
            &common::gate(),
            &mut ctx,
        )
        .map(|_| ())
}

#[test]
fn a_restored_record_undoes_in_a_second_registry() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let record = apply_retry(&first, &mut world);
    assert_eq!(world.job_life("j1"), Some(JobLife::Queued));

    let stored = first.open_undo_records(common::NOW);
    assert_eq!(stored, vec![record.clone()]);

    // The "restart": a new registry over the same catalogue, the same world.
    let second = Registry::seeds().unwrap();
    assert!(second.undo_record(&record.token).is_none());
    for stored_record in stored {
        second.restore_undo(stored_record, common::NOW).unwrap();
    }
    undo(
        &second,
        &mut world,
        "job.retry_failed",
        &record,
        common::NOW,
    )
    .unwrap();
    assert_eq!(world.job_life("j1"), Some(JobLife::Failed));
    assert!(
        second.undo_record(&record.token).is_none(),
        "the record is consumed by undo"
    );
}

#[test]
fn a_record_past_its_window_is_refused_and_its_token_is_unknown() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let record = apply_retry(&first, &mut world);

    let second = Registry::seeds().unwrap();
    let late = record.expires_at + 1;
    let error = second.restore_undo(record.clone(), late).unwrap_err();
    assert_eq!(
        error,
        RemedyError::UndoRestoreRefused {
            reason: RestoreRefusal::Expired
        }
    );
    assert!(
        !error.sentence().contains(record.token.as_str()),
        "the sentence names no value"
    );
    let error = undo(&second, &mut world, "job.retry_failed", &record, late).unwrap_err();
    assert!(
        matches!(error, RemedyError::UndoUnknown { .. }),
        "got {error}"
    );
    assert!(first.open_undo_records(late).is_empty());
}

#[test]
fn a_record_of_an_unknown_remedy_is_refused() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let mut record = apply_retry(&first, &mut world);
    record.remedy = RemedyId::new("no.such_remedy");

    let second = Registry::seeds().unwrap();
    let error = second.restore_undo(record, common::NOW).unwrap_err();
    assert_eq!(
        error,
        RemedyError::UndoRestoreRefused {
            reason: RestoreRefusal::UnknownRemedy
        }
    );
}

#[test]
fn restoring_twice_is_a_no_op() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let record = apply_retry(&first, &mut world);

    let second = Registry::seeds().unwrap();
    second.restore_undo(record.clone(), common::NOW).unwrap();
    second.restore_undo(record.clone(), common::NOW).unwrap();
    assert_eq!(second.open_undo_records(common::NOW), vec![record]);
}

#[test]
fn a_token_held_for_another_plan_is_refused() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let record = apply_retry(&first, &mut world);

    let second = Registry::seeds().unwrap();
    second.restore_undo(record.clone(), common::NOW).unwrap();
    let mut other = record.clone();
    other.plan_hash = PlanHash::compute(&record.remedy, &json!({"other": true}), &[]);
    let error = second.restore_undo(other, common::NOW).unwrap_err();
    assert_eq!(
        error,
        RemedyError::UndoRestoreRefused {
            reason: RestoreRefusal::TokenInUse
        }
    );
    assert_eq!(second.open_undo_records(common::NOW), vec![record]);
}

#[test]
fn a_reversible_record_without_changes_is_refused() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let mut record = apply_retry(&first, &mut world);
    record.changes.clear();

    let second = Registry::seeds().unwrap();
    let error = second.restore_undo(record, common::NOW).unwrap_err();
    assert_eq!(
        error,
        RemedyError::UndoRestoreRefused {
            reason: RestoreRefusal::NoChanges
        }
    );
}

#[test]
fn a_record_that_widens_the_undo_kind_is_refused() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let mut record = apply_retry(&first, &mut world);
    record.undo = UndoKind::irreversible("tampered");

    let second = Registry::seeds().unwrap();
    let error = second.restore_undo(record, common::NOW).unwrap_err();
    assert_eq!(
        error,
        RemedyError::UndoRestoreRefused {
            reason: RestoreRefusal::UndoKindMismatch
        }
    );
}

#[test]
fn a_window_wider_than_the_registry_grants_is_clamped() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let mut record = apply_retry(&first, &mut world);
    let granted = record.expires_at;
    record.expires_at = granted + 1_000_000;

    let second = Registry::seeds().unwrap();
    second.restore_undo(record, common::NOW).unwrap();
    assert!(second.open_undo_records(granted).len() == 1);
    assert!(
        second.open_undo_records(granted + 1).is_empty(),
        "the restored window never exceeds the registry's own"
    );
}

#[test]
fn a_later_conflicting_change_still_refuses_the_restored_undo() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let record = apply_retry(&first, &mut world);

    let second = Registry::seeds().unwrap();
    second.restore_undo(record.clone(), common::NOW).unwrap();
    world
        .jobs
        .restore(&JobRef::new("j1"), JobLife::Running)
        .unwrap();

    let error = undo(
        &second,
        &mut world,
        "job.retry_failed",
        &record,
        common::NOW,
    )
    .unwrap_err();
    match error {
        RemedyError::UndoConflict { target, .. } => assert_eq!(target, "job:j1"),
        other => panic!("expected an undo conflict, got {other}"),
    }
    assert_eq!(world.job_life("j1"), Some(JobLife::Running));
}

#[test]
fn a_new_apply_after_a_restore_never_reuses_a_restored_token() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let restored = apply_retry(&first, &mut world);

    // A fresh registry restores the record, then applies the same remedy on an identical world
    // at the same instant: the plan hash, the instant and the first sequence number all match.
    let second = Registry::seeds().unwrap();
    second.restore_undo(restored.clone(), common::NOW).unwrap();
    let mut other_world = job_world();
    let fresh = apply_retry(&second, &mut other_world);

    assert_ne!(fresh.token, restored.token);
    let open = second.open_undo_records(common::NOW);
    assert_eq!(open.len(), 2);
    assert!(open.iter().any(|record| record.token == restored.token));
    assert!(open.iter().any(|record| record.token == fresh.token));
    let mut tokens: Vec<_> = open.iter().map(|record| record.token.clone()).collect();
    let sorted = {
        let mut copy = tokens.clone();
        copy.sort();
        copy
    };
    assert_eq!(tokens, sorted, "records come ordered by token");
    tokens.dedup();
    assert_eq!(tokens.len(), 2);
}

#[test]
fn forget_undo_drops_a_record() {
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let record = apply_retry(&first, &mut world);
    assert!(first.forget_undo(&record.token));
    assert!(!first.forget_undo(&record.token));
    assert!(first.open_undo_records(common::NOW).is_empty());
    let error = undo(&first, &mut world, "job.retry_failed", &record, common::NOW).unwrap_err();
    assert!(
        matches!(error, RemedyError::UndoUnknown { .. }),
        "got {error}"
    );
}

#[test]
fn an_irreversible_record_is_listed_and_restored_and_undo_still_refuses() {
    let first = Registry::seeds().unwrap();
    let settings = common::settings();
    let agent = common::deep_agent("agent-deep");
    let id = RemedyId::new("quarantine.retry_all");
    let gate = common::gate();
    let mut world = FakeWorld::new();
    world.quarantine.seed("q1", QuarantineState::Held);
    let plan = {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        first
            .dry_run(&agent, &id, &json!({}), &gate, &mut ctx)
            .unwrap()
    };
    {
        let mut ctx = world.ctx("agent-deep", common::NOW, &settings);
        first.apply(&agent, &id, &plan, &gate, &mut ctx).unwrap();
    }
    let open = first.open_undo_records(common::NOW);
    assert_eq!(open.len(), 1);
    assert!(!open[0].undo.is_reversible());

    let second = Registry::seeds().unwrap();
    for record in open.clone() {
        second.restore_undo(record, common::NOW).unwrap();
    }
    let error = undo(
        &second,
        &mut world,
        "quarantine.retry_all",
        &open[0],
        common::NOW,
    )
    .unwrap_err();
    assert!(
        matches!(error, RemedyError::Irreversible { .. }),
        "got {error}"
    );
}

#[test]
fn the_serde_form_of_every_undo_kind_round_trips() {
    let kinds = [
        UndoKind::reversible("restores the job state"),
        UndoKind::irreversible("the held items are gone"),
    ];
    for kind in kinds {
        let text = serde_json::to_string(&kind).unwrap();
        let back: UndoKind = serde_json::from_str(&text).unwrap();
        assert_eq!(back, kind);
    }

    // A whole record, reversible and irreversible, round trips through JSON values too.
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let record = apply_retry(&first, &mut world);
    for undo_kind in [record.undo.clone(), UndoKind::irreversible("never")] {
        let mut variant = record.clone();
        variant.undo = undo_kind;
        let value = serde_json::to_value(&variant).unwrap();
        let back: UndoRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, variant);
    }
}

#[test]
fn a_custom_window_is_the_ceiling_for_a_restore() {
    let narrow = RemedySettings {
        undo_window_secs: 10,
        ..RemedySettings::default()
    };
    let first = Registry::seeds().unwrap();
    let mut world = job_world();
    let record = apply_retry(&first, &mut world);

    let second = Registry::with_seeds(narrow).unwrap();
    second.restore_undo(record.clone(), common::NOW).unwrap();
    assert_eq!(second.open_undo_records(common::NOW + 10).len(), 1);
    assert!(second.open_undo_records(common::NOW + 11).is_empty());
}
