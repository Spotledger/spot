//! The report test world: three DocTypes built through the real metadata, a permission model
//! with six principals and a protection group, seeded data, and the fake backend behind the
//! report engine.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_core::ManualClock;
use spot_field_guard::{
    AuditRecord, BoundedQueue, FieldAccessRecord, FieldGuard, MetaProtection, ProtectionSource,
};
use spot_meta::{
    Definition, EffectiveMeta, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec, Registry,
};
use spot_permissions::{
    Action, AppliesTo, AssignmentContext, DocFacts, Model, ModelTexts, PermissionEngine, Principal,
    Role, Snapshot, UserPermission, load,
};
use spot_reports::{
    BoxFuture, CompiledReport, Compiler, EffectiveMetaSet, QueryExecutor, ReportDef, ReportError,
};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, Fields, Isolation, Query, Rows, StorageBackend, StorageError,
    Transaction, TxOptions,
};
use spot_storage_fake::FakeBackend;

pub const DOCTYPES: [&str; 3] = ["company", "account", "journal"];

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// The three DocTypes: `company`, `account` (links `company`) and `journal` (links `account`,
/// with a protected `memo`).
pub fn definitions() -> Vec<Definition> {
    let mut company = Definition::new("company");
    let mut region = FieldSpec::new("region", FieldType::Select);
    region.options = Some("north\nsouth\neast".into());
    company.fields = vec![text("title"), region, text("created_by")];

    let mut account = Definition::new("account");
    let mut kind = FieldSpec::new("kind", FieldType::Select);
    kind.options = Some("asset\nliability\nincome\nexpense".into());
    let mut link = FieldSpec::new("company", FieldType::Link);
    link.options = Some("company".into());
    account.fields = vec![text("title"), kind, link, text("created_by")];

    let mut journal = Definition::new("journal");
    let mut account_link = FieldSpec::new("account", FieldType::Link);
    account_link.options = Some("account".into());
    let mut debit = FieldSpec::new("debit", FieldType::Decimal);
    debit.scale = Some(2);
    let mut credit = FieldSpec::new("credit", FieldType::Decimal);
    credit.scale = Some(2);
    let mut memo = text("memo");
    memo.protection_group = Some("pii".into());
    memo.masking = Some(MaskSpec {
        placeholder: false,
        last_chars: Some(2),
    });
    journal.fields = vec![
        text("title"),
        account_link,
        debit,
        credit,
        memo,
        text("created_by"),
    ];
    journal.protection_groups = vec![ProtectionGroupSpec::new("pii", "Personal")];

    vec![company, account, journal]
}

pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions())
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

const GROUPS: &str = r#"
[[entries]]
name = "pii"
"#;

fn grants() -> String {
    let mut out = String::new();
    for (role, rights, if_owner) in [
        ("reader", "[\"read\", \"report\"]", ""),
        (
            "owner_reader",
            "[\"read\", \"report\"]",
            "if_owner = true\n",
        ),
        ("masked", "[\"read\", \"report\"]", ""),
        ("analyst", "[\"read\", \"report\"]", ""),
        // Holds the field-level `read` right on the group, but not `report`.
        ("readonly", "[\"read\", \"report\"]", ""),
        ("writer", "[\"write\"]", ""),
    ] {
        for doctype in DOCTYPES {
            out.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\nrights = {rights}\n{if_owner}"
            ));
        }
    }
    // A role that reads only journals, so a join to an unreadable account is exercised.
    out.push_str(
        "[[entries]]\nrole = \"journal_reader\"\ndoctype = \"journal\"\nrights = [\"read\", \"report\"]\n",
    );
    out
}

fn field_grants() -> String {
    let mut out = String::new();
    out.push_str(
        "[[entries]]\nrole = \"masked\"\ndoctype = \"journal\"\ntarget = { group = \"pii\" }\naccess = [\"read_masked\", \"report\"]\n",
    );
    out.push_str(
        "[[entries]]\nrole = \"analyst\"\ndoctype = \"journal\"\ntarget = { group = \"pii\" }\naccess = [\"read\", \"report\"]\n",
    );
    out.push_str(
        "[[entries]]\nrole = \"readonly\"\ndoctype = \"journal\"\ntarget = { group = \"pii\" }\naccess = [\"read\"]\n",
    );
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

/// The engine, the field guard and the protection source, built over one metadata snapshot.
pub fn engine_and_guard(meta: &Arc<EffectiveMeta>) -> (Arc<PermissionEngine>, FieldGuard) {
    let protection = Arc::new(MetaProtection::new(meta));
    let mut snapshot = Snapshot::build(meta, &model()).unwrap_or_else(|e| panic!("{e:?}"));
    for doctype in DOCTYPES {
        let prot = protection.protection(doctype).unwrap();
        snapshot = snapshot
            .with_field_facts(doctype, prot.fields.clone())
            .unwrap_or_else(|e| panic!("{e:?}"));
    }
    let engine = Arc::new(PermissionEngine::new(Arc::new(snapshot)));
    let trace = Arc::new(BoundedQueue::<FieldAccessRecord>::new(256));
    let audit = Arc::new(BoundedQueue::<AuditRecord>::new(256));
    let clock = Arc::new(ManualClock::new(
        spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
    ));
    let guard = FieldGuard::new(engine.clone(), protection, trace, audit, clock);
    (engine, guard)
}

/// One seeded journal and the facts the decision core is asked about.
#[derive(Clone, Debug)]
pub struct JournalSeed {
    pub id: RecordId,
    pub account: RecordId,
    pub kind: &'static str,
    pub debit: Decimal,
    pub credit: Decimal,
    pub owner: &'static str,
    pub memo: String,
}

/// The world: metadata, catalog, backend, engine, guard and seeded rows.
pub struct World {
    pub meta: Arc<EffectiveMeta>,
    pub catalog: Arc<Catalog>,
    pub backend: FakeBackend,
    pub engine: Arc<PermissionEngine>,
    pub guard: FieldGuard,
    pub companies: Vec<RecordId>,
    pub accounts: Vec<RecordId>,
    pub journals: Vec<JournalSeed>,
}

const OWNERS: [&str; 4] = ["alice", "bob", "carol", "dave"];
const KINDS: [&str; 4] = ["asset", "liability", "income", "expense"];

pub async fn world() -> World {
    world_with(1000).await
}

/// A world with `count` journals.
pub async fn world_with(count: usize) -> World {
    let meta = meta();
    let catalog = Arc::new(
        meta.catalog(Collation::new("und").unwrap())
            .unwrap_or_else(|e| panic!("catalog: {e:?}")),
    );
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    let mut seed_options = TxOptions::new(n("seed"), Isolation::ReadCommitted);
    seed_options.max_writes = 1_000_000;
    let mut tx = backend.begin(catalog.clone(), seed_options).await.unwrap();

    let mut companies = Vec::new();
    for i in 0..3 {
        let id = RecordId::new_v7();
        companies.push(id);
        let mut fields = Fields::new();
        fields.insert(n("title"), Value::Text(format!("company-{i}")));
        fields.insert(
            n("region"),
            Value::Text(["north", "south", "east"][i].into()),
        );
        fields.insert(n("created_by"), Value::Text("zed".into()));
        insert(&mut tx, "company", id, fields).await;
    }

    let mut accounts = Vec::new();
    for i in 0..4 {
        let id = RecordId::new_v7();
        accounts.push(id);
        let mut fields = Fields::new();
        fields.insert(n("title"), Value::Text(format!("acct-{i}")));
        fields.insert(n("kind"), Value::Text(KINDS[i].into()));
        fields.insert(n("company"), Value::Id(companies[i % 3]));
        fields.insert(n("created_by"), Value::Text("zed".into()));
        insert(&mut tx, "account", id, fields).await;
    }

    let mut journals = Vec::new();
    for i in 0..count {
        let id = RecordId::new_v7();
        let account = accounts[i % 4];
        let owner = OWNERS[i % 4];
        let debit = Decimal::new(i64::try_from(i).unwrap() * 7, 2);
        let credit = Decimal::new(i64::try_from((i * 3) % 1000).unwrap(), 2);
        let memo = format!("memo-{i:04}");
        let mut fields = Fields::new();
        fields.insert(n("title"), Value::Text(format!("journal-{i:04}")));
        fields.insert(n("account"), Value::Id(account));
        fields.insert(n("debit"), Value::Decimal(debit));
        fields.insert(n("credit"), Value::Decimal(credit));
        fields.insert(n("memo"), Value::Text(memo.clone()));
        fields.insert(n("created_by"), Value::Text(owner.into()));
        insert(&mut tx, "journal", id, fields).await;
        journals.push(JournalSeed {
            id,
            account,
            kind: KINDS[i % 4],
            debit,
            credit,
            owner,
            memo,
        });
    }
    tx.commit().await.unwrap();

    let (engine, guard) = engine_and_guard(&meta);
    World {
        meta,
        catalog,
        backend,
        engine,
        guard,
        companies,
        accounts,
        journals,
    }
}

async fn insert<T: spot_storage_contract::Transaction>(
    tx: &mut T,
    entity: &str,
    id: RecordId,
    fields: Fields,
) {
    tx.insert(&DocumentData {
        entity: n(entity),
        id,
        fields,
        children: BTreeMap::new(),
    })
    .await
    .unwrap();
}

impl World {
    /// The facts of one seeded journal, as the decision core asks.
    pub fn journal_facts(&self, seed: &JournalSeed) -> DocFacts {
        let mut values = BTreeMap::new();
        values.insert("account".to_owned(), Value::Id(seed.account));
        DocFacts {
            doctype: "journal".into(),
            id: Some(seed.id.to_string()),
            created_by: Some(seed.owner.into()),
            state: None,
            values,
        }
    }

    /// The journal ids the decision core lets `principal` read, document by document.
    pub fn allowed_journals(&self, principal: &Principal) -> Vec<String> {
        self.journals
            .iter()
            .filter(|s| {
                self.engine
                    .can(principal, Action::Read, &self.journal_facts(s))
                    .allowed
            })
            .map(|s| s.id.to_string())
            .collect()
    }
}

/// The fake backend behind the report engine's [`QueryExecutor`].
pub struct FakeExecutor {
    pub backend: FakeBackend,
    pub catalog: Arc<Catalog>,
}

impl QueryExecutor for FakeExecutor {
    fn execute<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, Result<Rows, StorageError>> {
        Box::pin(async move {
            let mut tx = self
                .backend
                .begin(
                    self.catalog.clone(),
                    TxOptions::new(n("report"), Isolation::ReadCommitted).read_only(),
                )
                .await?;
            let rows = spot_storage_contract::QueryExecutor::query(&mut tx, query).await?;
            spot_storage_contract::Transaction::commit(tx).await?;
            Ok(rows)
        })
    }
}

/// Compiles a report definition against the world's metadata.
pub fn compile(world: &World, toml: &str) -> Result<CompiledReport, Vec<ReportError>> {
    let def = ReportDef::from_toml(toml).map_err(|e| vec![e])?;
    let set = EffectiveMetaSet::new(world.meta.clone());
    let compiler = Compiler::new(&set, &world.catalog);
    compiler.compile(&def)
}

/// Compiles a report definition against `meta`, for refusal tests.
pub fn compile_with(
    meta: &Arc<EffectiveMeta>,
    catalog: &Catalog,
    toml: &str,
) -> Result<CompiledReport, Vec<ReportError>> {
    let def = ReportDef::from_toml(toml).map_err(|e| vec![e])?;
    let set = EffectiveMetaSet::new(meta.clone());
    let compiler = Compiler::new(&set, catalog);
    compiler.compile(&def)
}

// ------------------------------------------------------------------ principals

pub fn principals() -> Vec<(&'static str, Principal)> {
    vec![
        ("alice", Principal::with_roles("alice", &["reader"])),
        ("bob", Principal::with_roles("bob", &["owner_reader"])),
        ("dave", Principal::with_roles("dave", &[])),
        ("erin", Principal::with_roles("erin", &["masked"])),
        ("frank", Principal::with_roles("frank", &["writer"])),
        ("gina", Principal::with_roles("gina", &["analyst"])),
        ("hank", Principal::with_roles("hank", &["readonly"])),
        ("system", Principal::system()),
    ]
}

/// Carol reads journals whose account is one of `accounts` (a scoped reader).
pub fn carol(accounts: &[RecordId]) -> Principal {
    let allowed: Vec<String> = accounts.iter().take(2).map(ToString::to_string).collect();
    Principal::user(
        "carol",
        vec![AssignmentContext {
            id: "a1".into(),
            roles: vec![Role("reader".into())],
            scope: vec![UserPermission {
                user: String::new(),
                doctype: None,
                field_or_link_doctype: "account".into(),
                allowed,
                applies_to: AppliesTo::AllDoctypes,
                include_descendants: false,
            }],
        }],
    )
}

// ------------------------------------------------------------------ report definitions

/// Ivy reads every journal but only the accounts of one company, so a joined row she may not
/// read yields NULLs for its columns rather than losing the journal row.
pub fn ivy(companies: &[RecordId]) -> Principal {
    Principal::user(
        "ivy",
        vec![
            AssignmentContext {
                id: "a1".into(),
                roles: vec![Role("journal_reader".into())],
                scope: Vec::new(),
            },
            AssignmentContext {
                id: "a2".into(),
                roles: vec![Role("reader".into())],
                scope: vec![UserPermission {
                    user: String::new(),
                    doctype: None,
                    field_or_link_doctype: "company".into(),
                    allowed: vec![companies[0].to_string()],
                    applies_to: AppliesTo::AllDoctypes,
                    include_descendants: false,
                }],
            },
        ],
    )
}

pub const JOURNAL_LIST: &str = r#"
name = "journal_list"
label = "Journal"
base = "journal"
limit = 100
max_rows = 5000
roles = ["reader", "owner_reader", "masked", "analyst"]

[[columns]]
field = "title"

[[columns]]
field = "debit"

[[columns]]
field = "credit"

[[order_by]]
field = "title"
"#;

pub const JOURNAL_IDS: &str = r#"
name = "journal_ids"
label = "Journal ids"
base = "journal"
limit = 100000
max_rows = 100000
roles = ["reader", "owner_reader", "masked", "analyst"]

[[columns]]
field = "id"

[[order_by]]
field = "id"
"#;

pub const JOURNAL_BY_ACCOUNT: &str = r#"
name = "journal_by_account"
label = "Journal by account"
base = "journal"
roles = ["reader", "owner_reader", "masked", "analyst"]

[[joins]]
alias = "acct"
doctype = "account"
on = "account"

[[columns]]
field = "title"

[[columns]]
field = "acct.title"

[[order_by]]
field = "title"
"#;

pub const TRIAL_BALANCE: &str = r#"
name = "trial_balance"
label = "Trial balance"
base = "journal"
limit = 100000
max_rows = 100000
roles = ["reader", "owner_reader", "masked", "analyst"]
group_by = ["acct.title"]

[[joins]]
alias = "acct"
doctype = "account"
on = "account"

[[columns]]
field = "acct.title"

[[columns]]
field = "debit"
aggregate = "sum"

[[columns]]
field = "credit"
aggregate = "sum"

[[order_by]]
field = "acct.title"
"#;

pub const JOURNAL_FILTERED: &str = r#"
name = "journal_filtered"
label = "Journal filtered"
base = "journal"
roles = ["reader", "owner_reader", "masked", "analyst"]

[[joins]]
alias = "acct"
doctype = "account"
on = "account"

[[columns]]
field = "title"

[[columns]]
field = "debit"

[[columns]]
field = "acct.kind"

[[parameters]]
name = "kind"
label = "Kind"
type = "select"
options = ["asset", "liability", "income", "expense"]
required = false

[[parameters]]
name = "min_amount"
label = "Minimum"
type = "decimal"
required = false

[[parameters]]
name = "company"
label = "Company"
type = "link"
doctype = "company"
required = false

[filters]
all = [
  { field = "acct.kind", op = "eq", param = "kind" },
  { field = "debit", op = "ge", param = "min_amount" },
  { field = "acct.company", op = "eq", param = "company" },
]

[[order_by]]
field = "title"
"#;

pub const MEMO_REPORT: &str = r#"
name = "memo_report"
label = "Memos"
base = "journal"
roles = ["reader", "owner_reader", "masked", "analyst"]

[[columns]]
field = "title"

[[columns]]
field = "memo"

[[order_by]]
field = "title"
"#;

pub const MEMO_FILTER: &str = r#"
name = "memo_filter"
label = "Memos filtered"
base = "journal"
roles = ["reader", "owner_reader", "masked", "analyst"]

[[columns]]
field = "title"

[[parameters]]
name = "memo"
label = "Memo"
type = "text"
required = true

[filters]
field = "memo"
op = "eq"
param = "memo"
"#;

pub const COMPUTED_REPORT: &str = r#"
name = "computed_report"
label = "Computed"
base = "journal"
limit = 100000
max_rows = 100000
roles = ["reader", "owner_reader", "masked", "analyst"]

[[columns]]
field = "debit"

[[columns]]
field = "credit"

[[computed]]
name = "net"
label = "Net"
type = "decimal"
expr = "doc.debit - doc.credit"

[[order_by]]
field = "debit"
"#;

pub const AVG_REPORT: &str = r#"
name = "avg_report"
label = "Average debit"
base = "journal"
limit = 100000
max_rows = 100000
roles = ["reader", "owner_reader", "masked", "analyst"]
group_by = ["acct.title"]

[[joins]]
alias = "acct"
doctype = "account"
on = "account"

[[columns]]
field = "acct.title"

[[columns]]
field = "debit"
aggregate = "avg"

[[order_by]]
field = "acct.title"
"#;

pub const SMALL_LIST: &str = r#"
name = "small_list"
label = "Small"
base = "journal"
limit = 10
max_rows = 10
roles = ["reader"]

[[columns]]
field = "title"

[[order_by]]
field = "title"
"#;

pub const BIG_LIST: &str = r#"
name = "big_list"
label = "Big"
base = "journal"
limit = 100000
max_rows = 100000
roles = ["reader"]

[[columns]]
field = "title"

[[columns]]
field = "debit"

[[order_by]]
field = "title"
"#;
