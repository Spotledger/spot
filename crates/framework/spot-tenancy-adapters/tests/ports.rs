//! The schema, seed, IAM, telemetry, approval and licence adapters: the port semantics, the
//! fail-closed defaults, and a replay of every step.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use spot_core::ManualClock;
use spot_license::issuing::Issuer;
use spot_license::model::{Entitlements, Limit};
use spot_license::{TrustedKey, TrustedKeys, verify};
use spot_tenancy::ports::{
    ApprovalHook, IamPort, LicencePort, SchemaPort, SiteSeeder, StepFailure, TelemetryPort,
};
use spot_tenancy::{
    ClusterId, DatabaseId, Edition, Placement, Region, Tenant, TenantFlags, TenantId, TenantName,
    TenantState,
};
use spot_tenancy_adapters::{
    AdapterError, ApprovalPolicy, IamAdapter, LicenceIssuer, LicenceStore, LicenceWindow,
    MemoryLicenceStore, MigrationRunner, RoleApprovals, SchemaAdapter, SeedAdapter,
    TelemetryAdapter, TelemetryRegistry, UnconfiguredIam, UnconfiguredTelemetry, UserProvisioning,
};

const KEY_ID: &str = "test-issuer-key";

fn tenant(name: &str, seed: u8, state: TenantState) -> Tenant {
    let id = TenantId::new_v7(1_700_000_000_000 + u64::from(seed), [seed; 10]);
    Tenant {
        id,
        name: TenantName::new(name).unwrap(),
        region: Region::new("eu-west").unwrap(),
        edition: Edition::new("standard").unwrap(),
        state,
        placement: Placement {
            cluster: ClusterId::new("c1").unwrap(),
            database: DatabaseId::new("db1").unwrap(),
            schema: id.schema_name(),
        },
        created: 1_700_000_000_000,
        flags: TenantFlags::NONE,
        version: 1,
        retention: None,
        name_reserved_until: None,
    }
}

// ---- schema and seed -------------------------------------------------------------------

/// A runner that records the calls and the resources it holds, like a set: a replay changes
/// nothing.
#[derive(Default)]
struct FakeRunner {
    resources: Mutex<BTreeSet<&'static str>>,
    calls: Mutex<Vec<&'static str>>,
    fails: Mutex<Option<AdapterError>>,
}

impl FakeRunner {
    fn record(&self, call: &'static str) -> Result<(), AdapterError> {
        self.calls.lock().unwrap().push(call);
        match *self.fails.lock().unwrap() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    fn holds(&self, resource: &str) -> bool {
        self.resources.lock().unwrap().contains(resource)
    }
}

impl MigrationRunner for FakeRunner {
    fn ensure_schema(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        self.record("ensure_schema")?;
        self.resources.lock().unwrap().insert("schema");
        Ok(())
    }

    fn drop_schema(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        self.record("drop_schema")?;
        self.resources.lock().unwrap().clear();
        Ok(())
    }

    fn apply_baseline(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        self.record("apply_baseline")?;
        self.resources.lock().unwrap().insert("baseline");
        Ok(())
    }

    fn seed(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        self.record("seed")?;
        self.resources.lock().unwrap().insert("seed");
        Ok(())
    }

    fn unseed(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        self.record("unseed")?;
        self.resources.lock().unwrap().remove("seed");
        Ok(())
    }
}

#[test]
fn schema_and_seed_adapters_replay_without_doing_the_work_twice() {
    let runner = FakeRunner::default();
    let schema = SchemaAdapter::new(&runner);
    let seeder = SeedAdapter::new(&runner);
    let t = tenant("acme", 1, TenantState::Provisioning);
    assert!(schema.create(&t).is_ok());
    assert!(schema.create(&t).is_ok());
    assert!(seeder.seed(&t).is_ok());
    assert!(seeder.seed(&t).is_ok());
    assert!(runner.holds("schema"));
    assert!(runner.holds("baseline"));
    assert!(runner.holds("seed"));
    assert_eq!(
        runner.calls(),
        vec![
            "ensure_schema",
            "apply_baseline",
            "ensure_schema",
            "apply_baseline",
            "seed",
            "seed"
        ]
    );
    // Compensations replay just as safely.
    assert!(schema.drop_schema(&t).is_ok());
    assert!(schema.drop_schema(&t).is_ok());
    assert!(seeder.unseed(&t).is_ok());
    assert!(seeder.unseed(&t).is_ok());
    assert!(!runner.holds("seed"));
    assert!(!runner.holds("schema"));
}

#[test]
fn drop_schema_refuses_a_tenant_that_is_not_on_its_way_out() {
    let runner = FakeRunner::default();
    let schema = SchemaAdapter::new(&runner);
    for state in [
        TenantState::Active,
        TenantState::Suspended,
        TenantState::Moving,
        TenantState::Deleted,
    ] {
        let t = tenant("acme", 1, state);
        let failure = schema.drop_schema(&t).unwrap_err();
        assert_eq!(failure.reason, AdapterError::Schema.code(), "{state:?}");
        assert!(!failure.retryable);
    }
    assert!(runner.calls().is_empty(), "no runner call was made");
    // Deprovisioning (after the verified archive) may drop.
    let archived = tenant("acme", 1, TenantState::Deprovisioning);
    assert!(schema.drop_schema(&archived).is_ok());
    assert_eq!(runner.calls(), vec!["drop_schema"]);
}

#[test]
fn a_failed_runner_step_is_a_fixed_code_never_free_text() {
    let runner = FakeRunner::default();
    *runner.fails.lock().unwrap() = Some(AdapterError::Seed);
    let seeder = SeedAdapter::new(&runner);
    let failure = seeder
        .seed(&tenant("acme", 1, TenantState::Provisioning))
        .unwrap_err();
    assert_eq!(failure.reason, AdapterError::Seed.code());
    assert!(failure.retryable);
}

// ---- IAM and telemetry ------------------------------------------------------------------

#[derive(Default)]
struct FakeIam {
    users: Mutex<BTreeSet<String>>,
}

impl UserProvisioning for FakeIam {
    fn create_users(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.users.lock().unwrap().insert(tenant.id.to_string());
        Ok(())
    }

    fn remove_users(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.users.lock().unwrap().remove(&tenant.id.to_string());
        Ok(())
    }
}

#[derive(Default)]
struct FakeTelemetry {
    registered: Mutex<BTreeSet<String>>,
}

impl TelemetryRegistry for FakeTelemetry {
    fn register(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.registered
            .lock()
            .unwrap()
            .insert(tenant.id.to_string());
        Ok(())
    }

    fn deregister(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        self.registered
            .lock()
            .unwrap()
            .remove(&tenant.id.to_string());
        Ok(())
    }
}

#[test]
fn iam_and_telemetry_adapters_replay_idempotently() {
    let t = tenant("acme", 1, TenantState::Provisioning);
    let iam = FakeIam::default();
    let port = IamAdapter::new(&iam);
    assert!(port.create_users(&t).is_ok());
    assert!(port.create_users(&t).is_ok());
    assert_eq!(iam.users.lock().unwrap().len(), 1);
    assert!(port.remove_users(&t).is_ok());
    assert!(port.remove_users(&t).is_ok());
    assert!(iam.users.lock().unwrap().is_empty());

    let telemetry = FakeTelemetry::default();
    let port = TelemetryAdapter::new(&telemetry);
    assert!(port.register(&t).is_ok());
    assert!(port.register(&t).is_ok());
    assert_eq!(telemetry.registered.lock().unwrap().len(), 1);
    assert!(port.deregister(&t).is_ok());
    assert!(port.deregister(&t).is_ok());
    assert!(telemetry.registered.lock().unwrap().is_empty());
}

#[test]
fn unconfigured_iam_and_telemetry_fail_closed() {
    let t = tenant("acme", 1, TenantState::Provisioning);
    let iam = IamAdapter::new(UnconfiguredIam);
    for failure in [
        iam.create_users(&t).unwrap_err(),
        iam.remove_users(&t).unwrap_err(),
    ] {
        assert_eq!(failure.reason, AdapterError::NotConfigured.code());
    }
    let telemetry = TelemetryAdapter::new(UnconfiguredTelemetry);
    for failure in [
        telemetry.register(&t).unwrap_err(),
        telemetry.deregister(&t).unwrap_err(),
    ] {
        assert_eq!(failure.reason, AdapterError::NotConfigured.code());
    }
}

// ---- approval ---------------------------------------------------------------------------

struct FakePolicy {
    roles: BTreeMap<String, Vec<String>>,
    members: BTreeMap<(String, String), Vec<String>>,
    fails: bool,
}

impl FakePolicy {
    fn new() -> Self {
        Self {
            roles: BTreeMap::new(),
            members: BTreeMap::new(),
            fails: false,
        }
    }

    fn with(mut self, role: &str, members: &[&str]) -> Self {
        self.roles
            .entry("acme".to_owned())
            .or_default()
            .push(role.to_owned());
        self.members.insert(
            ("acme".to_owned(), role.to_owned()),
            members.iter().map(|m| (*m).to_owned()).collect(),
        );
        self
    }
}

impl ApprovalPolicy for FakePolicy {
    fn approver_roles(&self, tenant: &Tenant) -> Result<Vec<String>, AdapterError> {
        if self.fails {
            return Err(AdapterError::Approval);
        }
        Ok(self
            .roles
            .get(tenant.name.as_str())
            .cloned()
            .unwrap_or_default())
    }

    fn role_members(&self, _tenant: &Tenant, role: &str) -> Result<Vec<String>, AdapterError> {
        if self.fails {
            return Err(AdapterError::Approval);
        }
        Ok(self
            .members
            .get(&("acme".to_owned(), role.to_owned()))
            .cloned()
            .unwrap_or_default())
    }
}

#[test]
fn approvers_come_from_the_tenants_roles_distinct_and_never_anyone() {
    let t = tenant("acme", 1, TenantState::Deprovisioning);
    let policy = FakePolicy::new()
        .with("tenant-owner", &["alice", "bob"])
        .with("platform-admin", &["bob", "carol", "  "]);
    let hook = RoleApprovals::new(policy);
    assert_eq!(hook.approvers(&t), vec!["alice", "bob", "carol"]);
    // Replaying the question does not change the answer.
    assert_eq!(hook.approvers(&t), vec!["alice", "bob", "carol"]);

    // No configured role: nobody approved, not everybody.
    assert!(
        RoleApprovals::new(FakePolicy::new())
            .approvers(&t)
            .is_empty()
    );
    // A role with no members approves nobody.
    assert!(
        RoleApprovals::new(FakePolicy::new().with("tenant-owner", &[]))
            .approvers(&t)
            .is_empty()
    );
    // A policy that cannot answer is a refusal, never an open door.
    let broken = FakePolicy {
        fails: true,
        ..FakePolicy::new()
    };
    assert!(RoleApprovals::new(broken).approvers(&t).is_empty());
}

#[test]
fn a_tenant_with_no_approver_roles_is_refused_even_by_another_tenants_policy() {
    let other = tenant("beta", 2, TenantState::Deprovisioning);
    let policy = FakePolicy::new().with("tenant-owner", &["alice"]);
    assert!(RoleApprovals::new(policy).approvers(&other).is_empty());
}

// ---- licence ----------------------------------------------------------------------------

fn issuer_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
}

fn trusted(key: &ed25519_dalek::SigningKey) -> TrustedKeys {
    TrustedKeys::new(vec![
        TrustedKey::from_bytes(KEY_ID, &key.verifying_key().to_bytes(), 0, None).unwrap(),
    ])
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(1_770_000_000).unwrap(),
    ))
}

fn licence_port(
    key: &ed25519_dalek::SigningKey,
    store: MemoryLicenceStore,
) -> LicenceIssuer<MemoryLicenceStore> {
    LicenceIssuer::new(
        Issuer::new(key.clone(), KEY_ID),
        KEY_ID,
        store,
        clock(),
        LicenceWindow::default(),
        Entitlements::from_lists(Some(Limit::UpTo(25)), vec![], vec![]),
    )
}

struct FailingStore;

impl LicenceStore for FailingStore {
    fn active(
        &self,
        _tenant: &TenantId,
    ) -> Result<Option<spot_license::SignedLicence>, AdapterError> {
        Err(AdapterError::LicenceStore)
    }

    fn save(
        &self,
        _tenant: &TenantId,
        _licence: &spot_license::SignedLicence,
        _serial: u64,
    ) -> Result<(), AdapterError> {
        Err(AdapterError::LicenceStore)
    }

    fn revoke(&self, _tenant: &TenantId) -> Result<(), AdapterError> {
        Err(AdapterError::LicenceStore)
    }

    fn highest_serial(&self, _tenant: &TenantId) -> Result<u64, AdapterError> {
        Err(AdapterError::LicenceStore)
    }
}

#[test]
fn an_issued_licence_is_signed_for_the_tenant_and_reused_on_replay() {
    let key = issuer_key();
    let t = tenant("acme", 1, TenantState::Provisioning);
    let port = licence_port(&key, MemoryLicenceStore::new());
    assert!(port.issue(&t).is_ok());
    let first = port.store().active(&t.id).unwrap().expect("a licence");
    let licence = verify(&first, &trusted(&key)).expect("the envelope verifies");
    assert_eq!(licence.tenant.as_deref(), Some(t.id.to_string().as_str()));
    assert_eq!(licence.edition.as_str(), "standard");
    assert_eq!(licence.serial, 1);
    assert_eq!(licence.issued_at, 1_770_000_000);
    assert_eq!(licence.expires_at, Some(1_770_000_000 + 365 * 86_400));
    assert_eq!(licence.entitlements.users(), Limit::UpTo(25));

    // The step is replayed: the same envelope comes back and nothing is re-signed.
    assert!(port.issue(&t).is_ok());
    assert_eq!(port.store().active(&t.id).unwrap().unwrap(), first);
    assert_eq!(port.store().highest_serial(&t.id).unwrap(), 1);
}

#[test]
fn revocation_is_idempotent_and_reissuing_after_it_bumps_the_serial() {
    let key = issuer_key();
    let t = tenant("acme", 1, TenantState::Provisioning);
    let port = licence_port(&key, MemoryLicenceStore::new());
    assert!(port.issue(&t).is_ok());
    assert!(port.revoke(&t).is_ok());
    assert!(port.revoke(&t).is_ok());
    assert!(port.store().active(&t.id).unwrap().is_none());
    assert!(port.issue(&t).is_ok());
    let renewed = port.store().active(&t.id).unwrap().expect("a licence");
    let licence = verify(&renewed, &trusted(&key)).expect("the envelope verifies");
    assert_eq!(
        licence.serial, 2,
        "a re-issued licence must not replay the revoked serial"
    );
}

#[test]
fn a_broken_licence_store_fails_the_step_with_a_fixed_code() {
    let key = issuer_key();
    let t = tenant("acme", 1, TenantState::Provisioning);
    let port = LicenceIssuer::new(
        Issuer::new(key.clone(), KEY_ID),
        KEY_ID,
        FailingStore,
        clock(),
        LicenceWindow::default(),
        Entitlements::none(),
    );
    let failure: StepFailure = port.issue(&t).unwrap_err();
    assert_eq!(failure.reason, AdapterError::LicenceStore.code());
    assert!(failure.retryable);
    assert_eq!(
        port.revoke(&t).unwrap_err().reason,
        AdapterError::LicenceStore.code()
    );
}
