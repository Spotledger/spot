//! The kill test: abort provisioning at each step by dropping the connection, then resume. The
//! end state and the completed journal equal an uninterrupted run.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{fixture, request, runtime};
use spot_tenancy::{ControlStore, JournalKind, Step, TenantState, codes};

#[test]
fn aborting_at_each_step_and_resuming_ends_like_an_uninterrupted_run() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let reference = f.provision("reference", 1, 1_000);
    let reference_tenant = f.store.get(&reference).unwrap();
    assert_eq!(reference_tenant.state, TenantState::Active);

    for (index, step) in Step::ALL.into_iter().enumerate() {
        let name = format!("kill-{}", step.name().replace('_', "-"));
        let now = 2_000 + index as u64 * 100;
        let plan = f
            .plane()
            .provision(&request(&name, "eu-west", index as u8 + 10), "ops", now)
            .unwrap();

        // Arm the fault for this step. `Activate` is the control store's own transition, so its
        // connection is the one dropped; the five port steps drop a placement connection.
        if step == Step::Activate {
            f.faults.kill_next_update();
        } else {
            f.faults.kill_at(Some(step.name()));
        }

        let error = f.plane().run(&plan.tenant, "ops", now + 1).unwrap_err();
        let expected = if step == Step::Activate {
            codes::STORE_FAILED
        } else {
            codes::STEP_FAILED
        };
        assert_eq!(error.code(), expected, "step {}", step.name());
        assert_eq!(
            f.store.get(&plan.tenant).unwrap().state,
            TenantState::Provisioning,
            "step {} left the tenant active",
            step.name()
        );

        // Resume: every port call is idempotent, so a second run finishes the job.
        let resumed = f.plane().run(&plan.tenant, "ops", now + 2).unwrap();
        assert_eq!(resumed.state, TenantState::Active, "step {}", step.name());
        assert_eq!(resumed.version, reference_tenant.version);
        assert!(f.schema_exists(&plan.tenant));
        assert_eq!(f.seed_rows(&plan.tenant), 1);

        let journal = f.store.journal(&plan.tenant).unwrap();
        assert!(
            !journal
                .iter()
                .any(|entry| entry.kind == JournalKind::Compensated),
            "step {} compensated a resumed run",
            step.name()
        );
        for expected_step in Step::ALL {
            let latest = journal
                .iter()
                .rev()
                .find(|entry| entry.step == expected_step)
                .map(|entry| entry.kind);
            assert_eq!(
                latest,
                Some(JournalKind::Done),
                "step {} is not Done after resuming past {}",
                expected_step.name(),
                step.name()
            );
        }

        // The reference run's shape: Active, version 2, same placement policy outcome.
        let stored = f.store.get(&plan.tenant).unwrap();
        assert_eq!(stored.state, reference_tenant.state);
        assert_eq!(stored.version, reference_tenant.version);
        assert_eq!(stored.retention, None);
        assert_eq!(stored.name_reserved_until, None);
    }
}

#[test]
fn the_journal_records_a_started_line_before_a_killed_step() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let plan = f
        .plane()
        .provision(&request("killed", "eu-west", 1), "ops", 1_000)
        .unwrap();
    f.faults.kill_at(Some(Step::Seed.name()));
    f.plane().run(&plan.tenant, "ops", 1_001).unwrap_err();

    let journal = f.store.journal(&plan.tenant).unwrap();
    // CreateSchema is Done; Seed has a Started line and no Done line.
    assert!(
        journal
            .iter()
            .any(|entry| entry.step == Step::CreateSchema && entry.kind == JournalKind::Done)
    );
    assert_eq!(
        journal
            .iter()
            .filter(|entry| entry.step == Step::Seed)
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![JournalKind::Started]
    );
    // The schema and the seed exist (the effect landed) and the journal says the step started.
    assert!(f.schema_exists(&plan.tenant));
    assert_eq!(f.seed_rows(&plan.tenant), 1);
}
