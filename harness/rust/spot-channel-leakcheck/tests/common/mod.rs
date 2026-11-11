//! The shared world: one DocType set with protection groups built through the real metadata
//! (a derived field included), one permission model with seven principals, the leak corpus
//! planted in a document, and the expectation of what each principal may see.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_bulk_data::{
    AllowAllExport, BackendPageReader, BulkError, BulkLimits, ChildTable, CursorKey, ExportPlan,
    ExportSpec, ExportStream, FieldSet, Format, NoopAuditSink, NoopCoordinator, NoopTelemetry,
    StreamContext,
};
use spot_core::ManualClock;
use spot_field_guard::leakcheck::{Corpus, Secret};
use spot_field_guard::{
    AuditRecord, BoundedQueue, FieldAccessRecord, FieldGuard, MetaProtection, ProtectionSource,
};
use spot_meta::{
    BuildOptions, Definition, Derivation, EffectiveMeta, FieldRef, FieldSpec, FieldType, MaskSpec,
    ProtectionGroupSpec, Registry,
};
use spot_permissions::model::Principal;
use spot_permissions::{DocFacts, Model, ModelTexts, PermissionEngine, Snapshot, load};
use spot_storage_contract::{
    Catalog, ChildRow, Collation, DocumentData, DocumentStore, Fields, Isolation, Name, RecordId,
    StorageBackend, Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

pub const DOCTYPE: &str = "sales_invoice";
pub const CHILD: &str = "sales_invoice_item";

pub const PLAIN_NUMBER: &str = "INV-0001";
pub const PLAIN_NOTE: &str = "ship early";

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
    let mut invoice = Definition::new(DOCTYPE);
    let mut items = FieldSpec::new("items", FieldType::Table);
    items.options = Some(CHILD.into());
    let mut bonus = grouped(
        "bonus",
        FieldType::Decimal,
        "finance",
        Some(MaskSpec::placeholder()),
    );
    bonus.scale = Some(2);
    invoice.fields = vec![
        FieldSpec::new("number", FieldType::Text),
        FieldSpec::new("notes", FieldType::LongText),
        FieldSpec::new("owner", FieldType::Text),
        FieldSpec::new("created_by", FieldType::Text),
        grouped("card_number", FieldType::Text, "pci", Some(last(4))),
        grouped(
            "holder",
            FieldType::Text,
            "pci",
            Some(MaskSpec::placeholder()),
        ),
        grouped(
            "profile_json",
            FieldType::LongText,
            "pci",
            Some(MaskSpec::placeholder()),
        ),
        grouped(
            "salary",
            FieldType::Text,
            "hr",
            Some(MaskSpec::placeholder()),
        ),
        bonus,
        grouped(
            "annual_cost",
            FieldType::Text,
            "hr",
            Some(MaskSpec::placeholder()),
        ),
        items,
    ];
    invoice.protection_groups = vec![
        ProtectionGroupSpec::new("pci", "Payment cards"),
        ProtectionGroupSpec {
            trace: true,
            ..ProtectionGroupSpec::new("hr", "Compensation")
        },
        ProtectionGroupSpec::new("finance", "Finance"),
    ];

    let mut item = Definition::new(CHILD);
    item.is_child = true;
    let mut qty = FieldSpec::new("qty", FieldType::Decimal);
    qty.scale = Some(3);
    item.fields = vec![
        qty,
        grouped("iban", FieldType::Text, "bank", Some(last(4))),
        grouped(
            "memo",
            FieldType::Text,
            "bank",
            Some(MaskSpec::placeholder()),
        ),
    ];
    item.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
    vec![invoice, item]
}

/// `annual_cost` is computed from `salary` (hr) and `bonus` (finance): it must inherit
/// `finance` as well, so it needs a grant in both groups.
pub fn derivations() -> Vec<Derivation> {
    vec![Derivation::new(
        FieldRef::new(DOCTYPE, "annual_cost"),
        vec![
            FieldRef::new(DOCTYPE, "salary"),
            FieldRef::new(DOCTYPE, "bonus"),
        ],
    )]
}

pub fn meta() -> Arc<EffectiveMeta> {
    let options = BuildOptions::new().with_derivations(&derivations());
    Registry::with_options(definitions(), options)
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

const GROUPS: &str = r#"
[[entries]]
name = "pci"
[[entries]]
name = "hr"
trace = true
[[entries]]
name = "finance"
[[entries]]
name = "bank"
"#;

fn grants() -> String {
    let mut out = String::new();
    for (role, rights) in [
        ("Clerk", r#"["read"]"#),
        ("MaskedReader", r#"["read", "report"]"#),
        ("Analyst", r#"["read", "report"]"#),
        ("ReadNoReport", r#"["read"]"#),
        ("NoCards", r#"["read", "report"]"#),
    ] {
        for doctype in [DOCTYPE, CHILD] {
            out.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\nrights = {rights}\n"
            ));
        }
    }
    out
}

fn field_grants() -> String {
    let mut out = String::new();
    let mut add = |role: &str, doctype: &str, group: &str, access: &str, deny: bool| {
        out.push_str(&format!(
            "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\ntarget = {{ group = \"{group}\" }}\naccess = {access}\ndeny = {deny}\n"
        ));
    };
    for (doctype, group) in [
        (DOCTYPE, "pci"),
        (DOCTYPE, "hr"),
        (DOCTYPE, "finance"),
        (CHILD, "bank"),
    ] {
        add(
            "MaskedReader",
            doctype,
            group,
            r#"["read_masked", "report"]"#,
            false,
        );
        add("Analyst", doctype, group, r#"["read", "report"]"#, false);
        add("NoCards", doctype, group, r#"["read", "report"]"#, false);
    }
    // The explicit deny: this role holds full rights everywhere except `pci`, which it is
    // told no to, and no grant lifts a deny.
    add("NoCards", DOCTYPE, "pci", r#"[]"#, true);
    // ReadNoReport holds `read` on two groups only, and no `report` anywhere.
    add("ReadNoReport", DOCTYPE, "pci", r#"["read"]"#, false);
    add("ReadNoReport", DOCTYPE, "hr", r#"["read"]"#, false);
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

/// The guard, the engine and the metadata every producer is built over.
pub struct Core {
    pub meta: Arc<EffectiveMeta>,
    pub protection: Arc<MetaProtection>,
    pub engine: Arc<PermissionEngine>,
    pub guard: FieldGuard,
    pub trace: Arc<BoundedQueue<FieldAccessRecord>>,
    pub audit: Arc<BoundedQueue<AuditRecord>>,
}

pub fn core() -> Core {
    let meta = meta();
    let protection = Arc::new(MetaProtection::new(&meta));
    let engine = Arc::new(PermissionEngine::new(Arc::new(snapshot(
        &meta,
        &*protection,
    ))));
    let trace = Arc::new(BoundedQueue::new(4096));
    let audit = Arc::new(BoundedQueue::new(4096));
    let clock = Arc::new(ManualClock::new(
        spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
    ));
    let guard = FieldGuard::new(
        engine.clone(),
        protection.clone(),
        trace.clone(),
        audit.clone(),
        clock,
    );
    Core {
        meta,
        protection,
        engine,
        guard,
        trace,
        audit,
    }
}

// ----------------------------------------------------------------- principals

/// Seven principals: no rights, read only, masked, analyst, read without report, an explicit
/// deny of one group, and the tenant administrator.
pub const PRINCIPALS: [&str; 7] = [
    "nobody",
    "clerk",
    "masked_reader",
    "analyst",
    "read_no_report",
    "no_cards",
    "admin",
];

pub fn principal(name: &str) -> Principal {
    match name {
        "nobody" => Principal::with_roles("nobody", &[]),
        "clerk" => Principal::with_roles("clerk", &["Clerk"]),
        "masked_reader" => Principal::with_roles("masked_reader", &["MaskedReader"]),
        "analyst" => Principal::with_roles("analyst", &["Analyst"]),
        "read_no_report" => Principal::with_roles("read_no_report", &["ReadNoReport"]),
        "no_cards" => Principal::with_roles("no_cards", &["NoCards"]),
        "admin" => Principal::with_roles("admin", &[spot_permissions::TENANT_ADMINISTRATOR]),
        other => panic!("unknown principal {other}"),
    }
}

pub const ALL_GROUPS: &[&str] = &["pci", "hr", "finance", "bank"];

/// The groups a principal sees **unmasked** through a channel that needs `read`, and through
/// one that needs `report`.
pub fn groups_seen(name: &str, needs_report: bool) -> &'static [&'static str] {
    match (name, needs_report) {
        ("analyst" | "admin", _) => ALL_GROUPS,
        ("no_cards", _) => &["hr", "finance", "bank"],
        ("read_no_report", false) => &["pci", "hr"],
        _ => &[],
    }
}

/// Whether a principal may read the DocType at all (a plain field follows this).
pub fn plain_visible(name: &str, needs_report: bool) -> bool {
    match name {
        "nobody" => false,
        "clerk" | "read_no_report" => !needs_report,
        _ => true,
    }
}

// ----------------------------------------------------------------- the corpus

/// Corpus label, the protection groups of the field that holds it, and where it is planted
/// (`field` or `table.column`).
pub const SECRETS: [(&str, &[&str], &str); 7] = [
    ("card", &["pci"], "card_number"),
    ("lookalike", &["pci"], "holder"),
    ("in_json", &["pci"], "profile_json"),
    ("salary", &["hr"], "salary"),
    ("annual", &["finance", "hr"], "annual_cost"),
    ("iban", &["bank"], "items.iban"),
    ("split_rows", &["bank"], "items.memo"),
];

pub fn corpus() -> Corpus {
    Corpus::standard().with(Secret::whole("annual", "1014009.00"))
}

/// The labels whose every group the principal sees unmasked.
pub fn visible_labels(name: &str, needs_report: bool) -> Vec<&'static str> {
    let groups = groups_seen(name, needs_report);
    SECRETS
        .iter()
        .filter(|(_, needs, _)| needs.iter().all(|g| groups.contains(g)))
        .map(|(label, _, _)| *label)
        .collect()
}

pub fn doc_facts() -> DocFacts {
    DocFacts {
        doctype: DOCTYPE.into(),
        ..DocFacts::default()
    }
}

// ----------------------------------------------------------------- storage

pub struct Store {
    pub backend: FakeBackend,
    pub catalog: Arc<Catalog>,
    pub ids: Vec<RecordId>,
}

/// A fake backend holding `copies` invoices with the corpus planted in every protected field,
/// the split secret across two child rows, and the plain fields untouched.
pub async fn store(meta: &EffectiveMeta, corpus: &Corpus, copies: usize) -> Store {
    let catalog = Arc::new(meta.catalog(Collation::new("und").unwrap()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    let split = corpus.get("split_rows").unwrap().planted.clone();
    let mut options = TxOptions::new(name("seed_leakcheck"), Isolation::ReadCommitted);
    options.max_writes = 100_000;
    let mut tx = backend.begin(catalog.clone(), options).await.unwrap();
    let mut ids = Vec::new();
    for i in 0..copies {
        let id = RecordId::new_v7();
        ids.push(id);
        let mut fields = Fields::new();
        fields.insert(name("number"), Value::Text(format!("INV-{:04}", i + 1)));
        fields.insert(name("notes"), Value::Text(PLAIN_NOTE.into()));
        fields.insert(name("owner"), Value::Text("seed".into()));
        fields.insert(name("created_by"), Value::Text("seed".into()));
        fields.insert(name("card_number"), Value::Text(value("card")));
        fields.insert(name("holder"), Value::Text(value("lookalike")));
        fields.insert(name("profile_json"), Value::Text(value("in_json")));
        fields.insert(name("salary"), Value::Text(value("salary")));
        fields.insert(
            name("bonus"),
            Value::Decimal(rust_decimal::Decimal::new(150_050, 2)),
        );
        fields.insert(name("annual_cost"), Value::Text(value("annual")));
        let mut rows = Vec::new();
        for (j, memo) in split.iter().enumerate() {
            let mut cf = Fields::new();
            cf.insert(
                name("qty"),
                Value::Decimal(rust_decimal::Decimal::new(2000 + j as i64, 3)),
            );
            cf.insert(name("iban"), Value::Text(value("iban")));
            cf.insert(name("memo"), Value::Text(memo.clone()));
            rows.push(ChildRow {
                id: RecordId::new_v7(),
                fields: cf,
            });
        }
        let mut children = BTreeMap::new();
        children.insert(name("items"), rows);
        tx.insert(&DocumentData {
            entity: name(DOCTYPE),
            id,
            fields,
            children,
        })
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    Store {
        backend,
        catalog,
        ids,
    }
}

// ----------------------------------------------------------------- export runner

pub const PARENT_COLUMNS: [&str; 9] = [
    "id",
    "number",
    "notes",
    "owner",
    "card_number",
    "holder",
    "profile_json",
    "salary",
    "annual_cost",
];
pub const CHILD_COLUMNS: [&str; 3] = ["qty", "iban", "memo"];

/// One export as `principal`, exporting `parent` columns and (when not empty) `child` columns
/// of the `items` table. Returns everything that left, or the refusal.
pub async fn run_export(
    core: &Core,
    store: &Store,
    principal: &Principal,
    format: Format,
    parent: &[&str],
    child: &[&str],
) -> Result<String, BulkError> {
    let mut fields = FieldSet::new(DOCTYPE, "owner", &PARENT_COLUMNS);
    let mut spec = ExportSpec::new(DOCTYPE, format).with_columns(parent);
    if !child.is_empty() {
        fields = fields.with_child(ChildTable::new("items", CHILD, child));
        spec = spec.with_children();
    }
    let plan = ExportPlan::build(
        &spec,
        principal,
        &core.guard,
        &AllowAllExport,
        &fields,
        &*core.protection,
    )?;
    let ctx = StreamContext {
        reader: Arc::new(BackendPageReader::new(
            store.backend.clone(),
            store.catalog.clone(),
        )),
        key: CursorKey::new(b"leakcheck-key".to_vec()),
        principal: principal.user.clone(),
        tenant: "tenant-1".to_owned(),
        limits: BulkLimits {
            rows_per_chunk: 2,
            bytes_per_chunk: 512,
            ..BulkLimits::default()
        },
        coordinator: Arc::new(NoopCoordinator),
        telemetry: Arc::new(NoopTelemetry),
        audit: Arc::new(NoopAuditSink),
    };
    let mut stream = ExportStream::start(Arc::new(plan), ctx)?;
    let mut text = String::new();
    while let Some(chunk) = stream.next_chunk().await? {
        text.push_str(&String::from_utf8_lossy(&chunk.bytes));
    }
    Ok(text)
}

// ----------------------------------------------------------------- report world

use spot_reports::{
    BoxFuture, Compiler, EffectiveMetaSet, NoTelemetry, QueryExecutor, ReportArgs, ReportDef,
    ReportError, ReportRunner, RunOutput,
};
use spot_storage_contract::{Query, Rows, StorageError};

struct Executor {
    backend: FakeBackend,
    catalog: Arc<Catalog>,
}

impl QueryExecutor for Executor {
    fn execute<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, Result<Rows, StorageError>> {
        Box::pin(async move {
            let mut tx = self
                .backend
                .begin(
                    self.catalog.clone(),
                    TxOptions::new(name("report"), Isolation::ReadCommitted).read_only(),
                )
                .await?;
            let rows = spot_storage_contract::QueryExecutor::query(&mut tx, query).await?;
            spot_storage_contract::Transaction::commit(tx).await?;
            Ok(rows)
        })
    }
}

pub fn report_toml(columns: &[&str], extra: &str) -> String {
    let mut out = format!(
        "name = \"leak_report\"\nlabel = \"Leak\"\nbase = \"{DOCTYPE}\"\nlimit = 100\nmax_rows = 1000\nroles = [\"Analyst\"]\n\n"
    );
    for c in columns {
        out.push_str(&format!("[[columns]]\nfield = \"{c}\"\n\n"));
    }
    out.push_str(extra);
    out
}

pub struct World {
    pub core: Core,
    pub store: Store,
}

pub async fn world() -> World {
    let core = core();
    let store = store(&core.meta, &corpus(), 3).await;
    World { core, store }
}

impl World {
    pub async fn run(
        &self,
        who: &str,
        toml: &str,
        args: ReportArgs,
    ) -> Result<RunOutput, ReportError> {
        let def = ReportDef::from_toml(toml)?;
        let set = EffectiveMetaSet::new(self.core.meta.clone());
        let report = Compiler::new(&set, &self.store.catalog)
            .compile(&def)
            .map_err(|mut e| e.remove(0))?;
        let runner = ReportRunner::new(
            self.core.engine.clone(),
            self.core.guard.clone(),
            Arc::new(Executor {
                backend: self.store.backend.clone(),
                catalog: self.store.catalog.clone(),
            }),
            Arc::new(NoTelemetry),
        );
        runner.run(&report, &principal(who), &args, None).await
    }
}
