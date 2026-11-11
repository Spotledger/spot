//! The lifecycle: closed until activation, refused out of order, quarantined on a leak, and
//! locked off from every outbound destination before the gate opens.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common as fake;

use serde_json::json;
use spot_sandbox::{
    Error, MaskingEngine, MaskingPolicy, MemoryEffects, MemorySnapshots, MemoryStores,
    MemoryTenants, Sandbox, SandboxPlan, SandboxState, SnapshotRef, Step, TenantPort,
};

struct Fixture {
    sandbox: Sandbox,
    store: spot_sandbox::SandboxData,
    classes: spot_sandbox::MetadataClasses,
    policy: MaskingPolicy,
    effects: MemoryEffects,
    snapshots: MemorySnapshots,
    tenants: MemoryTenants,
    stores: MemoryStores,
}

fn fixture() -> Fixture {
    Fixture {
        sandbox: Sandbox::new(fake::sample_plan()).unwrap(),
        store: fake::sample(),
        classes: fake::sample_classes(),
        policy: MaskingPolicy::default(),
        effects: MemoryEffects::new(),
        snapshots: MemorySnapshots::new(),
        tenants: MemoryTenants::new(),
        stores: MemoryStores::new(),
    }
}

impl Fixture {
    /// Publishes the current data as the sandbox tenant's store, then scans it through the
    /// workflow's own handle.
    fn scan(&mut self, at: i64) -> spot_sandbox::Result<spot_sandbox::ScanReport> {
        self.stores.insert("sandbox_acme", self.store.clone());
        self.sandbox.scan(&self.stores, &self.tenants, at)
    }

    fn advance_to_verified(&mut self) {
        self.sandbox.take_snapshot(&self.snapshots, 1_000).unwrap();
        self.sandbox
            .restore(
                &self.snapshots,
                &self.tenants,
                1_700_000_000_000,
                [1u8; 10],
                1_001,
            )
            .unwrap();
        let engine = MaskingEngine::new(&self.classes, &self.policy);
        self.sandbox
            .mask(
                &engine,
                &mut self.store,
                &self.effects,
                &self.tenants,
                fake::sample_key(),
                1_002,
            )
            .unwrap();
        self.scan(1_003).unwrap();
    }
}

#[test]
fn the_happy_path_opens_only_at_activation() {
    let mut fx = fixture();
    assert_eq!(fx.sandbox.state(), SandboxState::Planned);
    assert!(!fx.sandbox.is_reachable());

    fx.sandbox.take_snapshot(&fx.snapshots, 1_000).unwrap();
    assert_eq!(fx.sandbox.state(), SandboxState::SnapshotTaken);

    fx.sandbox
        .restore(
            &fx.snapshots,
            &fx.tenants,
            1_700_000_000_000,
            [1u8; 10],
            1_001,
        )
        .unwrap();
    assert_eq!(fx.sandbox.state(), SandboxState::Restored);
    let tenant = fx.sandbox.tenant().unwrap();
    assert!(!fx.tenants.is_reachable(&tenant));

    let engine = MaskingEngine::new(&fx.classes, &fx.policy);
    fx.sandbox
        .mask(
            &engine,
            &mut fx.store,
            &fx.effects,
            &fx.tenants,
            fake::sample_key(),
            1_002,
        )
        .unwrap();
    assert_eq!(fx.sandbox.state(), SandboxState::Masked);
    assert!(!fx.tenants.is_reachable(&tenant));

    let report = fx.scan(1_003).unwrap();
    assert!(report.is_clean());
    assert_eq!(fx.sandbox.state(), SandboxState::Verified);
    assert!(!fx.tenants.is_reachable(&tenant));

    fx.sandbox.activate(&fx.tenants, 1_004).unwrap();
    assert_eq!(fx.sandbox.state(), SandboxState::Active);
    assert!(fx.sandbox.is_reachable());
    assert!(fx.tenants.is_reachable(&tenant));

    let steps: Vec<Step> = fx
        .sandbox
        .steps()
        .iter()
        .map(|record| record.step)
        .collect();
    assert_eq!(
        steps,
        vec![
            Step::Snapshot,
            Step::Restore,
            Step::Mask,
            Step::Scan,
            Step::Activate
        ]
    );
    assert!(fx.sandbox.steps().iter().all(|record| record.ok));
}

#[test]
fn activation_before_a_scan_is_refused() {
    let mut fx = fixture();
    fx.sandbox.take_snapshot(&fx.snapshots, 1_000).unwrap();
    fx.sandbox
        .restore(
            &fx.snapshots,
            &fx.tenants,
            1_700_000_000_000,
            [1u8; 10],
            1_001,
        )
        .unwrap();
    let engine = MaskingEngine::new(&fx.classes, &fx.policy);
    fx.sandbox
        .mask(
            &engine,
            &mut fx.store,
            &fx.effects,
            &fx.tenants,
            fake::sample_key(),
            1_002,
        )
        .unwrap();

    let tenant = fx.sandbox.tenant().unwrap();
    let error = fx.sandbox.activate(&fx.tenants, 1_003).unwrap_err();
    assert!(matches!(error, Error::State { .. }));
    assert_eq!(error.code(), "sandbox.state");
    assert!(!fx.tenants.is_reachable(&tenant));
}

#[test]
fn a_leak_quarantines_the_sandbox_and_keeps_it_closed() {
    let mut fx = fixture();
    fx.advance_to_verified();
    let tenant = fx.sandbox.tenant().unwrap();

    // Re-seed a copy of the customer's e-mail in an unclassified field, then scan again.
    let invoice = fx
        .store
        .documents
        .iter_mut()
        .find(|doc| doc.id == "inv-1")
        .unwrap();
    invoice
        .fields
        .insert("status".to_owned(), json!(fake::CUSTOMER_EMAIL));

    let report = fx.scan(1_004).unwrap();
    assert!(!report.is_clean());
    assert_eq!(fx.sandbox.state(), SandboxState::Quarantined);

    let error = fx.sandbox.activate(&fx.tenants, 1_005).unwrap_err();
    assert!(matches!(error, Error::Leak { .. }));
    assert_eq!(error.code(), "sandbox.leak");
    assert_eq!(fx.sandbox.state(), SandboxState::Quarantined);
    assert!(!fx.tenants.is_reachable(&tenant));
    assert!(!fx.sandbox.is_reachable());
}

#[test]
fn out_of_order_steps_are_refused() {
    let mut fx = fixture();
    assert!(matches!(
        fx.sandbox
            .restore(
                &fx.snapshots,
                &fx.tenants,
                1_700_000_000_000,
                [1u8; 10],
                1_000
            )
            .unwrap_err(),
        Error::State { .. }
    ));
    assert!(matches!(
        fx.sandbox.scan(&fx.stores, &fx.tenants, 1_001).unwrap_err(),
        Error::State { .. }
    ));
    assert!(matches!(
        fx.sandbox.activate(&fx.tenants, 1_002).unwrap_err(),
        Error::State { .. }
    ));

    fx.sandbox.take_snapshot(&fx.snapshots, 1_003).unwrap();
    assert!(matches!(
        fx.sandbox.take_snapshot(&fx.snapshots, 1_004).unwrap_err(),
        Error::State { .. }
    ));
}

#[test]
fn a_failed_restore_leaves_the_sandbox_closed_and_failed() {
    let mut fx = fixture();
    fx.sandbox.take_snapshot(&fx.snapshots, 1_000).unwrap();
    let snapshots = MemorySnapshots::new().failing_restore("disk full");
    let error = fx
        .sandbox
        .restore(&snapshots, &fx.tenants, 1_700_000_000_000, [1u8; 10], 1_001)
        .unwrap_err();
    assert!(matches!(error, Error::Restore { .. }));
    assert_eq!(fx.sandbox.state(), SandboxState::Failed);
    assert!(!fx.sandbox.is_reachable());
    assert!(fx.sandbox.steps().iter().any(|record| !record.ok));
}

#[test]
fn a_failed_snapshot_leaves_the_sandbox_planned_or_failed() {
    let mut fx = fixture();
    let snapshots = MemorySnapshots::new().failing_take("no wal archive");
    let error = fx.sandbox.take_snapshot(&snapshots, 1_000).unwrap_err();
    assert!(matches!(error, Error::Snapshot { .. }));
    assert!(!fx.sandbox.is_reachable());
}

#[test]
fn the_outbound_lock_denies_every_destination_and_holds_forever() {
    let lock = spot_sandbox::OutboundLock::deny_all("sandbox_acme", "prod_eu");
    assert!(lock.denies_all());
    assert!(lock.holds_forever().is_ok());
    for host in [
        "api.example.com",
        "127.0.0.1",
        "hooks.example.com",
        "bank.example",
    ] {
        assert!(lock.denies(spot_egress::Purpose::Webhook, "https", host, 443));
        assert!(lock.denies(spot_egress::Purpose::Mail, "https", host, 25));
        assert!(lock.denies(spot_egress::Purpose::Connector, "http", host, 80));
    }
    // A tenant the policy does not know is denied by the global default.
    assert!(!lock.egress.is_allowed(
        "other",
        spot_egress::Purpose::Webhook,
        "https",
        "example.com",
        443
    ));
}

#[test]
fn masking_installs_the_lock_and_activation_requires_it() {
    let mut fx = fixture();
    fx.sandbox.take_snapshot(&fx.snapshots, 1_000).unwrap();
    fx.sandbox
        .restore(
            &fx.snapshots,
            &fx.tenants,
            1_700_000_000_000,
            [1u8; 10],
            1_001,
        )
        .unwrap();
    let engine = MaskingEngine::new(&fx.classes, &fx.policy);
    fx.sandbox
        .mask(
            &engine,
            &mut fx.store,
            &fx.effects,
            &fx.tenants,
            fake::sample_key(),
            1_002,
        )
        .unwrap();
    let lock = fx.sandbox.outbound_lock().expect("mask installs the lock");
    assert!(lock.denies_all());
    assert!(lock.holds_forever().is_ok());
    assert_eq!(fx.effects.locks().len(), 1);
    assert_eq!(lock.sandbox_tenant, "sandbox_acme");
    assert_eq!(lock.source_tenant, "prod_eu");

    // A lock that admits a destination is refused.
    let mut weak = lock.clone();
    weak.egress
        .tenants
        .get_mut("sandbox_acme")
        .unwrap()
        .allow
        .push(spot_egress::AllowEntry {
            scheme: None,
            host: "api.example.com".to_owned(),
            ports: None,
            purpose: None,
            allow_private: None,
        });
    assert!(!weak.denies_all());
}

#[test]
fn a_pinned_snapshot_is_adopted_and_a_foreign_one_is_refused() {
    let pinned = SnapshotRef::new("nightly-1", "spot-snapshot", "prod_eu", 900, 42);
    let plan = SandboxPlan::new(
        "prod_eu",
        Some(pinned.clone()),
        MaskingPolicy::default(),
        "sandbox_acme",
    );
    let mut sandbox = Sandbox::new(plan).unwrap();
    let adopted = sandbox
        .take_snapshot(&MemorySnapshots::new(), 1_000)
        .unwrap();
    assert_eq!(adopted, pinned);

    let foreign = SnapshotRef::new("nightly-2", "spot-snapshot", "prod_us", 900, 42);
    let plan = SandboxPlan::new(
        "prod_eu",
        Some(foreign),
        MaskingPolicy::default(),
        "sandbox_acme",
    );
    let error = Sandbox::new(plan).unwrap_err();
    assert!(matches!(error, Error::InvalidPlan { .. }));
}

#[test]
fn a_plan_may_not_clone_into_itself() {
    let plan = SandboxPlan::new("prod_eu", None, MaskingPolicy::default(), "prod_eu");
    let error = Sandbox::new(plan).unwrap_err();
    assert_eq!(error.code(), "sandbox.invalid_plan");

    let plan = SandboxPlan::new("prod_eu", None, MaskingPolicy::default(), "");
    assert!(Sandbox::new(plan).is_err());
}

#[test]
fn the_tenant_is_provisioned_exactly_once_and_closed() {
    let mut fx = fixture();
    fx.sandbox.take_snapshot(&fx.snapshots, 1_000).unwrap();
    let tenant = fx
        .sandbox
        .restore(
            &fx.snapshots,
            &fx.tenants,
            1_700_000_000_000,
            [1u8; 10],
            1_001,
        )
        .unwrap();
    assert_eq!(fx.tenants.names(), vec!["sandbox_acme".to_owned()]);
    let record = fx.tenants.record("sandbox_acme").unwrap();
    assert_eq!(record.id, tenant);
    assert!(!record.reachable);
    assert_eq!(
        fx.snapshots.restores(),
        vec![(
            fx.sandbox.snapshot().unwrap().id.clone(),
            "sandbox_acme".to_owned()
        )]
    );
}
