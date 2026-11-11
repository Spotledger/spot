//! The fingerprint engine on the storage fake: declared queries, group-by measures, ageing
//! buckets, the integer rule and the digest.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_rehearsal::{
    BucketSpec, CloneTarget, FingerprintQuery, FingerprintSpec, StorageFingerprintSource, codes,
};
use spot_snapshot::RestoreTarget;
use spot_storage_contract::{
    AggFunc, Catalog, Collation, DocumentData, DocumentStore, EntityDef, EntityKind, Expr,
    FieldDef, FieldType, Fields, Isolation, Name, Query, QueryPath, RecordId, Selection,
    StorageBackend, Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

fn n(s: &str) -> Name {
    Name::new(s).expect("name")
}

fn field(name: &str, ty: FieldType) -> FieldDef {
    FieldDef {
        name: n(name),
        ty,
        nullable: false,
    }
}

fn text(max: u32) -> FieldType {
    FieldType::Text {
        max_chars: Some(max),
    }
}

fn catalog() -> Arc<Catalog> {
    let catalog = Catalog::new(
        1,
        Collation::new("en").expect("collation"),
        vec![EntityDef {
            name: n("trial_line"),
            kind: EntityKind::Document,
            fields: vec![
                field("account", text(40)),
                field("company", text(40)),
                field("amount", FieldType::Int),
                field("age_days", FieldType::Int),
            ],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        }],
    )
    .expect("catalog");
    Arc::new(catalog)
}

fn select(expr: Expr, alias: &str) -> Selection {
    Selection {
        expr,
        alias: n(alias),
    }
}

fn sum_amount() -> Expr {
    Expr::Agg {
        func: AggFunc::Sum,
        arg: Some(Box::new(Expr::field(&n("t"), &n("amount")))),
        distinct: false,
    }
}

fn trial_query() -> Query {
    let mut query = Query::from(&n("trial_line"), &n("t"), QueryPath::Report);
    query.select = vec![
        select(Expr::field(&n("t"), &n("account")), "account"),
        select(Expr::field(&n("t"), &n("company")), "company"),
        select(sum_amount(), "debit"),
    ];
    query.group_by = vec![
        Expr::field(&n("t"), &n("account")),
        Expr::field(&n("t"), &n("company")),
    ];
    query
}

fn ageing_query() -> Query {
    let mut query = Query::from(&n("trial_line"), &n("t"), QueryPath::Report);
    query.select = vec![
        select(Expr::field(&n("t"), &n("account")), "account"),
        select(Expr::field(&n("t"), &n("age_days")), "age_days"),
        select(sum_amount(), "outstanding"),
    ];
    query.group_by = vec![
        Expr::field(&n("t"), &n("account")),
        Expr::field(&n("t"), &n("age_days")),
    ];
    query
}

fn spec() -> FingerprintSpec {
    FingerprintSpec::new(vec![
        FingerprintQuery {
            key: "trial_balance".to_owned(),
            title: "Trial balance".to_owned(),
            dimensions: vec!["account".to_owned(), "company".to_owned()],
            measures: vec!["debit".to_owned()],
            bucket: None,
            query: trial_query(),
        },
        FingerprintQuery {
            key: "ageing".to_owned(),
            title: "Ageing".to_owned(),
            dimensions: vec!["account".to_owned(), "age_days".to_owned()],
            measures: vec!["outstanding".to_owned()],
            bucket: Some(BucketSpec {
                age_column: "age_days".to_owned(),
                bucket_dimension: "bucket".to_owned(),
                bounds: vec![30, 60, 90],
                labels: vec![
                    "0-30".to_owned(),
                    "31-60".to_owned(),
                    "61-90".to_owned(),
                    "90+".to_owned(),
                ],
            }),
            query: ageing_query(),
        },
    ])
}

async fn seed(backend: &Arc<FakeBackend>, catalog: &Arc<Catalog>) {
    let options = TxOptions::new(n("seed"), Isolation::ReadCommitted);
    let mut tx = backend
        .begin(catalog.clone(), options)
        .await
        .expect("begin");
    let rows: [(&str, &str, i64, i64); 3] = [
        ("cash", "acme", 100, 10),
        ("cash", "acme", 50, 45),
        ("bank", "acme", 200, 100),
    ];
    for (account, company, amount, age) in rows {
        let mut fields = Fields::new();
        fields.insert(n("account"), Value::Text(account.to_owned()));
        fields.insert(n("company"), Value::Text(company.to_owned()));
        fields.insert(n("amount"), Value::Int(amount));
        fields.insert(n("age_days"), Value::Int(age));
        tx.insert(&DocumentData {
            entity: n("trial_line"),
            id: RecordId::new_v7(),
            fields,
            children: BTreeMap::new(),
        })
        .await
        .expect("insert");
    }
    tx.commit().await.expect("commit");
}

fn clone() -> CloneTarget {
    CloneTarget::new(
        "localhost",
        RestoreTarget::Database {
            connection: "postgres://u:p@localhost/postgres".to_owned(),
            database: "spot_scratch".to_owned(),
        },
    )
}

fn measure(
    fp: &spot_rehearsal::Fingerprint,
    section: &str,
    dims: &[(&str, &str)],
    m: &str,
) -> i128 {
    let section = fp.section(section).expect("section");
    for row in &section.rows {
        let matches = dims.iter().all(|(name, value)| {
            row.dimensions
                .iter()
                .any(|d| d.name == *name && d.value == *value)
        });
        if matches {
            return row.measure(m);
        }
    }
    panic!("no row for {dims:?}");
}

#[tokio::test]
async fn declared_queries_produce_a_deterministic_fingerprint() {
    let backend = Arc::new(FakeBackend::new());
    let catalog = catalog();
    seed(&backend, &catalog).await;
    let source = StorageFingerprintSource::new(backend.clone(), catalog.clone());

    let first = spec()
        .measure(&source, &clone())
        .await
        .expect("first measurement");
    let second = spec()
        .measure(&source, &clone())
        .await
        .expect("second measurement");

    assert_eq!(first.digest(), second.digest(), "deterministic");
    assert_eq!(
        measure(
            &first,
            "trial_balance",
            &[("account", "cash"), ("company", "acme")],
            "debit"
        ),
        150
    );
    assert_eq!(
        measure(
            &first,
            "trial_balance",
            &[("account", "bank"), ("company", "acme")],
            "debit"
        ),
        200
    );
}

#[tokio::test]
async fn ageing_rows_are_bucketed_and_regrouped() {
    let backend = Arc::new(FakeBackend::new());
    let catalog = catalog();
    seed(&backend, &catalog).await;
    let source = StorageFingerprintSource::new(backend.clone(), catalog.clone());
    let fp = spec().measure(&source, &clone()).await.expect("measure");

    assert_eq!(
        measure(
            &fp,
            "ageing",
            &[("account", "cash"), ("bucket", "0-30")],
            "outstanding"
        ),
        100
    );
    assert_eq!(
        measure(
            &fp,
            "ageing",
            &[("account", "cash"), ("bucket", "31-60")],
            "outstanding"
        ),
        50
    );
    assert_eq!(
        measure(
            &fp,
            "ageing",
            &[("account", "bank"), ("bucket", "90+")],
            "outstanding"
        ),
        200
    );
    let section = fp.section("ageing").expect("ageing");
    assert!(section.dimensions.contains(&"bucket".to_owned()));
    assert!(!section.dimensions.contains(&"age_days".to_owned()));
}

#[tokio::test]
async fn a_non_integer_measure_is_refused() {
    let backend = Arc::new(FakeBackend::new());
    let catalog = catalog();
    seed(&backend, &catalog).await;
    let source = StorageFingerprintSource::new(backend.clone(), catalog.clone());

    let mut bad = trial_query();
    // Measure the text `account` column as if it were an integer.
    bad.select = vec![
        select(Expr::field(&n("t"), &n("account")), "account"),
        select(Expr::field(&n("t"), &n("account")), "debit"),
    ];
    let spec = FingerprintSpec::new(vec![FingerprintQuery {
        key: "trial_balance".to_owned(),
        title: "Trial balance".to_owned(),
        dimensions: vec!["account".to_owned()],
        measures: vec!["debit".to_owned()],
        bucket: None,
        query: bad,
    }]);

    let error = spec
        .measure(&source, &clone())
        .await
        .expect_err("text is not an integer");
    assert_eq!(error.code(), codes::FINGERPRINT_FAILED);
}

#[test]
fn a_bucket_with_the_wrong_label_count_is_refused() {
    let mut bad = spec();
    if let Some(bucket) = bad.sections[1].bucket.as_mut() {
        bucket.labels.pop();
    }
    let error = bad.validate().expect_err("labels must match bounds");
    assert_eq!(error.code(), codes::INVALID_INPUT);
}
