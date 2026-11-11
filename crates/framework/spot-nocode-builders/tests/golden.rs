//! A golden report: a sales-by-month style summary — a count and a sum grouped by a date and
//! a field reached through a link — compiles to the expected storage query IR and runs on the
//! storage fake.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use spot_nocode_builders::{
    Aggregate, Compare, Direction, DraftColumn, DraftFilter, DraftJoin, DraftParameter, DraftSort,
    FilterTarget, FilterValue, ReportDraft,
};
use spot_storage_contract::{
    CmpOp, Expr, Join, JoinKind, Nulls, Order, OrderDir, QueryPath, Selection, Source, Value,
};
use support::{n, principal, world};
use time::{Date, Month};

/// The golden draft: rows per month and account, with the debit summed and a date filter the
/// viewer supplies.
fn golden_draft() -> ReportDraft {
    let mut draft = ReportDraft::new(
        "draft-1",
        "gina",
        "sales_by_month",
        "Sales by month",
        "journal",
    );
    draft
        .joins
        .push(DraftJoin::new("acct", "account", "account"));
    draft.columns = vec![
        DraftColumn::new("date").with_label("Month"),
        DraftColumn::new("acct.title").with_label("Account"),
        DraftColumn::new("id")
            .aggregated(Aggregate::Count)
            .with_label("Rows"),
        DraftColumn::new("debit")
            .aggregated(Aggregate::Sum)
            .with_label("Debited")
            .with_format("money"),
    ];
    draft.group_by = vec!["date".to_owned(), "acct.title".to_owned()];
    draft.parameters = vec![
        DraftParameter::date("from", "From").defaulting(FilterValue::Date {
            text: "2026-01-01".to_owned(),
        }),
    ];
    draft.filters = vec![DraftFilter::new(
        "date",
        Compare::Ge,
        FilterTarget::parameter("from"),
    )];
    draft.sort = vec![DraftSort {
        field: "date".to_owned(),
        direction: Direction::Asc,
        nulls: None,
    }];
    draft.limit = Some(100);
    draft.max_rows = Some(1_000);
    draft.roles = vec![
        "reader".to_owned(),
        "masked".to_owned(),
        "analyst".to_owned(),
    ];
    draft
}

#[tokio::test]
async fn the_golden_report_compiles_to_the_expected_query() {
    let world = world().await;
    let draft = golden_draft();
    let plan = support::compile(&world, &draft).expect("the golden report compiles");
    let from = Date::from_calendar_date(2026, Month::January, 1).expect("a calendar date in range");
    let query = plan
        .compiled
        .build_query(&|binding: &str| (binding == "from").then_some(Value::Date(from)))
        .expect("the query builds");

    assert_eq!(
        query.from,
        Source {
            entity: n("journal"),
            alias: n("base"),
        }
    );
    assert_eq!(
        query.select,
        vec![
            Selection {
                expr: Expr::field(&n("base"), &n("date")),
                alias: n("date"),
            },
            Selection {
                expr: Expr::field(&n("acct"), &n("title")),
                alias: n("acct_title"),
            },
            Selection {
                expr: Expr::Agg {
                    func: spot_storage_contract::AggFunc::Count,
                    arg: Some(Box::new(Expr::field(&n("base"), &n("id")))),
                    distinct: false,
                },
                alias: n("id"),
            },
            Selection {
                expr: Expr::Agg {
                    func: spot_storage_contract::AggFunc::Sum,
                    arg: Some(Box::new(Expr::field(&n("base"), &n("debit")))),
                    distinct: false,
                },
                alias: n("debit"),
            },
        ]
    );
    assert_eq!(
        query.joins,
        vec![Join {
            kind: JoinKind::Left,
            source: Source {
                entity: n("account"),
                alias: n("acct"),
            },
            on: Expr::field(&n("base"), &n("account")).eq(Expr::field(&n("acct"), &n("id"))),
        }]
    );
    assert_eq!(
        query.filter,
        Some(Expr::field(&n("base"), &n("date")).cmp(CmpOp::Ge, Expr::Value(Value::Date(from))))
    );
    assert_eq!(
        query.group_by,
        vec![
            Expr::field(&n("base"), &n("date")),
            Expr::field(&n("acct"), &n("title")),
        ]
    );
    assert_eq!(
        query.order_by,
        vec![Order {
            expr: Expr::field(&n("base"), &n("date")),
            dir: OrderDir::Asc,
            nulls: Nulls::First,
        }]
    );
    assert_eq!(query.limit, None, "the runner sets the limit at run time");
    assert_eq!(query.path, QueryPath::Report);
}

#[tokio::test]
async fn the_golden_report_runs_on_the_storage_fake() {
    let world = world().await;
    let draft = golden_draft();
    let plan = support::compile(&world, &draft).expect("compiles");
    let args = support::args(json!({ "from": "2026-01-01" }));
    let run = world
        .runner()
        .run(&plan.compiled, &principal("gina"), &args, None)
        .await
        .expect("runs");

    assert_eq!(run.result.columns.len(), 4);
    assert_eq!(
        run.result.rows,
        vec![
            vec![
                json!("2026-01-15"),
                json!("acct-0"),
                json!(3),
                json!("9.00"),
            ],
            vec![
                json!("2026-02-15"),
                json!("acct-1"),
                json!(3),
                json!("12.00"),
            ],
        ]
    );
    assert!(!run.result.truncated);

    // The seeded totals agree with the stored rows, not just with the report.
    let january = Date::from_calendar_date(2026, Month::January, 15).expect("a date");
    assert_eq!(
        world.bucket(january, 0),
        (3, rust_decimal::Decimal::new(900, 2))
    );
}

#[tokio::test]
async fn a_missing_required_value_is_refused_at_run_time() {
    let world = world().await;
    let mut draft = golden_draft();
    draft.parameters[0].required = true;
    draft.parameters[0].default = None;
    let plan = support::compile(&world, &draft).expect("compiles");
    let err = world
        .runner()
        .run(
            &plan.compiled,
            &principal("gina"),
            &support::args(json!({})),
            None,
        )
        .await
        .expect_err("the value is required");
    assert_eq!(err.code(), "report.bad_argument");
    assert_eq!(err.path(), Some("from"));
}

#[tokio::test]
async fn an_unavailable_field_is_stated_in_plain_language() {
    let world = world().await;
    let mut draft = golden_draft();
    draft
        .columns
        .push(DraftColumn::new("acct.missing").aggregated(Aggregate::Count));
    let errs = support::compile(&world, &draft).expect_err("the field does not exist");
    assert!(
        errs.iter()
            .any(|i| i.code == "nocode_builders.unknown_field"),
        "{errs:?}"
    );
    assert!(
        errs.iter().all(|i| {
            let text = format!("{} {}", i.message, i.remedy);
            !text.contains("IR") && !text.contains("SQL")
        }),
        "no issue mentions the query language: {errs:?}"
    );
}
