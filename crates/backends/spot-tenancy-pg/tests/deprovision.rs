//! Deprovisioning: archive, wait out the retention delay, delete with two-person approval. The
//! tenant's schema is gone and the database keeps nothing of it; the other tenant is untouched.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{fixture, request, runtime};
use spot_tenancy::{Config, ControlStore, JournalKind, Step, TenantState, codes};

#[test]
fn deleting_a_tenant_leaves_no_schema_and_no_rows_and_spares_its_neighbour() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let doomed = f.provision("doomed", 1, 1_000);
    let neighbour = f.provision("neighbour", 2, 1_100);
    assert_eq!(f.seed_labels(&neighbour), vec!["neighbour".to_owned()]);

    let plane = f.plane();
    plane.suspend(&doomed, "ops", 2_000).unwrap();
    let retention = Config::default().min_retention_ms;
    let archived = plane.archive(&doomed, "ops", 2_001, retention).unwrap();
    assert_eq!(archived.state, TenantState::Deprovisioning);
    let delete_after = archived.retention.as_ref().unwrap().delete_after;
    assert!(archived.retention.as_ref().unwrap().verified);

    // One approver is not enough.
    f.approvals.set(&["alice"]);
    let refused = plane.delete(&doomed, "ops", delete_after).unwrap_err();
    assert_eq!(refused.code(), codes::APPROVAL_REQUIRED);

    // Two approvers other than the requester are.
    f.approvals.set(&["alice", "bob"]);
    let deleted = plane.delete(&doomed, "ops", delete_after).unwrap();
    assert_eq!(deleted.state, TenantState::Deleted);
    assert!(deleted.name_reserved_until.is_some());

    // The tenant's schema is gone: no tables, no seed, nothing of it left behind.
    assert!(!f.schema_exists(&doomed));
    let leaked = rt.block_on(async {
        let client = f.db.raw_client().await;
        client
            .query_one(
                "SELECT count(*) FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1",
                &[&doomed.schema_name()],
            )
            .await
            .expect("count the objects of a dropped schema")
            .get::<_, i64>(0)
    });
    assert_eq!(leaked, 0, "objects remain in the deleted tenant's schema");

    // The neighbour's schema and row are untouched.
    assert!(f.schema_exists(&neighbour));
    assert_eq!(f.seed_labels(&neighbour), vec!["neighbour".to_owned()]);
    assert_eq!(f.store.get(&neighbour).unwrap().state, TenantState::Active);

    // The registry keeps the deleted record for audit but counts only live tenants.
    assert_eq!(f.store.list().unwrap().len(), 2);
    assert_eq!(
        f.store
            .load()
            .unwrap()
            .get(&(f.cluster.clone(), f.database.clone()))
            .copied(),
        Some(1)
    );

    // Every provisioning step is compensated in the journal, and the record cannot be updated.
    let journal = f.store.journal(&doomed).unwrap();
    for step in Step::ALL {
        let latest = journal
            .iter()
            .rev()
            .find(|entry| entry.step == step)
            .map(|entry| entry.kind);
        assert_eq!(
            latest,
            Some(JournalKind::Compensated),
            "step {} was not compensated",
            step.name()
        );
    }
    let stored = f.store.get(&doomed).unwrap();
    let error = f.store.update(stored.clone(), stored.version).unwrap_err();
    assert_eq!(error.code(), codes::WRONG_STATE);

    // And the name cannot be reused before the reservation ends.
    let error = f
        .plane()
        .provision(&request("doomed", "eu-west", 3), "ops", delete_after + 1)
        .unwrap_err();
    assert_eq!(error.code(), codes::NAME_RESERVED);
}
