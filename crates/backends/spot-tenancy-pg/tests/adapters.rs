//! The framework's schema and seed adapters wired over this crate's real executor: the control
//! plane's ports drive `spot-pg-migrate` through the executor, and a replay does nothing.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{fixture, request, runtime};
use spot_tenancy::ports::{Ports, SchemaPort, SiteSeeder};
use spot_tenancy::{Config, ControlPlane, ControlStore, TenantState};
use spot_tenancy_adapters::{SchemaAdapter, SeedAdapter};

#[test]
fn the_adapters_provision_through_the_real_executor_and_replay_safely() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let schema = SchemaAdapter::new(&f.provisioner);
    let seeder = SeedAdapter::new(&f.provisioner);
    let plane = ControlPlane {
        store: &f.store,
        ports: Ports {
            schema: &schema,
            seeder: &seeder,
            // The inert in-crate ports keep the workflow runnable; the real IAM, licence,
            // telemetry, archive, approval and audit adapters are wired by the control plane.
            iam: &f.provisioner,
            licence: &f.provisioner,
            telemetry: &f.provisioner,
        },
        archiver: &f.archiver,
        approvals: &f.approvals,
        audit: &f.audit,
        clusters: &f.clusters,
        config: Config::default(),
    };
    let plan = plane
        .provision(&request("adapter", "eu-west", 5), "ops", 1_000)
        .unwrap();
    let active = plane.run(&plan.tenant, "ops", 1_001).unwrap();
    assert_eq!(active.state, TenantState::Active);
    assert!(f.schema_exists(&plan.tenant));
    assert_eq!(f.seed_rows(&plan.tenant), 1);

    // The workflow re-runs steps after a crash without knowing whether they landed: replaying
    // both adapters against the stored tenant must succeed and change nothing.
    let stored = f.store.get(&plan.tenant).unwrap();
    schema.create(&stored).unwrap();
    seeder.seed(&stored).unwrap();
    assert_eq!(f.seed_rows(&plan.tenant), 1);
}
