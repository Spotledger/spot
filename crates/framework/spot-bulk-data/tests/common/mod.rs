//! Shared fixtures for the acceptance tests: a DocType with a child table and protected
//! fields, six principals, a real permission snapshot, a field guard, and a fake backend.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_bulk_data::{
    AllowAllExport, BulkLimits, ChildTable, CursorKey, ExportAuthority, ExportPlan, ExportSpec,
    FieldSet, NoopAuditSink, NoopCoordinator, NoopTelemetry, StreamContext,
};
use spot_core::ManualClock;
use spot_field_guard::{FieldGuard, MetaProtection, ProtectionSource};
use spot_meta::{
    Definition, EffectiveMeta, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec, Registry,
};
use spot_permissions::{
    Action, AssignmentContext, DocFacts, Model, ModelTexts, PermissionEngine, Principal, Snapshot,
    load,
};
use spot_storage_contract::{
    Catalog, ChildRow, Collation, DocumentData, DocumentStore, Fields, Isolation, Name, RecordId,
    StorageBackend, Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

/// The parent DocType.
pub const DOCTYPE: &str = "bulk_doc";
/// The child DocType.
pub const CHILD: &str = "bulk_line";
/// The six principals the suite exercises.
pub const USERS: [&str; 6] = ["nobody", "clerk", "exporter", "masked", "analyst", "owner"];

pub fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn last(n: u8) -> MaskSpec {
    MaskSpec {
        placeholder: false,
        last_chars: Some(n),
    }
}

fn grouped(name: &str, ty: FieldType, group: &str, mask: Option<MaskSpec>) -> FieldSpec {
    let mut f = FieldSpec::new(name, ty);
    f.protection_group = Some(group.to_owned());
    f.masking = mask;
    f
}

pub fn definitions() -> Vec<Definition> {
    let mut doc = Definition::new(DOCTYPE);
    let mut amount = FieldSpec::new("amount", FieldType::Decimal);
    amount.scale = Some(2);
    let mut lines = FieldSpec::new("lines", FieldType::Table);
    lines.options = Some(CHILD.into());
    let mut region = FieldSpec::new("region", FieldType::Select);
    region.options = Some("north\nsouth".into());
    doc.fields = vec![
        FieldSpec::new("modified", FieldType::DateTime),
        FieldSpec::new("title", FieldType::Text),
        FieldSpec::new("owner", FieldType::Text),
        amount,
        FieldSpec::new("note", FieldType::LongText),
        region,
        grouped("card_number", FieldType::Text, "pci", Some(last(4))),
        grouped(
            "salary",
            FieldType::Text,
            "hr",
            Some(MaskSpec::placeholder()),
        ),
        lines,
    ];
    doc.protection_groups = vec![
        ProtectionGroupSpec::new("pci", "Payment cards"),
        ProtectionGroupSpec::new("hr", "Compensation"),
    ];

    let mut line = Definition::new(CHILD);
    line.is_child = true;
    let mut qty = FieldSpec::new("qty", FieldType::Decimal);
    qty.scale = Some(3);
    line.fields = vec![qty, grouped("iban", FieldType::Text, "bank", Some(last(4)))];
    line.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
    vec![doc, line]
}

pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions())
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

pub fn protection(meta: &EffectiveMeta) -> Arc<MetaProtection> {
    Arc::new(MetaProtection::new(meta))
}

const GROUPS: &str = r#"
[[entries]]
name = "pci"
[[entries]]
name = "hr"
[[entries]]
name = "bank"
"#;

fn grants() -> String {
    let mut out = String::new();
    for (role, rights) in [
        ("Clerk", r#"["read"]"#),
        ("Reader", r#"["read", "export", "report"]"#),
        ("MaskedReporter", r#"["read", "export", "report"]"#),
        ("Analyst", r#"["read", "export", "report"]"#),
        ("OwnerReader", r#"["read", "export", "report"]"#),
        ("ScopedReader", r#"["read", "export", "report"]"#),
        ("ReadNoReport", r#"["read", "export", "report"]"#),
    ] {
        let owner = if role == "OwnerReader" {
            "\nif_owner = true"
        } else {
            ""
        };
        for doctype in [DOCTYPE, CHILD] {
            out.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\nrights = {rights}{owner}\n"
            ));
        }
    }
    out
}

fn field_grants() -> String {
    let mut out = String::new();
    let mut add = |role: &str, doctype: &str, group: &str, access: &str| {
        out.push_str(&format!(
            "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\ntarget = {{ group = \"{group}\" }}\naccess = {access}\n"
        ));
    };
    for (doctype, group) in [(DOCTYPE, "pci"), (DOCTYPE, "hr"), (CHILD, "bank")] {
        add(
            "MaskedReporter",
            doctype,
            group,
            r#"["read_masked", "report"]"#,
        );
        add("Analyst", doctype, group, r#"["read", "report"]"#);
        // May read the protected fields, but not report them (WP-SEC-04.T13).
        add("ReadNoReport", doctype, group, r#"["read"]"#);
    }
    // The scoped reader may see the child's bank column in full, so its child part is allowed.
    add("ScopedReader", CHILD, "bank", r#"["read", "report"]"#);
    out
}

pub fn model() -> Model {
    load(&ModelTexts {
        grants: grants(),
        field_grants: field_grants(),
        groups: GROUPS.to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

pub fn snapshot(meta: &EffectiveMeta, protection: &dyn ProtectionSource) -> Snapshot {
    let mut snap = Snapshot::build(meta, &model()).unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    for doctype in [DOCTYPE, CHILD] {
        let prot = protection.protection(doctype).unwrap();
        snap = snap
            .with_field_facts(doctype, prot.fields.clone())
            .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    }
    snap
}

pub fn principal(name: &str) -> Principal {
    match name {
        "nobody" => Principal::user("nobody", vec![]),
        "clerk" => Principal::user("clerk", vec![assignment("Clerk")]),
        "exporter" => Principal::user("exporter", vec![assignment("Reader")]),
        "masked" => Principal::user("masked", vec![assignment("MaskedReporter")]),
        "analyst" => Principal::user("analyst", vec![assignment("Analyst")]),
        "owner" => Principal::user("owner", vec![assignment("OwnerReader")]),
        "readnr" => Principal::user("readnr", vec![assignment("ReadNoReport")]),
        "scoped" => Principal::user(
            "scoped",
            vec![AssignmentContext {
                id: "a1".into(),
                roles: vec![spot_permissions::Role("ScopedReader".into())],
                scope: vec![spot_permissions::UserPermission {
                    user: String::new(),
                    doctype: None,
                    field_or_link_doctype: format!("{DOCTYPE}.region"),
                    allowed: vec!["north".into()],
                    applies_to: spot_permissions::AppliesTo::AllDoctypes,
                    include_descendants: false,
                }],
            }],
        ),
        "system" => Principal::system(),
        other => panic!("unknown principal {other}"),
    }
}

fn assignment(role: &str) -> AssignmentContext {
    AssignmentContext {
        id: "a1".into(),
        roles: vec![spot_permissions::Role(role.into())],
        scope: Vec::new(),
    }
}

/// The export authority the fixture uses: the stand-in for a host wrapping its
/// `PermissionEngine`'s `export` right. `clerk` has `read` but not `export`.
pub struct TestAuthority;

impl ExportAuthority for TestAuthority {
    fn may_export(&self, principal: &Principal, _doctype: &str) -> bool {
        principal.is_system
            || matches!(
                principal.user.as_str(),
                "exporter" | "masked" | "analyst" | "owner" | "scoped" | "readnr"
            )
    }
}

/// One inserted document and the facts the decision core is asked about.
pub struct Doc {
    pub id: RecordId,
    pub facts: DocFacts,
    pub modified: i64,
}

/// The world: a fake backend, a catalog, the guard and the decision core.
pub struct World {
    pub backend: FakeBackend,
    pub catalog: Arc<Catalog>,
    pub guard: FieldGuard,
    pub engine: Arc<PermissionEngine>,
    pub protection: Arc<MetaProtection>,
    pub meta: Arc<EffectiveMeta>,
    pub docs: Vec<Doc>,
    pub base: time::OffsetDateTime,
}

/// The field set the plan is built against. `id` is a system column the export may carry.
pub fn field_set() -> FieldSet {
    FieldSet::new(
        DOCTYPE,
        "owner",
        &[
            "id",
            "modified",
            "title",
            "owner",
            "region",
            "amount",
            "note",
            "card_number",
            "salary",
        ],
    )
    .with_child(ChildTable::new("lines", CHILD, &["qty", "iban"]))
}

/// A base instant so `modified` values are deterministic.
pub fn base_instant() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_770_000_000).unwrap()
}

/// Builds a world with `n` documents. Owners cycle through the users so the owner rules bite.
pub async fn build(n: usize) -> World {
    build_with(n, |i| (i / 3) as i64).await
}

/// Builds a world where every document shares one `modified` value (the keyset stress case).
pub async fn build_same_modified(n: usize) -> World {
    build_with(n, |_| 0).await
}

async fn build_with(n: usize, modified_of: impl Fn(usize) -> i64) -> World {
    let meta = meta();
    let catalog = Arc::new(meta.catalog(Collation::new("und").unwrap()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    let base = base_instant();
    let owners = ["clerk", "exporter", "masked", "analyst", "owner"];
    let mut docs = Vec::new();
    let mut options = TxOptions::new(name("seed_bulk"), Isolation::ReadCommitted);
    options.max_writes = 1_000_000;
    let mut tx = backend.begin(catalog.clone(), options).await.unwrap();
    for i in 0..n {
        let id = RecordId::new_v7();
        let owner = owners[i % owners.len()];
        let region = if i % 2 == 0 { "north" } else { "south" };
        let modified = base + time::Duration::seconds(modified_of(i));
        let mut fields = Fields::new();
        fields.insert(name("modified"), Value::DateTime(modified));
        fields.insert(name("title"), Value::Text(format!("doc {i}")));
        fields.insert(name("owner"), Value::Text(owner.into()));
        fields.insert(name("region"), Value::Text(region.into()));
        fields.insert(
            name("amount"),
            Value::Decimal(rust_decimal::Decimal::new(1000 + i as i64, 2)),
        );
        fields.insert(name("note"), Value::Text(format!("note {i}")));
        fields.insert(
            name("card_number"),
            Value::Text(format!("4111-1111-1111-{i:04}")),
        );
        fields.insert(name("salary"), Value::Text(format!("salary {i}")));
        let mut children = BTreeMap::new();
        let mut rows = Vec::new();
        for j in 0..2 {
            let mut cf = Fields::new();
            cf.insert(
                name("qty"),
                Value::Decimal(rust_decimal::Decimal::new(100 + j, 3)),
            );
            cf.insert(name("iban"), Value::Text(format!("GB00-{i:04}-{j}")));
            rows.push(ChildRow {
                id: RecordId::new_v7(),
                fields: cf,
            });
        }
        children.insert(name("lines"), rows);
        tx.insert(&DocumentData {
            entity: name(DOCTYPE),
            id,
            fields,
            children,
        })
        .await
        .unwrap();
        let mut values = BTreeMap::new();
        values.insert("owner".to_owned(), Value::Text(owner.into()));
        values.insert("region".to_owned(), Value::Text(region.into()));
        docs.push(Doc {
            id,
            modified: modified_of(i),
            facts: DocFacts {
                doctype: DOCTYPE.into(),
                id: Some(id.to_string()),
                created_by: Some(owner.into()),
                state: None,
                values,
            },
        });
    }
    tx.commit().await.unwrap();

    let protection = protection(&meta);
    let engine = Arc::new(PermissionEngine::new(Arc::new(snapshot(
        &meta,
        &*protection,
    ))));
    let clock = Arc::new(ManualClock::new(base));
    let guard = FieldGuard::new(
        engine.clone(),
        protection.clone(),
        Arc::new(spot_field_guard::BoundedQueue::new(1024)),
        Arc::new(spot_field_guard::BoundedQueue::new(1024)),
        clock,
    );
    World {
        backend,
        catalog,
        guard,
        engine,
        protection,
        meta,
        docs,
        base,
    }
}

impl World {
    /// The ids of `doctype` the decision core lets the principal read.
    pub fn allowed(&self, principal: &Principal) -> std::collections::BTreeSet<RecordId> {
        self.docs
            .iter()
            .filter(|d| self.engine.can(principal, Action::Read, &d.facts).allowed)
            .map(|d| d.id)
            .collect()
    }

    /// The export plan for `spec` and `principal`.
    pub fn plan(&self, spec: &ExportSpec, principal: &Principal) -> ExportPlan {
        ExportPlan::build(
            spec,
            principal,
            &self.guard,
            &TestAuthority,
            &field_set(),
            &*self.protection,
        )
        .unwrap_or_else(|e| panic!("plan: {e}"))
    }

    /// The plan, allowing the export right for anyone (for cursor tests).
    pub fn plan_allow_all(&self, spec: &ExportSpec, principal: &Principal) -> ExportPlan {
        ExportPlan::build(
            spec,
            principal,
            &self.guard,
            &AllowAllExport,
            &field_set(),
            &*self.protection,
        )
        .unwrap_or_else(|e| panic!("plan: {e}"))
    }

    /// A stream context for `principal`.
    pub fn ctx(&self, principal_name: &str, limits: BulkLimits) -> StreamContext {
        StreamContext {
            reader: Arc::new(spot_bulk_data::BackendPageReader::new(
                self.backend.clone(),
                self.catalog.clone(),
            )),
            key: CursorKey::new(b"fixture-key".to_vec()),
            principal: principal_name.to_owned(),
            tenant: "tenant-1".to_owned(),
            limits,
            coordinator: Arc::new(NoopCoordinator),
            telemetry: Arc::new(NoopTelemetry),
            audit: Arc::new(NoopAuditSink),
        }
    }

    /// Runs a whole export and returns `(bytes, rows)`.
    pub async fn export_bytes(&self, spec: &ExportSpec, principal_name: &str) -> (Vec<u8>, u64) {
        let plan = Arc::new(self.plan(spec, &principal(principal_name)));
        let mut stream =
            spot_bulk_data::ExportStream::start(plan, self.ctx(principal_name, small_limits()))
                .unwrap();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next_chunk().await.unwrap() {
            bytes.extend_from_slice(&chunk.bytes);
        }
        (bytes, stream.rows())
    }

    /// The exported parent ids, in delivery order, for `principal_name` (exports the `id`
    /// column only, so the file is one id per line).
    pub async fn export_ids(&self, spec: &ExportSpec, principal_name: &str) -> Vec<RecordId> {
        let mut id_spec = spec.clone();
        id_spec.columns = vec!["id".to_owned()];
        let plan = Arc::new(self.plan(&id_spec, &principal(principal_name)));
        let mut stream =
            spot_bulk_data::ExportStream::start(plan, self.ctx(principal_name, small_limits()))
                .unwrap();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next_chunk().await.unwrap() {
            bytes.extend_from_slice(&chunk.bytes);
        }
        let mut ids = Vec::new();
        for line in String::from_utf8_lossy(&bytes).lines().skip(1) {
            if line.is_empty() {
                continue;
            }
            if let Ok(u) = uuid::Uuid::parse_str(line)
                && let Ok(id) = RecordId::from_bytes(*u.as_bytes())
            {
                ids.push(id);
            }
        }
        ids
    }
}

/// Small limits so tests cross many chunk boundaries.
pub fn small_limits() -> BulkLimits {
    BulkLimits {
        rows_per_chunk: 3,
        bytes_per_chunk: 512,
        ..BulkLimits::default()
    }
}
