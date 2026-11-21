//! Parity of the PostgreSQL query path with the in-memory fake: the same seeded data and the
//! same 200 random valid queries give identical `Rows` (or the same error kind) from the fake,
//! from PostgreSQL inside a transaction and from PostgreSQL through `read`.
//!
//! The seed has nulls in every nullable column, duplicate non-null values (ties), text that
//! differs only by case, `%`, `_` and non-ASCII characters, and negative numbers. The generator
//! covers three-valued logic (`NOT`, `OR`, `IN`, comparisons with null columns), null ordering,
//! collation ties, aggregates over empty sets, grouping with null keys, joins and paging.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::contract_tests::catalog;
use spot_storage_contract::{
    AggFunc, ArithOp, Catalog, ChildRow, CmpOp, DocumentData, DocumentStore, ErrorKind, Expr,
    Fields, Isolation, Join, JoinKind, Name, Nulls, Order, OrderDir, Query, QueryExecutor,
    QueryPath, ReadSource, RecordId, Rows, Selection, StorageBackend, StorageResult, TextMatch,
    TextMode, Transaction, Value,
};
use spot_storage_fake::FakeBackend;
use time::{Date, Month, OffsetDateTime, Time};

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

/// xorshift64*: a fixed seed makes every run generate the same queries.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const TEXTS: &[&str] = &[
    "alpha",
    "Alpha",
    "ALPHA",
    "beta",
    "Beta",
    "gamma",
    "a%",
    "a_",
    "100%",
    "x_y",
    "x%y",
    "zeta",
    "Zeta",
    "delta",
    "b",
    "B",
    "ab",
    "Ab",
    "aB",
    "é",
    "e",
    "f",
    "10",
    "9",
    "2",
    "résumé",
    "resume",
    "mid dle",
    "mid-dle",
    "mid_dle",
    "back\\slash",
    "",
];

fn date(i: usize) -> Date {
    Date::from_calendar_date(
        2020 + (i % 4) as i32,
        Month::try_from(1 + (i % 12) as u8).unwrap(),
        1 + (i % 28) as u8,
    )
    .unwrap()
}

fn seed_parties() -> Vec<DocumentData> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut docs: Vec<DocumentData> = Vec::new();
    for (i, name) in TEXTS.iter().enumerate() {
        let mut fields = Fields::new();
        fields.insert(n("name"), Value::Text((*name).to_owned()));
        // Cents repeat on purpose; some are negative.
        let cents = [0, 100, 1_050, 1_050, 99_999, -250, 12_345, 7][rng.below(8)];
        fields.insert(n("credit_limit"), Value::Decimal(Decimal::new(cents, 2)));
        let parent = if i > 0 && rng.chance(40) {
            Value::Id(docs[rng.below(i)].id)
        } else {
            Value::Null
        };
        fields.insert(n("parent_party"), parent);
        let notes = [
            Value::Null,
            Value::Text("n1".into()),
            Value::Text("N1".into()),
            Value::Text("n2".into()),
            Value::Text(String::new()),
        ];
        fields.insert(n("note"), rng.pick(&notes).clone());
        let since = if rng.chance(30) {
            Value::Null
        } else {
            Value::Date(date(rng.below(6)))
        };
        fields.insert(n("since"), since);
        let opens = if rng.chance(30) {
            Value::Null
        } else {
            Value::Time(Time::from_hms(rng.below(24) as u8, 0, 0).unwrap())
        };
        fields.insert(n("opens_at"), opens);
        let seen = if rng.chance(30) {
            Value::Null
        } else {
            Value::DateTime(
                OffsetDateTime::from_unix_timestamp(1_600_000_000 + 86_400 * rng.below(5) as i64)
                    .unwrap(),
            )
        };
        fields.insert(n("seen_at"), seen);
        let active = [Value::Null, Value::Bool(true), Value::Bool(false)];
        fields.insert(n("active"), rng.pick(&active).clone());
        let visits = if rng.chance(25) {
            Value::Null
        } else {
            Value::Int(rng.below(7) as i64 - 2)
        };
        fields.insert(n("visits"), visits);
        fields.insert(n("profile"), Value::Null);
        let logo = if rng.chance(50) {
            Value::Null
        } else {
            Value::Bytes(vec![rng.below(3) as u8])
        };
        fields.insert(n("logo"), logo);
        docs.push(DocumentData {
            entity: n("party"),
            id: RecordId::new_v7(),
            fields,
            children: BTreeMap::new(),
        });
    }
    docs
}

fn seed_invoices(parties: &[DocumentData]) -> Vec<DocumentData> {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    (0..30)
        .map(|i| {
            let mut fields = Fields::new();
            fields.insert(n("number"), Value::Text(format!("INV-{i:03}")));
            // Parties 20..32 stay without invoices, for left joins from `party`.
            fields.insert(n("party"), Value::Id(parties[rng.below(20)].id));
            fields.insert(
                n("total"),
                Value::Decimal(Decimal::new(rng.below(5) as i64 * 250, 2)),
            );
            fields.insert(n("ref_entity"), Value::Null);
            fields.insert(n("ref_id"), Value::Null);
            DocumentData {
                entity: n("invoice"),
                id: RecordId::new_v7(),
                fields,
                children: BTreeMap::<Name, Vec<ChildRow>>::new(),
            }
        })
        .collect()
}

async fn seed<B: StorageBackend>(b: &B, cat: &Arc<Catalog>, docs: &[DocumentData]) {
    b.prepare(cat).await.unwrap();
    let options = common::options("seed", Isolation::ReadCommitted);
    let mut tx = b.begin(cat.clone(), options).await.unwrap();
    for d in docs {
        tx.insert(d).await.unwrap();
    }
    tx.commit().await.unwrap();
}

// --- the generator ------------------------------------------------------------------------------

fn f(alias: &str, field: &str) -> Expr {
    Expr::field(&n(alias), &n(field))
}

fn lit(v: impl Into<Value>) -> Expr {
    Expr::value(v)
}

fn cmp_op(rng: &mut Rng) -> CmpOp {
    *rng.pick(&[
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ])
}

fn text_lit(rng: &mut Rng) -> String {
    (*rng.pick(TEXTS)).to_owned()
}

fn leaf(rng: &mut Rng, a: &str) -> Expr {
    match rng.below(11) {
        0 => f(a, "visits").cmp(cmp_op(rng), lit(rng.below(7) as i64 - 2)),
        1 => f(a, "credit_limit").cmp(
            cmp_op(rng),
            lit(Decimal::new([0, 100, 1_050, -250][rng.below(4)], 2)),
        ),
        2 => f(a, "credit_limit").cmp(cmp_op(rng), lit(rng.below(3) as i64)),
        3 => f(a, "name").cmp(cmp_op(rng), lit(text_lit(rng))),
        4 => f(a, "note").cmp(cmp_op(rng), lit(["n1", "N1", "", "n2"][rng.below(4)])),
        5 => f(a, "since").cmp(cmp_op(rng), Expr::Value(Value::Date(date(rng.below(6))))),
        6 => f(a, "active").eq(lit(rng.chance(50))),
        7 => {
            let field = *rng.pick(&[
                "visits",
                "note",
                "since",
                "active",
                "parent_party",
                "logo",
                "seen_at",
                "opens_at",
            ]);
            Expr::IsNull(Box::new(f(a, field)))
        }
        8 => {
            let values: Vec<Value> = (0..rng.below(4))
                .map(|_| Value::Text(text_lit(rng)))
                .collect();
            Expr::In(Box::new(f(a, "name")), values)
        }
        9 => {
            let field = *rng.pick(&["name", "note"]);
            Expr::Text(
                Box::new(f(a, field)),
                TextMatch {
                    mode: *rng.pick(&[
                        TextMode::Equals,
                        TextMode::Prefix,
                        TextMode::Contains,
                        TextMode::Suffix,
                    ]),
                    pattern: ["a", "A", "%", "_", "x", "n", "e", "", "\\"][rng.below(9)].to_owned(),
                    case_insensitive: rng.chance(50),
                },
            )
        }
        _ => {
            let sum = Expr::Arith(
                Box::new(f(a, "credit_limit")),
                *rng.pick(&[ArithOp::Add, ArithOp::Sub, ArithOp::Mul]),
                Box::new(f(a, "visits")),
            );
            sum.cmp(cmp_op(rng), lit(Decimal::new(rng.below(300) as i64, 2)))
        }
    }
}

fn condition(rng: &mut Rng, a: &str, depth: usize) -> Expr {
    if depth == 0 || rng.chance(45) {
        return leaf(rng, a);
    }
    match rng.below(3) {
        0 => Expr::And(
            (0..rng.below(4))
                .map(|_| condition(rng, a, depth - 1))
                .collect(),
        ),
        1 => Expr::Or(
            (0..rng.below(4))
                .map(|_| condition(rng, a, depth - 1))
                .collect(),
        ),
        _ => Expr::Not(Box::new(condition(rng, a, depth - 1))),
    }
}

fn sel(expr: Expr, alias: &str) -> Selection {
    Selection {
        expr,
        alias: n(alias),
    }
}

fn order(expr: Expr, rng: &mut Rng) -> Order {
    Order {
        expr,
        dir: *rng.pick(&[OrderDir::Asc, OrderDir::Desc]),
        nulls: *rng.pick(&[Nulls::First, Nulls::Last]),
    }
}

/// The last sort key of every non-aggregate query: the id, so ties on the random keys do not leave
/// the row order to each backend (PostgreSQL's physical order differs from the fake's).
fn tiebreak(alias: &str) -> Order {
    Order {
        expr: f(alias, "id"),
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    }
}

const SORTABLE: &[&str] = &[
    "name",
    "visits",
    "credit_limit",
    "note",
    "since",
    "seen_at",
    "active",
    "opens_at",
];
const PLAIN: &[&str] = &[
    "name",
    "credit_limit",
    "parent_party",
    "note",
    "since",
    "opens_at",
    "seen_at",
    "active",
    "visits",
    "logo",
];

fn plain(rng: &mut Rng) -> Query {
    let mut q = Query::from(&n("party"), &n("p"), QueryPath::Report);
    q.select = vec![sel(f("p", "id"), "id")];
    for (i, field) in (0..1 + rng.below(4)).map(|i| (i, *rng.pick(PLAIN))) {
        q.select.push(sel(f("p", field), &format!("c{i}")));
    }
    if rng.chance(75) {
        q.filter = Some(condition(rng, "p", 3));
    }
    for _ in 0..rng.below(3) {
        q.order_by.push(order(f("p", rng.pick(SORTABLE)), rng));
    }
    q.order_by.push(tiebreak("p"));
    if rng.chance(60) {
        q.limit = Some(1 + rng.below(15) as u32);
    }
    if rng.chance(50) {
        q.offset = rng.below(12) as u64;
    }
    q
}

fn aggregate(rng: &mut Rng) -> Query {
    let mut q = Query::from(&n("party"), &n("p"), QueryPath::Report);
    let key = rng
        .chance(70)
        .then(|| *rng.pick(&["active", "visits", "since", "note", "name"]));
    if let Some(key) = key {
        q.group_by = vec![f("p", key)];
        q.select.push(sel(f("p", key), "k"));
        q.order_by.push(order(f("p", key), rng));
    }
    q.select.push(sel(
        Expr::Agg {
            func: AggFunc::Count,
            arg: None,
            distinct: false,
        },
        "n",
    ));
    let counted = *rng.pick(&["visits", "note", "since", "active"]);
    q.select.push(sel(
        Expr::Agg {
            func: AggFunc::Count,
            arg: Some(Box::new(f("p", counted))),
            distinct: rng.chance(50),
        },
        "cd",
    ));
    q.select.push(sel(
        Expr::Agg {
            func: AggFunc::Sum,
            arg: Some(Box::new(f("p", "credit_limit"))),
            distinct: false,
        },
        "total",
    ));
    q.select.push(sel(
        Expr::Agg {
            func: AggFunc::Sum,
            arg: Some(Box::new(f("p", "visits"))),
            distinct: rng.chance(30),
        },
        "vsum",
    ));
    for (i, field) in ["name", "since", "credit_limit", "active", "note"]
        .iter()
        .enumerate()
    {
        if rng.chance(50) {
            let func = *rng.pick(&[AggFunc::Min, AggFunc::Max]);
            q.select.push(sel(
                Expr::Agg {
                    func,
                    arg: Some(Box::new(f("p", field))),
                    distinct: false,
                },
                &format!("m{i}"),
            ));
        }
    }
    if rng.chance(70) {
        // Frequently false for every row, so the aggregate runs over an empty set.
        q.filter = Some(condition(rng, "p", 2));
    }
    if key.is_some() && rng.chance(35) {
        q.having = Some(
            Expr::Agg {
                func: AggFunc::Count,
                arg: None,
                distinct: false,
            }
            .cmp(CmpOp::Gt, lit(rng.below(3) as i64)),
        );
    }
    q
}

fn joined(rng: &mut Rng) -> Query {
    let left_from_party = rng.chance(50);
    let (from, other, on) = if left_from_party {
        ("party", "invoice", f("i", "party").eq(f("p", "id")))
    } else {
        ("invoice", "party", f("i", "party").eq(f("p", "id")))
    };
    let (from_alias, other_alias) = if left_from_party {
        ("p", "i")
    } else {
        ("i", "p")
    };
    let mut q = Query::from(&n(from), &n(from_alias), QueryPath::Report);
    q.joins = vec![Join {
        kind: *rng.pick(&[JoinKind::Inner, JoinKind::Left]),
        source: spot_storage_contract::Source {
            entity: n(other),
            alias: n(other_alias),
        },
        on,
    }];
    q.select = vec![
        sel(f(from_alias, "id"), "id"),
        sel(f("p", "name"), "name"),
        sel(f("i", "number"), "number"),
        sel(f("i", "total"), "total"),
    ];
    if rng.chance(60) {
        q.filter = Some(condition(rng, "p", 2));
    }
    if rng.chance(50) {
        q.order_by.push(order(f("i", "total"), rng));
    }
    q.order_by.push(tiebreak(from_alias));
    q.order_by.push(tiebreak(other_alias));
    if rng.chance(40) {
        q.limit = Some(1 + rng.below(20) as u32);
        q.offset = rng.below(5) as u64;
    }
    q
}

fn generate(rng: &mut Rng) -> Query {
    match rng.below(10) {
        0..=5 => plain(rng),
        6..=7 => aggregate(rng),
        _ => joined(rng),
    }
}

// --- the test -----------------------------------------------------------------------------------

async fn in_tx(b: &PgBackend, cat: &Arc<Catalog>, q: &Query) -> StorageResult<Rows> {
    let mut tx = b
        .begin(
            cat.clone(),
            common::options("parity", Isolation::ReadCommitted),
        )
        .await?;
    let result = tx.query(q).await;
    tx.rollback().await.unwrap();
    result
}

#[tokio::test]
async fn random_queries_match_the_fake() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pg = PgBackend::connect(db.config()).await.unwrap();
    let fake = FakeBackend::new();
    let cat = Arc::new(catalog().unwrap());
    let parties = seed_parties();
    let mut docs = parties.clone();
    docs.extend(seed_invoices(&parties));
    seed(&pg, &cat, &docs).await;
    seed(&fake, &cat, &docs).await;

    // The fake's query engine is WP-CON-01.T3. Until it is merged the fake answers `Unsupported`;
    // the comparison with the fake is then skipped (loudly) and the two PostgreSQL paths are still
    // compared with each other. The test starts comparing with the fake the day T3 lands.
    let probe = fake
        .read(cat.clone(), ReadSource::Primary, &plain(&mut Rng(1)))
        .await;
    let fake_ready = !matches!(&probe, Err(e) if e.kind == ErrorKind::Unsupported);
    if !fake_ready {
        println!("SKIPPED parity with the fake: its query engine (WP-CON-01.T3) is not merged yet");
    }

    let mut rng = Rng(0x5EED_0FB0_B5EE_D0F0);
    let (mut compared, mut errors, mut non_empty) = (0, 0, 0);
    for round in 0..200 {
        let q = generate(&mut rng);
        let via_tx = in_tx(&pg, &cat, &q).await;
        let via_read = pg.read(cat.clone(), ReadSource::Primary, &q).await;
        match (&via_tx, &via_read) {
            (Ok(t), Ok(r)) => {
                assert_eq!(
                    t, r,
                    "round {round}: the two PostgreSQL paths differ: {q:#?}"
                );
                compared += 1;
                non_empty += usize::from(!t.rows.is_empty());
            }
            (Err(t), Err(r)) => {
                assert_eq!(t.kind, r.kind, "round {round}: {q:#?}");
                errors += 1;
            }
            _ => panic!("round {round}: tx {via_tx:?}, read {via_read:?}\n{q:#?}"),
        }
        if fake_ready {
            let want = fake.read(cat.clone(), ReadSource::Primary, &q).await;
            match (&want, &via_tx) {
                (Ok(w), Ok(t)) => assert_eq!(
                    w, t,
                    "round {round}, postgres differs from the fake: {q:#?}"
                ),
                (Err(w), Err(t)) => {
                    assert_eq!(
                        w.kind, t.kind,
                        "round {round}: {q:#?}\nfake: {w}\npostgres: {t}"
                    );
                }
                _ => panic!("round {round}: fake {want:?}, postgres {via_tx:?}\n{q:#?}"),
            }
        }
    }
    // The generator must mostly produce queries that run, and many that return rows.
    assert!(
        compared >= 150,
        "only {compared} of 200 queries ran ({errors} errors)"
    );
    assert!(non_empty >= 100, "only {non_empty} queries returned rows");
}
