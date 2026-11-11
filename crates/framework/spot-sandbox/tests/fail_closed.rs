//! Recorded security findings: the sandbox fails closed. A scan can only read the sandbox
//! tenant's store, a run that fails after the copy started leaves the tenant unusable (and
//! refused on open), and a field with no masking rule stops the run.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::cell::Cell;

use common as fake;

use serde_json::json;
use spot_sandbox::data::Document;
use spot_sandbox::{
    Attachment, AuditRow, CacheEntry, Cleanup, Error, FieldPlan, Integration, JournalRow,
    KeepField, MaskingEngine, MaskingPolicy, MemoryEffects, MemorySnapshots, MemoryStores,
    MemoryTenants, Sandbox, SandboxData, SandboxState, SandboxStore, SearchRow, SecretRow,
    StorePort, TenantPort, TenantStatus,
};
use spot_tenancy::TenantId;

const SANDBOX: &str = "sandbox_acme";

struct Fixture {
    sandbox: Sandbox,
    store: SandboxData,
    classes: spot_sandbox::MetadataClasses,
    policy: MaskingPolicy,
    effects: MemoryEffects,
    snapshots: MemorySnapshots,
    tenants: MemoryTenants,
    stores: MemoryStores,
}

fn fixture_with(tenants: MemoryTenants) -> Fixture {
    Fixture {
        sandbox: Sandbox::new(fake::sample_plan()).unwrap(),
        store: fake::sample(),
        classes: fake::sample_classes(),
        policy: MaskingPolicy::default(),
        effects: MemoryEffects::new(),
        snapshots: MemorySnapshots::new(),
        tenants,
        stores: MemoryStores::new(),
    }
}

impl Fixture {
    fn restored(&mut self) -> TenantId {
        self.sandbox.take_snapshot(&self.snapshots, 1_000).unwrap();
        self.sandbox
            .restore(
                &self.snapshots,
                &self.tenants,
                1_700_000_000_000,
                [1u8; 10],
                1_001,
            )
            .unwrap()
    }

    fn mask(&mut self) -> spot_sandbox::Result<()> {
        let engine = MaskingEngine::new(&self.classes, &self.policy);
        self.sandbox.mask(
            &engine,
            &mut self.store,
            &self.effects,
            &self.tenants,
            fake::sample_key(),
            1_002,
        )
    }

    fn masked(&mut self) -> TenantId {
        let tenant = self.restored();
        self.mask().unwrap();
        tenant
    }
}

/// A store whose journal replace fails: masking dies halfway, after the documents were
/// already rewritten.
struct HalfwayStore(SandboxData);

impl SandboxStore for HalfwayStore {
    fn documents(&self) -> Vec<Document> {
        self.0.documents()
    }
    fn replace_documents(&mut self, documents: Vec<Document>) -> spot_sandbox::Result<()> {
        self.0.replace_documents(documents)
    }
    fn journal(&self) -> Vec<JournalRow> {
        self.0.journal()
    }
    fn replace_journal(&mut self, _rows: Vec<JournalRow>) -> spot_sandbox::Result<()> {
        Err(Error::Store {
            reason: "the disk went away".to_owned(),
        })
    }
    fn audit(&self) -> Vec<AuditRow> {
        self.0.audit()
    }
    fn replace_audit(&mut self, rows: Vec<AuditRow>) -> spot_sandbox::Result<()> {
        self.0.replace_audit(rows)
    }
    fn search(&self) -> Vec<SearchRow> {
        self.0.search()
    }
    fn replace_search(&mut self, rows: Vec<SearchRow>) -> spot_sandbox::Result<()> {
        self.0.replace_search(rows)
    }
    fn attachments(&self) -> Vec<Attachment> {
        self.0.attachments()
    }
    fn replace_attachments(&mut self, rows: Vec<Attachment>) -> spot_sandbox::Result<()> {
        self.0.replace_attachments(rows)
    }
    fn caches(&self) -> Vec<CacheEntry> {
        self.0.caches()
    }
    fn replace_caches(&mut self, rows: Vec<CacheEntry>) -> spot_sandbox::Result<()> {
        self.0.replace_caches(rows)
    }
    fn secrets(&self) -> Vec<SecretRow> {
        self.0.secrets()
    }
    fn replace_secrets(&mut self, rows: Vec<SecretRow>) -> spot_sandbox::Result<()> {
        self.0.replace_secrets(rows)
    }
    fn integrations(&self) -> Vec<Integration> {
        self.0.integrations()
    }
    fn replace_integrations(&mut self, rows: Vec<Integration>) -> spot_sandbox::Result<()> {
        self.0.replace_integrations(rows)
    }
    fn tenant_id(&self) -> String {
        self.0.tenant_id()
    }
}

/// A port that hands out a store of the wrong tenant, whatever it is asked for.
struct Redirect<'a> {
    stores: &'a MemoryStores,
    to: &'a str,
}

impl StorePort for Redirect<'_> {
    fn open(
        &self,
        tenant: &TenantId,
        _name: &str,
    ) -> spot_sandbox::Result<Box<dyn SandboxStore + '_>> {
        self.stores.open(tenant, self.to)
    }
}

/// A port that hands out an unbound store.
struct Unbound(SandboxData);

impl StorePort for Unbound {
    fn open(
        &self,
        _tenant: &TenantId,
        _name: &str,
    ) -> spot_sandbox::Result<Box<dyn SandboxStore + '_>> {
        let mut data = self.0.clone();
        data.tenant = String::new();
        Ok(Box::new(data))
    }
}

/// A store that names the sandbox when asked first and another tenant afterwards: the handle
/// check passes and the report then names the wrong tenant.
struct Shifty {
    data: SandboxData,
    asked: Cell<u32>,
}

impl SandboxStore for Shifty {
    fn documents(&self) -> Vec<Document> {
        self.data.documents()
    }
    fn replace_documents(&mut self, d: Vec<Document>) -> spot_sandbox::Result<()> {
        self.data.replace_documents(d)
    }
    fn journal(&self) -> Vec<JournalRow> {
        self.data.journal()
    }
    fn replace_journal(&mut self, r: Vec<JournalRow>) -> spot_sandbox::Result<()> {
        self.data.replace_journal(r)
    }
    fn audit(&self) -> Vec<AuditRow> {
        self.data.audit()
    }
    fn replace_audit(&mut self, r: Vec<AuditRow>) -> spot_sandbox::Result<()> {
        self.data.replace_audit(r)
    }
    fn search(&self) -> Vec<SearchRow> {
        self.data.search()
    }
    fn replace_search(&mut self, r: Vec<SearchRow>) -> spot_sandbox::Result<()> {
        self.data.replace_search(r)
    }
    fn attachments(&self) -> Vec<Attachment> {
        self.data.attachments()
    }
    fn replace_attachments(&mut self, r: Vec<Attachment>) -> spot_sandbox::Result<()> {
        self.data.replace_attachments(r)
    }
    fn caches(&self) -> Vec<CacheEntry> {
        self.data.caches()
    }
    fn replace_caches(&mut self, r: Vec<CacheEntry>) -> spot_sandbox::Result<()> {
        self.data.replace_caches(r)
    }
    fn secrets(&self) -> Vec<SecretRow> {
        self.data.secrets()
    }
    fn replace_secrets(&mut self, r: Vec<SecretRow>) -> spot_sandbox::Result<()> {
        self.data.replace_secrets(r)
    }
    fn integrations(&self) -> Vec<Integration> {
        self.data.integrations()
    }
    fn replace_integrations(&mut self, r: Vec<Integration>) -> spot_sandbox::Result<()> {
        self.data.replace_integrations(r)
    }
    fn tenant_id(&self) -> String {
        let asked = self.asked.get();
        self.asked.set(asked + 1);
        if asked == 0 {
            SANDBOX.to_owned()
        } else {
            "prod_eu".to_owned()
        }
    }
}

struct ShiftyPort(SandboxData);

impl StorePort for ShiftyPort {
    fn open(
        &self,
        _tenant: &TenantId,
        _name: &str,
    ) -> spot_sandbox::Result<Box<dyn SandboxStore + '_>> {
        Ok(Box::new(Shifty {
            data: self.0.clone(),
            asked: Cell::new(0),
        }))
    }
}

// ---- Recorded security finding: the scan is bound to the sandbox tenant's store ------------------------------

#[test]
fn a_scan_given_the_production_store_is_refused() {
    let mut fx = fixture_with(MemoryTenants::new());
    fx.masked();
    // The sandbox's own store holds the masked data; production's holds the clear original.
    fx.stores.insert(SANDBOX, fx.store.clone());
    fx.stores.insert("prod_eu", fake::sample());
    let redirect = Redirect {
        stores: &fx.stores,
        to: "prod_eu",
    };
    let error = fx.sandbox.scan(&redirect, &fx.tenants, 1_003).unwrap_err();
    assert_eq!(error.code(), "sandbox.tenant");
    assert!(fx.sandbox.last_scan().is_none());
    assert_eq!(fx.sandbox.state(), SandboxState::Masked);
    // No clean scan exists, so the tenant cannot be activated.
    let error = fx.sandbox.activate(&fx.tenants, 1_004).unwrap_err();
    assert_eq!(error.code(), "sandbox.state");
    assert!(!fx.sandbox.is_reachable());
}

#[test]
fn a_scan_given_an_unbound_store_is_refused() {
    let mut fx = fixture_with(MemoryTenants::new());
    fx.masked();
    let unbound = Unbound(fx.store.clone());
    let error = fx.sandbox.scan(&unbound, &fx.tenants, 1_003).unwrap_err();
    assert_eq!(error.code(), "sandbox.tenant");
    assert!(fx.sandbox.last_scan().is_none());
    assert!(!fx.sandbox.is_reachable());
}

#[test]
fn a_report_that_names_another_tenant_is_refused_and_the_tenant_is_made_unusable() {
    let mut fx = fixture_with(MemoryTenants::new());
    let tenant = fx.masked();
    let port = ShiftyPort(fx.store.clone());
    let error = fx.sandbox.scan(&port, &fx.tenants, 1_003).unwrap_err();
    assert_eq!(error.code(), "sandbox.tenant");
    assert_eq!(fx.sandbox.state(), SandboxState::Failed);
    assert!(fx.sandbox.last_scan().is_none());
    assert_eq!(fx.tenants.status(&tenant), Some(TenantStatus::Destroyed));
    assert!(!fx.tenants.is_reachable(&tenant));
}

#[test]
fn masking_a_store_of_another_tenant_is_refused_before_it_is_touched() {
    let mut fx = fixture_with(MemoryTenants::new());
    fx.restored();
    fx.store.tenant = "prod_eu".to_owned();
    let before = fx.store.clone();
    let error = fx.mask().unwrap_err();
    assert_eq!(error.code(), "sandbox.tenant");
    assert_eq!(fx.store, before);
    assert_eq!(fx.sandbox.state(), SandboxState::Restored);

    fx.store.tenant = String::new();
    let mut unbound = before;
    unbound.tenant = String::new();
    assert_eq!(fx.mask().unwrap_err().code(), "sandbox.tenant");
    assert_eq!(fx.store, unbound);
}

// ---- Recorded security finding: a failed run leaves the tenant unusable --------------------------------------

#[test]
fn a_mask_that_fails_halfway_leaves_the_tenant_unusable_and_open_is_refused() {
    let mut fx = fixture_with(MemoryTenants::new().failing_destroy());
    let tenant = fx.restored();
    let mut store = HalfwayStore(fx.store.clone());
    let engine = MaskingEngine::new(&fx.classes, &fx.policy);
    let error = fx
        .sandbox
        .mask(
            &engine,
            &mut store,
            &fx.effects,
            &fx.tenants,
            fake::sample_key(),
            1_002,
        )
        .unwrap_err();
    assert_eq!(error.code(), "sandbox.store");
    // The documents were already rewritten: the copy is partly masked.
    assert_ne!(store.0.documents, fx.store.documents);
    assert_eq!(fx.sandbox.state(), SandboxState::Failed);

    // Marked unusable first; the destroy failed, so it stays unusable and visible.
    assert_eq!(fx.tenants.status(&tenant), Some(TenantStatus::Unusable));
    assert_eq!(fx.sandbox.cleanup(), Some(Cleanup::LeftUnusable));
    assert_eq!(
        fx.tenants.record(SANDBOX).unwrap().status,
        TenantStatus::Unusable
    );

    // Opening it is refused by the control plane and by the workflow.
    assert!(fx.tenants.set_reachable(&tenant, true).is_err());
    assert!(fx.tenants.mark_ready(&tenant).is_err());
    assert_eq!(
        fx.sandbox.activate(&fx.tenants, 1_003).unwrap_err().code(),
        "sandbox.state"
    );
    assert!(!fx.tenants.is_reachable(&tenant));
    assert!(!fx.sandbox.is_reachable());
}

#[test]
fn a_failed_mask_destroys_the_tenant_when_it_can() {
    let mut fx = fixture_with(MemoryTenants::new());
    let tenant = fx.restored();
    let mut store = HalfwayStore(fx.store.clone());
    let engine = MaskingEngine::new(&fx.classes, &fx.policy);
    fx.sandbox
        .mask(
            &engine,
            &mut store,
            &fx.effects,
            &fx.tenants,
            fake::sample_key(),
            1_002,
        )
        .unwrap_err();
    assert_eq!(fx.tenants.status(&tenant), Some(TenantStatus::Destroyed));
    assert_eq!(fx.sandbox.cleanup(), Some(Cleanup::Destroyed));
    assert!(fx.tenants.set_reachable(&tenant, true).is_err());
}

#[test]
fn a_tenant_that_cannot_be_marked_unusable_is_reported_not_swallowed() {
    let mut fx = fixture_with(
        MemoryTenants::new()
            .failing_mark_unusable()
            .failing_destroy(),
    );
    let tenant = fx.restored();
    let mut store = HalfwayStore(fx.store.clone());
    let engine = MaskingEngine::new(&fx.classes, &fx.policy);
    let error = fx
        .sandbox
        .mask(
            &engine,
            &mut store,
            &fx.effects,
            &fx.tenants,
            fake::sample_key(),
            1_002,
        )
        .unwrap_err();
    assert_eq!(error.code(), "sandbox.tenant");
    assert_eq!(fx.sandbox.cleanup(), Some(Cleanup::NotMarked));
    assert_eq!(fx.sandbox.state(), SandboxState::Failed);
    assert!(!fx.tenants.is_reachable(&tenant));
}

#[test]
fn a_failed_restore_marks_the_tenant_unusable() {
    let mut fx = fixture_with(MemoryTenants::new().failing_destroy());
    fx.sandbox.take_snapshot(&fx.snapshots, 1_000).unwrap();
    let snapshots = MemorySnapshots::new().failing_restore("disk full");
    let error = fx
        .sandbox
        .restore(&snapshots, &fx.tenants, 1_700_000_000_000, [1u8; 10], 1_001)
        .unwrap_err();
    assert_eq!(error.code(), "sandbox.restore");
    let tenant = fx.sandbox.tenant().unwrap();
    assert_eq!(fx.tenants.status(&tenant), Some(TenantStatus::Unusable));
    assert!(fx.tenants.set_reachable(&tenant, true).is_err());
}

#[test]
fn a_failing_outbound_lock_marks_the_tenant_unusable() {
    struct NoLock;
    impl spot_sandbox::EffectPort for NoLock {
        fn lock(&self, _: &str, _: &str) -> spot_sandbox::Result<spot_sandbox::OutboundLock> {
            Err(Error::Effects {
                reason: "no lock".to_owned(),
            })
        }
    }
    let mut fx = fixture_with(MemoryTenants::new().failing_destroy());
    let tenant = fx.restored();
    let engine = MaskingEngine::new(&fx.classes, &fx.policy);
    let error = fx
        .sandbox
        .mask(
            &engine,
            &mut fx.store,
            &NoLock,
            &fx.tenants,
            fake::sample_key(),
            1_002,
        )
        .unwrap_err();
    assert_eq!(error.code(), "sandbox.effects");
    assert_eq!(fx.tenants.status(&tenant), Some(TenantStatus::Unusable));
}

#[test]
fn a_leaky_scan_marks_the_tenant_unusable() {
    let mut fx = fixture_with(MemoryTenants::new().failing_destroy());
    let tenant = fx.masked();
    let invoice = fx
        .store
        .documents
        .iter_mut()
        .find(|doc| doc.id == "c-1")
        .unwrap();
    invoice
        .fields
        .insert("city".to_owned(), json!(fake::CUSTOMER_EMAIL));
    fx.stores.insert(SANDBOX, fx.store.clone());
    let report = fx.sandbox.scan(&fx.stores, &fx.tenants, 1_003).unwrap();
    assert!(!report.is_clean());
    assert_eq!(report.tenant, SANDBOX);
    assert_eq!(fx.sandbox.state(), SandboxState::Quarantined);
    assert_eq!(fx.tenants.status(&tenant), Some(TenantStatus::Unusable));
    assert!(fx.tenants.set_reachable(&tenant, true).is_err());
    assert!(fx.sandbox.activate(&fx.tenants, 1_004).is_err());
    // A quarantined sandbox is not scanned again.
    assert_eq!(
        fx.sandbox
            .scan(&fx.stores, &fx.tenants, 1_005)
            .unwrap_err()
            .code(),
        "sandbox.state"
    );
}

#[test]
fn a_tenant_marked_unusable_never_becomes_ready() {
    let mut fx = fixture_with(MemoryTenants::new());
    let tenant = fx.masked();
    fx.tenants.mark_unusable(&tenant).unwrap();
    fx.stores.insert(SANDBOX, fx.store.clone());
    fx.sandbox.scan(&fx.stores, &fx.tenants, 1_003).unwrap();
    // The scan was clean, but the registry says unusable: activation is refused.
    let error = fx.sandbox.activate(&fx.tenants, 1_004).unwrap_err();
    assert_eq!(error.code(), "sandbox.tenant");
    assert!(!fx.tenants.is_reachable(&tenant));
    assert_eq!(fx.tenants.status(&tenant), Some(TenantStatus::Unusable));
}

/// A control plane that enforces nothing: the workflow's own status checks must still hold.
struct Lax(std::cell::RefCell<TenantStatus>, Cell<bool>);

impl TenantPort for Lax {
    fn provision(&self, _: &str, now_ms: u64, entropy: [u8; 10]) -> spot_sandbox::Result<TenantId> {
        *self.0.borrow_mut() = TenantStatus::Provisioning;
        Ok(TenantId::new_v7(now_ms, entropy))
    }
    fn set_reachable(&self, _: &TenantId, reachable: bool) -> spot_sandbox::Result<()> {
        self.1.set(reachable);
        Ok(())
    }
    fn is_reachable(&self, _: &TenantId) -> bool {
        self.1.get()
    }
    fn status(&self, _: &TenantId) -> Option<TenantStatus> {
        Some(*self.0.borrow())
    }
    fn mark_unusable(&self, _: &TenantId) -> spot_sandbox::Result<()> {
        *self.0.borrow_mut() = TenantStatus::Unusable;
        Ok(())
    }
    fn mark_ready(&self, _: &TenantId) -> spot_sandbox::Result<()> {
        *self.0.borrow_mut() = TenantStatus::Ready;
        Ok(())
    }
    fn destroy(&self, _: &TenantId) -> spot_sandbox::Result<()> {
        Ok(())
    }
}

#[test]
fn the_workflow_refuses_an_unusable_tenant_even_when_the_port_would_not() {
    let lax = Lax(
        std::cell::RefCell::new(TenantStatus::Provisioning),
        Cell::new(false),
    );
    let mut fx = fixture_with(MemoryTenants::new());
    fx.sandbox.take_snapshot(&fx.snapshots, 1_000).unwrap();
    let tenant = fx
        .sandbox
        .restore(&fx.snapshots, &lax, 1_700_000_000_000, [1u8; 10], 1_001)
        .unwrap();
    let engine = MaskingEngine::new(&fx.classes, &fx.policy);
    fx.sandbox
        .mask(
            &engine,
            &mut fx.store,
            &fx.effects,
            &lax,
            fake::sample_key(),
            1_002,
        )
        .unwrap();
    fx.stores.insert(SANDBOX, fx.store.clone());
    fx.sandbox.scan(&fx.stores, &lax, 1_003).unwrap();
    lax.mark_unusable(&tenant).unwrap();
    let error = fx.sandbox.activate(&lax, 1_004).unwrap_err();
    assert_eq!(error.code(), "sandbox.tenant");
    assert!(!lax.is_reachable(&tenant));
    assert_eq!(lax.status(&tenant), Some(TenantStatus::Unusable));
}

#[test]
fn the_control_plane_will_not_open_or_ready_a_tenant_out_of_order() {
    let tenants = MemoryTenants::new();
    let tenant = tenants
        .provision(SANDBOX, 1_700_000_000_000, [3u8; 10])
        .unwrap();
    // Not ready yet: cannot be opened.
    assert!(tenants.set_reachable(&tenant, true).is_err());
    // Destroy only follows mark_unusable.
    assert!(tenants.destroy(&tenant).is_err());
    tenants.mark_ready(&tenant).unwrap();
    // Ready cannot be promoted twice, and is not destroyed unmarked.
    assert!(tenants.mark_ready(&tenant).is_err());
    tenants.set_reachable(&tenant, true).unwrap();
    tenants.mark_unusable(&tenant).unwrap();
    assert!(!tenants.is_reachable(&tenant));
    assert_eq!(tenants.status(&tenant), Some(TenantStatus::Unusable));
    // A failed destroy keeps it unusable; a successful one is the only way to Destroyed.
    let stuck = MemoryTenants::new().failing_destroy();
    let id = stuck
        .provision(SANDBOX, 1_700_000_000_000, [4u8; 10])
        .unwrap();
    stuck.mark_unusable(&id).unwrap();
    assert!(stuck.destroy(&id).is_err());
    assert_eq!(stuck.status(&id), Some(TenantStatus::Unusable));
    assert!(tenants.destroy(&tenant).is_ok());
    assert_eq!(tenants.status(&tenant), Some(TenantStatus::Destroyed));
    // A destroyed tenant is never resurrected by a later mark_unusable.
    tenants.mark_unusable(&tenant).unwrap();
    assert_eq!(tenants.status(&tenant), Some(TenantStatus::Destroyed));
}

#[test]
fn the_happy_path_still_opens_with_the_workflows_own_handle() {
    let mut fx = fixture_with(MemoryTenants::new());
    let tenant = fx.masked();
    fx.stores.insert(SANDBOX, fx.store.clone());
    let report = fx.sandbox.scan(&fx.stores, &fx.tenants, 1_003).unwrap();
    assert!(report.is_clean());
    assert_eq!(report.tenant, SANDBOX);
    fx.sandbox.activate(&fx.tenants, 1_004).unwrap();
    assert_eq!(fx.tenants.status(&tenant), Some(TenantStatus::Ready));
    assert!(fx.tenants.is_reachable(&tenant));
    assert!(fx.sandbox.cleanup().is_none());
}

// ---- Recorded security finding: the plan lists every field and refuses to run without a rule -----------------

#[test]
fn the_field_plan_lists_every_field_with_its_class() {
    let classes = fake::sample_classes();
    let policy = MaskingPolicy {
        keep: vec![KeepField::new("customer", "city")],
        ..MaskingPolicy::default()
    };
    let plan = FieldPlan::build(&classes, &policy).unwrap();
    assert_eq!(plan.fields.len(), classes.len());
    let city = plan.get("customer", "city").unwrap();
    assert!(city.kept);
    assert_eq!(city.kind, spot_sandbox::MaskKind::Unprotected);
    let number = plan.get("sales_invoice", "number").unwrap();
    assert!(!number.kept);
    assert_eq!(number.kind, spot_sandbox::MaskKind::Identifier);
    let total = plan.get("sales_invoice", "total").unwrap();
    assert_eq!(total.kind, spot_sandbox::MaskKind::AmountKept);
}

#[test]
fn the_workflow_records_the_field_plan() {
    let mut fx = fixture_with(MemoryTenants::new());
    fx.masked();
    let plan = fx.sandbox.field_plan().unwrap();
    assert_eq!(plan.fields.len(), fx.classes.len());
}

#[test]
fn a_classification_with_no_masking_rule_stops_the_run() {
    // A field that carries only a tag the policy does not close has no rule: the plan
    // refuses rather than leave it open by omission.
    let mut classes = fake::sample_definitions();
    let mut spec = spot_meta::FieldSpec::new("internal_ref", spot_meta::FieldType::Text);
    spec.classification = vec!["internal".to_owned()];
    classes[0].fields.push(spec);
    let classes = spot_sandbox::MetadataClasses::from_definitions(&classes);

    let policy = MaskingPolicy::default();
    let error = FieldPlan::build(&classes, &policy).unwrap_err();
    assert_eq!(error.code(), "sandbox.policy");
    assert!(error.to_string().contains("customer.internal_ref"));

    let mut fx = fixture_with(MemoryTenants::new());
    fx.classes = classes.clone();
    fx.restored();
    let before = fx.store.clone();
    let error = fx.mask().unwrap_err();
    assert_eq!(error.code(), "sandbox.policy");
    assert_eq!(fx.store, before, "nothing was written");
    assert_eq!(fx.sandbox.state(), SandboxState::Failed);

    // An explicit Keep decision is the way to say it stays.
    let keeping = MaskingPolicy {
        keep: vec![KeepField::new("customer", "internal_ref")],
        ..MaskingPolicy::default()
    };
    assert!(FieldPlan::build(&classes, &keeping).is_ok());
}

#[test]
fn a_keep_decision_must_name_a_known_field_and_never_a_secret() {
    let classes = fake::sample_classes();
    let unknown = MaskingPolicy {
        keep: vec![KeepField::new("customer", "no_such_field")],
        ..MaskingPolicy::default()
    };
    assert_eq!(
        FieldPlan::build(&classes, &unknown).unwrap_err().code(),
        "sandbox.policy"
    );
    let secret = MaskingPolicy {
        keep: vec![KeepField::new("customer", "api_key")],
        ..MaskingPolicy::default()
    };
    assert_eq!(
        FieldPlan::build(&classes, &secret).unwrap_err().code(),
        "sandbox.policy"
    );
    let duplicate = MaskingPolicy {
        keep: vec![KeepField::new("a", "b"), KeepField::new("a", "b")],
        ..MaskingPolicy::default()
    };
    assert!(duplicate.validate().is_err());
}

#[test]
fn keep_decisions_are_part_of_the_plan_and_load_from_toml() {
    let policy = MaskingPolicy::from_toml(
        r#"
        keep = [{ doctype = "customer", field = "city" }]
        "#,
    )
    .unwrap();
    assert!(policy.keeps("customer", "city"));
    assert!(!policy.keeps("customer", "name"));
    let plan = spot_sandbox::SandboxPlan::new("prod_eu", None, policy, SANDBOX);
    let back: spot_sandbox::SandboxPlan =
        serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
    assert_eq!(back, plan);
}
