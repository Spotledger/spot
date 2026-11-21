//! Security (WP-SEC-04.T13): filtering, ordering, grouping or aggregating on a protected
//! field needs the `report` right, not `read`. A principal who may read `memo` but not report
//! it (hank) gets no ordering, grouping or filter signal from it; a principal with both
//! rights (gina) runs the same queries.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use serde_json::json;
use spot_reports::{NoTelemetry, ReportArgs, ReportRunner};
use support::{FakeExecutor, compile, principals, world};

const HEAD: &str = "name = \"q\"\nlabel = \"Q\"\nbase = \"journal\"\n\
                    roles = [\"reader\", \"owner_reader\", \"masked\", \"analyst\", \"readonly\"]\n";

fn plain_with(extra: &str) -> String {
    format!("{HEAD}[[columns]]\nfield = \"title\"\n{extra}")
}

fn queries() -> Vec<(String, String)> {
    let mut out = vec![
        (
            "filter".to_owned(),
            plain_with(
                "[[parameters]]\nname = \"m\"\nlabel = \"M\"\ntype = \"text\"\nrequired = true\n\
                 [filters]\nfield = \"memo\"\nop = \"eq\"\nparam = \"m\"\n",
            ),
        ),
        (
            "order".to_owned(),
            plain_with("[[order_by]]\nfield = \"memo\"\n"),
        ),
        (
            "group".to_owned(),
            format!(
                "{HEAD}group_by = [\"memo\"]\n[[columns]]\nfield = \"memo\"\n\
                 [[columns]]\nfield = \"debit\"\naggregate = \"sum\"\n"
            ),
        ),
    ];
    for agg in ["count", "min", "max"] {
        out.push((
            format!("aggregate {agg}"),
            format!(
                "{HEAD}group_by = [\"title\"]\n[[columns]]\nfield = \"title\"\n\
                 [[columns]]\nfield = \"memo\"\naggregate = \"{agg}\"\n"
            ),
        ));
    }
    out
}

fn runner(world: &support::World) -> ReportRunner {
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    ReportRunner::new(
        world.engine.clone(),
        world.guard.clone(),
        executor,
        Arc::new(NoTelemetry),
    )
}

fn who(name: &str) -> spot_permissions::Principal {
    principals()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, p)| p)
        .expect("known principal")
}

#[tokio::test]
async fn read_without_report_is_refused_on_every_query_use() {
    let w = world().await;
    for (what, toml) in queries() {
        let args = if what == "filter" {
            ReportArgs::from_json(&json!({"m": "memo-0000"})).unwrap()
        } else {
            ReportArgs::empty()
        };
        let report = compile(&w, &toml).unwrap_or_else(|e| panic!("{what}: {e:?}"));
        let err = runner(&w)
            .run(&report, &who("hank"), &args, None)
            .await
            .expect_err(&what);
        assert_eq!(err.code(), "permission.field_denied", "{what}");
        let text = format!("{err:?} {err}");
        assert!(!text.contains("memo-0"), "{what}: a value in the refusal");
    }
}

#[tokio::test]
async fn read_with_report_runs_the_same_queries() {
    let w = world().await;
    for (what, toml) in queries() {
        let args = if what == "filter" {
            ReportArgs::from_json(&json!({"m": "memo-0000"})).unwrap()
        } else {
            ReportArgs::empty()
        };
        let report = compile(&w, &toml).unwrap_or_else(|e| panic!("{what}: {e:?}"));
        runner(&w)
            .run(&report, &who("gina"), &args, None)
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
    }
}
