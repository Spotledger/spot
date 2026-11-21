//! Shared doubles for the hub end-to-end tests (WP-SYN-02.T10): the registry, references, image
//! source, node report, the policy book, the destination builder and a ledger shared by relays.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Value, json};
use spot_conditions::MemorySink;
use spot_sync_acks::{AckError, AckLedger, MemoryStore, NodeReportRow, NodeStateStore, StreamHead};
use spot_sync_closure::{ClosureError, ImageSource, RecordKey, Redactor, References, SourceImage};
use spot_sync_hub::{
    AckBook, Destination, EntryRole, HubRelay, MemoryQueue, MemoryStream, QueuedBundle,
    RelayConfig, StreamEntry,
};
use spot_sync_hub_adapters::{MasterScope, PolicyBook, PolicyRedactor, ScopeKeysOf};
use spot_sync_master::{Doctype, ScopeKey, Subscription};
use spot_sync_obligations::{Lane, MandatoryEvent, ObligationSettings, Pending};
use spot_sync_policy::{Class, Direction, FieldName, SyncPolicy, Trigger};
use spot_sync_protocol::NodeId;
use spot_sync_skew::{NodeReport, SoftwareVersion};

/// `RefCell`'s `borrow` and `borrow_mut` over a mutex: the relay's collaborators are `Send`.
pub struct SyncCell<T>(std::sync::Mutex<T>);

impl<T> SyncCell<T> {
    pub fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }
    pub fn borrow(&self) -> std::sync::MutexGuard<'_, T> {
        self.0.lock().unwrap()
    }
    pub fn borrow_mut(&self) -> std::sync::MutexGuard<'_, T> {
        self.0.lock().unwrap()
    }
}

pub const TENANT: &str = "t1";
pub const SOURCE: &str = "hub";

/// The text of every field name and value the tests treat as secret. Each must be absent from the
/// serialised bytes of every queued bundle.
pub const SECRETS: &[&str] = &[
    "COST-0.40-SECRET",
    "MARGIN-77-SECRET",
    "ROWCOST-0.30-SECRET",
    "ROWMARGIN-12-SECRET",
    "AUDITOR-NAME-SECRET",
    "SUPCOST-5-SECRET",
    "ORPHAN-SECRET",
];

/// The policy book of the tests: `price_list` excludes `cost`, `margin` and the table
/// `audit_rows`; `item` excludes `supplier_cost`. The entity `orphan` has no policy on purpose.
pub fn book() -> PolicyBook {
    fn policy(excluded: &[&str]) -> SyncPolicy {
        SyncPolicy::new(
            Class::Master,
            None,
            Direction::Down,
            Trigger::OnSubmit,
            excluded
                .iter()
                .map(|f| FieldName::new(f).unwrap())
                .collect(),
        )
    }
    let mut b = PolicyBook::new();
    b.insert("price_list", policy(&["cost", "margin", "audit_rows"]));
    b.insert("item", policy(&["supplier_cost"]));
    b
}

/// A price list image with an excluded field, an excluded table and excluded names inside the
/// rows of a kept child table.
pub fn price_list_image() -> Value {
    json!({
        "rate": "1.00",
        "cost": "COST-0.40-SECRET",
        "margin": "MARGIN-77-SECRET",
        "tiers": [
            {"qty": "1", "cost": "ROWCOST-0.30-SECRET", "margin": "ROWMARGIN-12-SECRET"},
            {"qty": "10", "cost": "ROWCOST-0.30-SECRET"}
        ],
        "audit_rows": [{"who": "AUDITOR-NAME-SECRET"}]
    })
}

/// What a price list image must look like after the redaction.
pub fn price_list_redacted() -> Value {
    json!({"rate": "1.00", "tiers": [{"qty": "1"}, {"qty": "10"}]})
}

/// The scope keys each record requires, from the hub registry. A key that is absent is unknown
/// to the hub and is not covered.
pub struct Registry(pub BTreeMap<RecordKey, Vec<String>>);

impl ScopeKeysOf for Registry {
    fn scope_keys(&self, k: &RecordKey) -> Option<Vec<String>> {
        self.0.get(k).cloned()
    }
}

/// Every record the tests use is for every subscriber of its entity, except the price lists,
/// which name a company.
pub fn registry(companies: &[(&str, &str)]) -> &'static Registry {
    let mut m = BTreeMap::new();
    for (id, company) in companies {
        m.insert(
            RecordKey::new("price_list", *id),
            vec![format!("company:{company}")],
        );
    }
    for (entity, id) in [
        ("item", "I8"),
        ("item", "I9"),
        ("orphan", "O1"),
        ("orphan", "Z1"),
    ] {
        m.insert(RecordKey::new(entity, id), Vec::new());
    }
    Box::leak(Box::new(Registry(m)))
}

/// A field `ref_<entity>` holding a text id is a reference to that record.
pub struct Refs;

impl References for Refs {
    fn references(&self, _entity: &str, image: &Value) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(image
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(name, v)| {
                        let entity = name.strip_prefix("ref_")?;
                        Some(RecordKey::new(entity, v.as_str()?))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[derive(Default, Clone)]
pub struct Src(pub BTreeMap<RecordKey, SourceImage>);

impl Src {
    pub fn put(&mut self, entity: &str, id: &str, image: Value, position: u64) {
        let key = RecordKey::new(entity, id);
        self.0
            .insert(key.clone(), SourceImage::new(key, image, position));
    }
}

impl ImageSource for Src {
    fn image(&self, key: &RecordKey) -> Result<Option<SourceImage>, ClosureError> {
        Ok(self.0.get(key).cloned())
    }
}

pub fn entry(position: u64, tenant: &str, entity: &str, id: &str) -> StreamEntry {
    StreamEntry {
        position,
        tenant: tenant.into(),
        source: SOURCE.into(),
        origin: None,
        txn: format!("{tenant}-t{position}"),
        role: EntryRole::Root,
        key: RecordKey::new(entity, id),
        version: 1,
        topic: entity.into(),
        major: 1,
        lane: Lane::Bulk,
        deleted: false,
    }
}

/// A node that decodes major 1 of every entity the tests use.
pub fn report() -> NodeReport {
    NodeReport {
        software_version: SoftwareVersion::new(1, 0, 0),
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: ["price_list", "item", "orphan"]
            .iter()
            .map(|e| {
                (
                    (*e).to_owned(),
                    [1u32].into_iter().collect::<BTreeSet<u32>>(),
                )
            })
            .collect(),
    }
}

/// The hub-side subscription of a node: company scope keys and entities.
pub fn subscription(node: &str, companies: &[&str], entities: &[&str]) -> &'static Subscription {
    Box::leak(Box::new(Subscription::new(
        NodeId::new(node).unwrap(),
        companies
            .iter()
            .map(|c| ScopeKey::new("company", c).unwrap()),
        entities.iter().map(|e| Doctype::new(e).unwrap()),
    )))
}

/// The entities of a subscription as the host-side start check lists them.
pub fn scope_entities(sub: &Subscription) -> Vec<String> {
    sub.doctypes()
        .iter()
        .map(|d| d.as_str().to_owned())
        .collect()
}

/// The check a deployment writes before it starts a destination: every entity of its scope has a
/// policy. `Err` carries the uncovered entities, so the destination is refused.
pub fn start_check(book: &PolicyBook, sub: &Subscription) -> Result<(), Vec<String>> {
    let missing = book.missing_for(&scope_entities(sub));
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// A redactor handle the test keeps, so it can read `unredacted_entities` after the tick.
pub struct SharedRedactor(pub &'static PolicyRedactor<'static>);

impl Redactor for SharedRedactor {
    fn redact_down(&self, entity: &str, image: &mut Value) {
        self.0.redact_down(entity, image);
    }
}

/// A destination whose scope is the master-data `MasterScope` and whose redactor is the policy
/// redactor, plus the handle to that redactor.
pub fn destination(
    id: &str,
    sub: &'static Subscription,
    reg: &'static Registry,
    book: &'static PolicyBook,
) -> (Destination, &'static PolicyRedactor<'static>) {
    let redactor: &'static PolicyRedactor<'static> = Box::leak(Box::new(PolicyRedactor::new(book)));
    let dest = Destination::new(
        id,
        Box::new(MasterScope::new(sub, reg)),
        Box::new(SharedRedactor(redactor)),
        report(),
    );
    (dest, redactor)
}

pub type Ledger = AckLedger<MemoryStore, MemorySink>;
pub type Relay = HubRelay<MemoryQueue, MemoryStream, Src>;

/// A relay for `tenant` over `entries` and `src`, with the shared ledger set.
pub fn relay(
    tenant: &str,
    entries: Vec<StreamEntry>,
    src: Src,
    led: &Arc<SyncCell<Ledger>>,
) -> Relay {
    let mut stream = MemoryStream::new();
    for e in entries {
        stream.append(e);
    }
    let mut r = HubRelay::new(
        RelayConfig::new(tenant, SOURCE),
        stream,
        MemoryQueue::new(),
        src,
        Box::new(Refs),
    )
    .unwrap();
    r.set_ledger(Box::new(Shared(led.clone())));
    r
}

pub fn ledger() -> Arc<SyncCell<Ledger>> {
    Arc::new(SyncCell::new(
        AckLedger::with_catalogue(
            MemoryStore::new(),
            MemorySink::new(),
            ObligationSettings::default(),
        )
        .unwrap(),
    ))
}

/// One ledger owned by the test and shared by the relays of several tenants (as the hub's host
/// would share it); the relay passes its own tenant on every call.
#[derive(Clone)]
pub struct Shared(pub Arc<SyncCell<Ledger>>);

impl AckBook for Shared {
    fn on_ack(
        &mut self,
        node: &str,
        tenant: &str,
        prefix: u64,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError> {
        self.0
            .borrow_mut()
            .on_ack(node, tenant, prefix, head, at)
            .map(|_| ())
    }
    fn on_checkin(&mut self, node: &str, tenant: &str, at: i64) -> Result<(), AckError> {
        let prefix = self
            .0
            .borrow()
            .store()
            .load(node, tenant)
            .map(|r| r.applied_prefix)
            .ok_or_else(|| AckError::UnknownNode {
                node: node.into(),
                tenant: tenant.into(),
            })?;
        self.on_ack(node, tenant, prefix, StreamHead::new(prefix), at)
    }
    fn set_holds(
        &mut self,
        node: &str,
        tenant: &str,
        holds: Vec<u64>,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError> {
        self.0
            .borrow_mut()
            .set_holds(node, tenant, holds, head, at)
            .map(|_| ())
    }
    fn register_node(
        &mut self,
        node: &str,
        tenant: &str,
        version: &str,
        at: i64,
    ) -> Result<bool, AckError> {
        self.0.borrow_mut().register_node(node, tenant, version, at)
    }
    fn register_mandatory(&mut self, tenant: &str, event: MandatoryEvent) -> bool {
        self.0.borrow_mut().register_mandatory(tenant, event)
    }
    fn mandatory_events(&self, tenant: &str) -> Vec<spot_sync_freshness::MandatoryEvent> {
        self.0
            .borrow()
            .store()
            .events(tenant)
            .into_iter()
            .map(|e| spot_sync_freshness::MandatoryEvent {
                position: e.position,
                published_at: e.published_at_millis,
                effective_from: e.effective_from_millis,
                record: e.record,
            })
            .collect()
    }
    fn who_is_missing(&self, tenant: &str, position: u64) -> Result<Vec<NodeReportRow>, AckError> {
        self.0.borrow().who_is_missing(tenant, position)
    }
}

/// The applied prefix the ledger holds for `(node, tenant)`, or `None` when it has no record.
pub fn applied(led: &Arc<SyncCell<Ledger>>, node: &str, tenant: &str) -> Option<u64> {
    led.borrow()
        .store()
        .load(node, tenant)
        .map(|r| r.applied_prefix)
}

/// Everything queued for `dest`, in serving order.
pub fn queued(relay: &Relay, dest: &str) -> Vec<Pending<QueuedBundle>> {
    relay.queue().all(dest)
}

/// The serialised bytes of every queued bundle of `dest`, as one text.
pub fn bundle_text(relay: &Relay, dest: &str) -> String {
    queued(relay, dest)
        .iter()
        .map(|p| serde_json::to_string(&p.item.bundle).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `(entity, id, image)` of every record queued for `dest`.
pub fn records(relay: &Relay, dest: &str) -> Vec<(String, String, Value)> {
    queued(relay, dest)
        .iter()
        .flat_map(|p| {
            p.item
                .bundle
                .records
                .iter()
                .map(|r| (r.key.entity.clone(), r.key.id.clone(), r.image.clone()))
        })
        .collect()
}

/// The names that must never appear as a JSON key in a queued bundle.
pub const EXCLUDED_KEYS: &[&str] = &[
    "\"cost\"",
    "\"margin\"",
    "\"audit_rows\"",
    "\"supplier_cost\"",
];

/// Assert that no secret name or value is in the bytes.
pub fn assert_no_leak(text: &str) {
    for s in SECRETS.iter().chain(EXCLUDED_KEYS) {
        assert!(
            !text.contains(s),
            "`{s}` leaked into a queued bundle: {text}"
        );
    }
}
