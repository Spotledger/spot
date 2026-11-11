//! The PostgreSQL `ControlStore`: registry, optimistic versions, unique names, the journal and
//! the state-transition history.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{fixture, request, runtime};
use spot_tenancy::{
    ControlStore, JournalEntry, JournalKind, Placement, Step, Tenant, TenantState, codes,
};

#[test]
fn a_tenant_round_trips_through_the_registry() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    let stored = f.store.get(&plan.tenant).unwrap();
    assert_eq!(stored.name.as_str(), "acme");
    assert_eq!(stored.state, TenantState::Provisioning);
    assert_eq!(stored.version, 1);
    assert_eq!(stored.placement, plan.placement);
    assert_eq!(
        f.store.find_by_name(&stored.name).unwrap(),
        Some(stored.clone())
    );
    assert_eq!(f.store.list().unwrap(), vec![stored.clone()]);
    assert_eq!(
        f.store
            .load()
            .unwrap()
            .get(&(f.cluster.clone(), f.database.clone()))
            .copied(),
        Some(1)
    );
    assert!(f.store.journal(&plan.tenant).unwrap().is_empty());
}

#[test]
fn a_live_name_is_unique() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    f.provision("acme", 1, 1_000);
    let error = f
        .plane()
        .provision(&request("acme", "eu-west", 2), "ops", 1_001)
        .unwrap_err();
    assert_eq!(error.code(), codes::NAME_TAKEN);
}

#[test]
fn updates_are_optimistic_and_immutable_fields_are_refused() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    let stored = f.store.get(&plan.tenant).unwrap();

    let mut next = stored.clone();
    next.state = TenantState::Active;
    let error = f
        .store
        .update(next.clone(), stored.version + 1)
        .unwrap_err();
    assert_eq!(error.code(), codes::VERSION_CONFLICT);

    let updated = f.store.update(next.clone(), stored.version).unwrap();
    assert_eq!(updated.state, TenantState::Active);
    assert_eq!(updated.version, 2);

    let mut renamed = updated.clone();
    renamed.name = spot_tenancy::TenantName::new("other").unwrap();
    let error = f.store.update(renamed, updated.version).unwrap_err();
    assert_eq!(error.code(), codes::INVALID_REQUEST);
}

#[test]
fn the_state_machine_is_checked() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    let stored = f.store.get(&plan.tenant).unwrap();
    let mut next = stored.clone();
    next.state = TenantState::Suspended;
    let error = f.store.update(next, stored.version).unwrap_err();
    assert_eq!(error.code(), codes::INVALID_TRANSITION);
}

#[test]
fn the_journal_is_ordered_and_requires_a_known_tenant() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    f.store
        .journal_append(
            &plan.tenant,
            JournalEntry {
                step: Step::CreateSchema,
                kind: JournalKind::Started,
                at: 1_001,
            },
        )
        .unwrap();
    f.store
        .journal_append(
            &plan.tenant,
            JournalEntry {
                step: Step::CreateSchema,
                kind: JournalKind::Done,
                at: 1_002,
            },
        )
        .unwrap();
    let journal = f.store.journal(&plan.tenant).unwrap();
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].kind, JournalKind::Started);
    assert_eq!(journal[1].kind, JournalKind::Done);

    let unknown = spot_tenancy::TenantId::new_v7(2_000, [9u8; 10]);
    let error = f
        .store
        .journal_append(
            &unknown,
            JournalEntry {
                step: Step::Seed,
                kind: JournalKind::Started,
                at: 2_001,
            },
        )
        .unwrap_err();
    assert_eq!(error.code(), codes::NOT_FOUND);
    assert_eq!(
        f.store.journal(&unknown).unwrap_err().code(),
        codes::NOT_FOUND
    );
}

#[test]
fn a_state_change_is_recorded_in_history() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let plan = f
        .plane()
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    let stored = f.store.get(&plan.tenant).unwrap();
    let mut next = stored.clone();
    next.state = TenantState::Active;
    f.store.update(next, stored.version).unwrap();

    let tenant = plan.tenant;
    let rows = rt.block_on(async {
        let client = f.db.raw_client().await;
        client
            .query(
                "SELECT from_state, to_state, version FROM spot_control.tenant_history \
                 WHERE tenant = $1 ORDER BY seq",
                &[&tenant.as_uuid()],
            )
            .await
            .expect("read the history")
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "provisioning");
    assert_eq!(rows[0].get::<_, String>(1), "active");
    assert_eq!(rows[0].get::<_, i64>(2), 2);
}

#[test]
fn a_deleted_tenants_name_is_reserved_until_the_reservation_ends() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    plane.run(&plan.tenant, "ops", 1_001).unwrap();
    plane.suspend(&plan.tenant, "ops", 1_002).unwrap();
    let archived = plane
        .archive(
            &plan.tenant,
            "ops",
            1_003,
            spot_tenancy::Config::default().min_retention_ms,
        )
        .unwrap();
    let delete_after = archived.retention.as_ref().unwrap().delete_after;
    f.approvals.set(&["alice", "bob"]);
    let deleted = plane.delete(&plan.tenant, "ops", delete_after).unwrap();
    assert_eq!(deleted.state, TenantState::Deleted);
    let reserved_until = deleted.name_reserved_until.unwrap();

    // Before the reservation ends the name is refused.
    let error = f
        .plane()
        .provision(&request("acme", "eu-west", 2), "ops", delete_after + 1)
        .unwrap_err();
    assert_eq!(error.code(), codes::NAME_RESERVED);

    // After it ends the name can be taken again.
    let replacement = f
        .plane()
        .provision(&request("acme", "eu-west", 3), "ops", reserved_until + 1)
        .unwrap();
    let stored = f.store.get(&replacement.tenant).unwrap();
    assert_eq!(stored.name.as_str(), "acme");
    assert_eq!(stored.state, TenantState::Provisioning);
    // The deleted row stays for audit.
    assert_eq!(f.store.list().unwrap().len(), 2);
}

#[test]
fn the_placement_changes_only_while_moving() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    let stored = f.store.get(&plan.tenant).unwrap();
    let mut moved = stored.clone();
    moved.placement = Placement {
        cluster: spot_tenancy::ClusterId::new("c2").unwrap(),
        database: spot_tenancy::DatabaseId::new("db2").unwrap(),
        schema: stored.placement.schema.clone(),
    };
    let error = f.store.update(moved, stored.version).unwrap_err();
    assert_eq!(error.code(), codes::INVALID_PLACEMENT);
}

#[test]
fn two_concurrent_registrations_of_one_name_leave_exactly_one_winner() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    std::thread::scope(|scope| {
        let fixture = &f;
        let mut handles = Vec::new();
        for entropy in [1u8, 2u8] {
            handles.push(scope.spawn(move || {
                fixture
                    .plane()
                    .provision(&request("acme", "eu-west", entropy), "ops", 1_000)
            }));
        }
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        let loser = outcomes.into_iter().find_map(Result::err).unwrap();
        assert_eq!(loser.code(), codes::NAME_TAKEN);
    });
}

#[test]
fn a_tenant_payload_survives_a_second_read() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let id = f.provision("acme", 1, 1_000);
    let first: Tenant = f.store.get(&id).unwrap();
    let second = f.store.get(&id).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.state, TenantState::Active);
}
