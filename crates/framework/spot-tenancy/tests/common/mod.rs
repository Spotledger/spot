//! A simulated executor: records the order of port calls, keeps the resources each tenant owns,
//! and can fail a chosen call before or after its effect lands.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

use spot_tenancy::ports::{
    ApprovalHook, Archiver, AuditEvent, AuditSink, IamPort, LicencePort, PortResult, Ports,
    SchemaPort, SiteSeeder, StepFailure, TelemetryPort,
};
use spot_tenancy::{
    Cluster, ClusterId, Config, ControlPlane, DatabaseId, MemoryStore, ProvisionRequest, Region,
    Tenant, TenantFlags, TenantId,
};

pub const DAY: u64 = 86_400_000;

#[derive(Default)]
pub struct Sim {
    /// `(kind, tenant)` resources that currently exist.
    pub resources: RefCell<BTreeSet<(&'static str, TenantId)>>,
    /// Every apply call in order.
    pub applied: RefCell<Vec<&'static str>>,
    pub undone: RefCell<Vec<&'static str>>,
    /// Apply calls so far.
    pub apply_calls: Cell<usize>,
    pub undo_calls: Cell<usize>,
    /// Fail the apply with this index; `true` = after the effect landed.
    pub fail_apply: Cell<Option<(usize, bool)>>,
    pub fail_undo: Cell<Option<usize>>,
    pub audit: RefCell<Vec<AuditEvent>>,
    pub audit_fails_on: RefCell<Option<&'static str>>,
    pub approvers: RefCell<Vec<String>>,
    pub verify_ok: Cell<bool>,
}

impl Sim {
    pub fn new() -> Self {
        let s = Self::default();
        s.verify_ok.set(true);
        s
    }

    fn apply(&self, kind: &'static str, t: &Tenant) -> PortResult {
        let n = self.apply_calls.get();
        self.apply_calls.set(n + 1);
        let fail = self.fail_apply.get().filter(|(k, _)| *k == n);
        if matches!(fail, Some((_, false))) {
            self.fail_apply.set(None);
            return Err(StepFailure {
                retryable: true,
                reason: "injected failure",
            });
        }
        self.resources.borrow_mut().insert((kind, t.id)); // idempotent: a set
        self.applied.borrow_mut().push(kind);
        if fail.is_some() {
            self.fail_apply.set(None);
            return Err(StepFailure {
                retryable: true,
                reason: "injected failure after effect",
            });
        }
        Ok(())
    }

    fn undo(&self, kind: &'static str, t: &Tenant) -> PortResult {
        let n = self.undo_calls.get();
        self.undo_calls.set(n + 1);
        if self.fail_undo.get() == Some(n) {
            self.fail_undo.set(None);
            return Err(StepFailure {
                retryable: true,
                reason: "injected undo failure",
            });
        }
        self.resources.borrow_mut().remove(&(kind, t.id));
        self.undone.borrow_mut().push(kind);
        Ok(())
    }

    pub fn owned_by(&self, id: &TenantId) -> usize {
        self.resources
            .borrow()
            .iter()
            .filter(|(_, t)| t == id)
            .count()
    }
}

impl SchemaPort for Sim {
    fn create(&self, t: &Tenant) -> PortResult {
        self.apply("schema", t)
    }
    fn drop_schema(&self, t: &Tenant) -> PortResult {
        self.undo("schema", t)
    }
}
impl SiteSeeder for Sim {
    fn seed(&self, t: &Tenant) -> PortResult {
        self.apply("seed", t)
    }
    fn unseed(&self, t: &Tenant) -> PortResult {
        self.undo("seed", t)
    }
}
impl IamPort for Sim {
    fn create_users(&self, t: &Tenant) -> PortResult {
        self.apply("users", t)
    }
    fn remove_users(&self, t: &Tenant) -> PortResult {
        self.undo("users", t)
    }
}
impl LicencePort for Sim {
    fn issue(&self, t: &Tenant) -> PortResult {
        self.apply("licence", t)
    }
    fn revoke(&self, t: &Tenant) -> PortResult {
        self.undo("licence", t)
    }
}
impl TelemetryPort for Sim {
    fn register(&self, t: &Tenant) -> PortResult {
        self.apply("telemetry", t)
    }
    fn deregister(&self, t: &Tenant) -> PortResult {
        self.undo("telemetry", t)
    }
}
impl Archiver for Sim {
    fn final_backup(&self, _t: &Tenant) -> PortResult<String> {
        Ok("backup-1".to_owned())
    }
    fn verify(&self, _t: &Tenant, _r: &str) -> PortResult<bool> {
        Ok(self.verify_ok.get())
    }
}
impl ApprovalHook for Sim {
    fn approvers(&self, _t: &Tenant) -> Vec<String> {
        self.approvers.borrow().clone()
    }
}
impl AuditSink for Sim {
    fn record(&self, e: &AuditEvent) -> PortResult {
        if *self.audit_fails_on.borrow() == Some(e.action) {
            return Err(StepFailure {
                retryable: true,
                reason: "audit store down",
            });
        }
        self.audit.borrow_mut().push(e.clone());
        Ok(())
    }
}

pub fn cluster(cluster: &str, db: &str, region: &str, capacity: u32) -> Cluster {
    Cluster {
        cluster: ClusterId::new(cluster).unwrap(),
        database: DatabaseId::new(db).unwrap(),
        region: Region::new(region).unwrap(),
        capacity,
        accepting: true,
    }
}

pub fn request(name: &str, region: &str, seed: u8) -> ProvisionRequest {
    ProvisionRequest {
        name: name.to_owned(),
        region: region.to_owned(),
        edition: "standard".to_owned(),
        entropy: [seed; 10],
        flags: TenantFlags::NONE,
    }
}

pub struct Fixture {
    pub store: MemoryStore,
    pub sim: Sim,
    pub clusters: Vec<Cluster>,
    pub config: Config,
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            store: MemoryStore::new(),
            sim: Sim::new(),
            clusters: vec![
                cluster("c1", "db1", "eu-west", 10),
                cluster("c2", "db1", "us-east", 10),
            ],
            config: Config::default(),
        }
    }

    pub fn plane(&self) -> ControlPlane<'_> {
        ControlPlane {
            store: &self.store,
            ports: Ports {
                schema: &self.sim,
                seeder: &self.sim,
                iam: &self.sim,
                licence: &self.sim,
                telemetry: &self.sim,
            },
            archiver: &self.sim,
            approvals: &self.sim,
            audit: &self.sim,
            clusters: &self.clusters,
            config: self.config,
        }
    }
}
