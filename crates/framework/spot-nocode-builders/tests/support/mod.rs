//! The builder test world: three record types built through the real metadata, a permission
//! model with four principals, seeded journals, and the fake backend behind the report
//! engine.
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
use spot_permissions::{Model, ModelTexts, PermissionEngine, Principal, Snapshot, load};
use spot_reports::{
    BoxFuture, Compiler, EffectiveMetaSet, NoTelemetry, QueryExecutor, ReportArgs, ReportRunner,
};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, Fields, Isolation, Query, Rows, StorageBackend, StorageError,
    Transaction, TxOptions,
};
use spot_storage_fake::FakeBackend;
use time::{Date, Month};

/// The record types the world has.
pub const DOCTYPES: [&str; 3] = ["company", "account", "journal"];

/// A name the storage layer accepts.
pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn date(year: i32, month: Month, day: u8) -> Date {
    Date::from_calendar_date(year, month, day).unwrap()
}

/// The three DocTypes: `company`, `account` (links `company`) and `journal` (links `account`,
/// with a protected `memo`).
pub fn definitions() -> Vec<Definition> {
    let mut company = Definition::new("company");
    let mut region = FieldSpec::new("region", FieldType::Select);
    region.options = Some("north\nsouth\neast".into());
    company.fields = vec![text("title"), region, text("created_by")];

    let mut account = Definition::new("account");
    let mut link = FieldSpec::new("company", FieldType::Link);
    link.options = Some("company".into());
    account.fields = vec![text("title"), link, text("created_by")];

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
        FieldSpec::new("date", FieldType::Date),
        debit,
        credit,
        memo,
        text("created_by"),
    ];
    journal.protection_groups = vec![ProtectionGroupSpec::new("pii", "Personal")];

    vec![company, account, journal]
}

/// The effective metadata of the three DocTypes.
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
    for role in ["reader", "masked", "analyst"] {
        for doctype in DOCTYPES {
            out.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\nrights = [\"read\", \"report\"]\n"
            ));
        }
    }
    // A role that reads only journals, so a link to an account is exercised.
    out.push_str(
        "[[entries]]\nrole = \"journal_reader\"\ndoctype = \"journal\"\nrights = [\"read\", \"report\"]\n",
    );
    out.push_str("[[entries]]\nrole = \"writer\"\ndoctype = \"journal\"\nrights = [\"write\"]\n");
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
    out
}

/// The permission model.
pub fn model() -> Model {
    load(&ModelTexts {
        grants: grants(),
        field_grants: field_grants(),
        groups: GROUPS.to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

/// The engine, the field guard and the protection source, over one metadata snapshot.
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

/// One seeded journal.
#[derive(Clone, Debug)]
pub struct JournalSeed {
    pub id: RecordId,
    pub account: RecordId,
    pub date: Date,
    pub debit: Decimal,
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
    pub accounts: Vec<RecordId>,
    pub journals: Vec<JournalSeed>,
}

const OWNERS: [&str; 3] = ["alice", "bob", "carol"];

pub async fn world() -> World {
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

    let company = RecordId::new_v7();
    let mut fields = Fields::new();
    fields.insert(n("title"), Value::Text("northwind".into()));
    fields.insert(n("region"), Value::Text("north".into()));
    fields.insert(n("created_by"), Value::Text("zed".into()));
    insert(&mut tx, "company", company, fields).await;

    let mut accounts = Vec::new();
    for i in 0..2 {
        let id = RecordId::new_v7();
        accounts.push(id);
        let mut fields = Fields::new();
        fields.insert(n("title"), Value::Text(format!("acct-{i}")));
        fields.insert(n("company"), Value::Id(company));
        fields.insert(n("created_by"), Value::Text("zed".into()));
        insert(&mut tx, "account", id, fields).await;
    }

    let dates = [
        date(2026, Month::January, 15),
        date(2026, Month::February, 15),
    ];
    let mut journals = Vec::new();
    for i in 0..6usize {
        let id = RecordId::new_v7();
        let account = accounts[i % 2];
        let day = dates[i % 2];
        let owner = OWNERS[i % 3];
        let debit = Decimal::new(i64::try_from(i + 1).unwrap() * 100, 2);
        let memo = format!("memo-{i:04}");
        let mut fields = Fields::new();
        fields.insert(n("title"), Value::Text(format!("journal-{i:04}")));
        fields.insert(n("account"), Value::Id(account));
        fields.insert(n("date"), Value::Date(day));
        fields.insert(n("debit"), Value::Decimal(debit));
        fields.insert(n("credit"), Value::Decimal(Decimal::ZERO));
        fields.insert(n("memo"), Value::Text(memo.clone()));
        fields.insert(n("created_by"), Value::Text(owner.into()));
        insert(&mut tx, "journal", id, fields).await;
        journals.push(JournalSeed {
            id,
            account,
            date: day,
            debit,
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
        accounts,
        journals,
    }
}

async fn insert<T: Transaction>(tx: &mut T, entity: &str, id: RecordId, fields: Fields) {
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
    /// The metadata set the builder compiles against.
    pub fn meta_set(&self) -> EffectiveMetaSet {
        EffectiveMetaSet::new(self.meta.clone())
    }

    /// A runner over the fake backend.
    pub fn runner(&self) -> ReportRunner {
        ReportRunner::new(
            self.engine.clone(),
            self.guard.clone(),
            Arc::new(FakeExecutor {
                backend: self.backend.clone(),
                catalog: self.catalog.clone(),
            }),
            Arc::new(NoTelemetry),
        )
    }

    /// The row count of one (date, account title) bucket, seeded, for the golden assertions.
    pub fn bucket(&self, day: Date, account: usize) -> (i64, Decimal) {
        let title = format!("acct-{account}");
        let mut rows = 0i64;
        let mut total = Decimal::ZERO;
        for seed in &self.journals {
            if seed.date == day && self.accounts[account] == seed.account {
                rows += 1;
                total += seed.debit;
            }
        }
        let _ = title;
        (rows, total)
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
            Transaction::commit(tx).await?;
            Ok(rows)
        })
    }
}

/// The principals the tests use.
pub fn principal(name: &str) -> Principal {
    match name {
        "alice" => Principal::with_roles("alice", &["reader"]),
        "erin" => Principal::with_roles("erin", &["masked"]),
        "gina" => Principal::with_roles("gina", &["analyst"]),
        "frank" => Principal::with_roles("frank", &["writer"]),
        "system" => Principal::system(),
        _ => Principal::with_roles(name, &[]),
    }
}

/// A report runner over the world, for a golden run.
pub fn args(values: serde_json::Value) -> ReportArgs {
    ReportArgs::from_json(&values).unwrap()
}

/// Compiles a draft against the world's metadata.
pub fn compile(
    world: &World,
    draft: &spot_nocode_builders::ReportDraft,
) -> Result<spot_nocode_builders::ReportPlan, Vec<spot_nocode_builders::Issue>> {
    spot_nocode_builders::plan(draft, &world.meta_set(), &world.catalog)
}

/// A compiler over the world's metadata, for registering reports in a dashboard test.
pub fn report_compiler<'a>(world: &'a World, set: &'a EffectiveMetaSet) -> Compiler<'a> {
    Compiler::new(set, &world.catalog)
}
