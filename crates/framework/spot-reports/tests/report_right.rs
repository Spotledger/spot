//! Security: the `report` right, not `read`, decides whether a protected field leaves in a
//! report (ADR-070 section 12, rule 6; `Channel::Report` needs `report`). A principal with the
//! field-level `read` right but no `report` right must never receive the value, in a plain
//! column, an aggregate, a computed column or a group-by. Fail closed on a guard error.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_field_guard::{DocProtection, FieldGuard, MetaProtection, ProtectionSource};
use spot_permissions::{MaskFormat, Principal};
use spot_reports::{NoTelemetry, ReportArgs, ReportRunner, RunOutput};
use support::{FakeExecutor, compile, principals, world};

const ROLES: &str = r#"roles = ["reader", "owner_reader", "masked", "analyst", "readonly"]"#;

fn plain() -> String {
    format!(
        "name = \"p\"\nlabel = \"P\"\nbase = \"journal\"\n{ROLES}\n\
         [[columns]]\nfield = \"title\"\n[[columns]]\nfield = \"memo\"\n\
         [[order_by]]\nfield = \"title\"\n"
    )
}

fn aggregate() -> String {
    format!(
        "name = \"a\"\nlabel = \"A\"\nbase = \"journal\"\n{ROLES}\ngroup_by = [\"title\"]\n\
         [[columns]]\nfield = \"title\"\n\
         [[columns]]\nfield = \"memo\"\naggregate = \"max\"\n\
         [[order_by]]\nfield = \"title\"\n"
    )
}

fn computed() -> String {
    format!(
        "name = \"c\"\nlabel = \"C\"\nbase = \"journal\"\n{ROLES}\n\
         [[columns]]\nfield = \"title\"\n[[columns]]\nfield = \"memo\"\n\
         [[computed]]\nname = \"copy\"\ntype = \"text\"\nexpr = \"doc.memo\"\n\
         [[order_by]]\nfield = \"title\"\n"
    )
}

fn group_by() -> String {
    format!(
        "name = \"g\"\nlabel = \"G\"\nbase = \"journal\"\n{ROLES}\ngroup_by = [\"memo\"]\n\
         [[columns]]\nfield = \"memo\"\n\
         [[columns]]\nfield = \"debit\"\naggregate = \"sum\"\n\
         [[order_by]]\nfield = \"memo\"\n"
    )
}

fn runner_with(world: &support::World, guard: FieldGuard) -> ReportRunner {
    let executor = Arc::new(FakeExecutor {
        backend: world.backend.clone(),
        catalog: world.catalog.clone(),
    });
    ReportRunner::new(world.engine.clone(), guard, executor, Arc::new(NoTelemetry))
}

async fn run(world: &support::World, toml: &str, who: &Principal) -> RunOutput {
    let report = compile(world, toml).expect("compiles");
    runner_with(world, world.guard.clone())
        .run(&report, who, &ReportArgs::empty(), None)
        .await
        .expect("runs")
}

fn principal(name: &str) -> Principal {
    principals()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, p)| p)
        .expect("known principal")
}

fn has_column(out: &RunOutput, name: &str) -> bool {
    out.result.columns.iter().any(|c| c.name == name)
}

fn removed_warning(out: &RunOutput) -> bool {
    out.messages
        .iter()
        .any(|m| m.code == "report.columns_removed")
}

fn dump(out: &RunOutput) -> String {
    format!(
        "{}{:?}",
        serde_json::to_string(&out.result.rows).unwrap(),
        out.result
            .columns
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    )
}

#[tokio::test]
async fn read_without_report_loses_a_plain_column() {
    let w = world().await;
    let out = run(&w, &plain(), &principal("hank")).await;
    assert!(has_column(&out, "title"));
    assert!(!has_column(&out, "memo"));
    assert!(removed_warning(&out));
    assert!(!dump(&out).contains("memo-0"));
}

/// Since WP-SEC-04.T13 a query that aggregates or groups by a field the principal may read
/// but not report is refused outright (it used to lose only the column): the result's
/// grouping or range would otherwise leak the field.
async fn refused(world: &support::World, toml: &str, who: &Principal) -> String {
    let report = compile(world, toml).expect("compiles");
    let err = runner_with(world, world.guard.clone())
        .run(&report, who, &ReportArgs::empty(), None)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), "permission.field_denied");
    format!("{err:?}{err}")
}

#[tokio::test]
async fn read_without_report_is_refused_an_aggregate() {
    let w = world().await;
    let text = refused(&w, &aggregate(), &principal("hank")).await;
    assert!(!text.contains("memo-0"));
}

#[tokio::test]
async fn read_without_report_loses_a_computed_column_over_a_protected_input() {
    let w = world().await;
    let out = run(&w, &computed(), &principal("hank")).await;
    assert!(!has_column(&out, "memo"));
    assert!(!has_column(&out, "copy"));
    assert!(removed_warning(&out));
    assert!(!dump(&out).contains("memo-0"));
}

#[tokio::test]
async fn read_without_report_is_refused_a_group_by() {
    let w = world().await;
    let text = refused(&w, &group_by(), &principal("hank")).await;
    assert!(!text.contains("memo-0"));
}

#[tokio::test]
async fn read_with_report_gets_the_value() {
    let w = world().await;
    let who = principal("gina");
    for toml in [plain(), computed(), group_by(), aggregate()] {
        let out = run(&w, &toml, &who).await;
        assert!(has_column(&out, "memo"));
        assert!(
            dump(&out).contains("memo-0"),
            "value reaches a principal with report"
        );
    }
    let out = run(&w, &computed(), &who).await;
    assert!(has_column(&out, "copy"));
}

#[tokio::test]
async fn read_masked_with_report_gets_the_masked_form() {
    let w = world().await;
    let out = run(&w, &plain(), &principal("erin")).await;
    assert!(has_column(&out, "memo"));
    let expected = spot_permissions::mask("memo-0000", &MaskFormat::LastChars(2));
    assert_eq!(out.result.rows[0][1].as_str(), Some(expected.as_str()));
    assert!(!dump(&out).contains("memo-0"));
}

/// A protection source that knows no DocType, so the guard errors on every column.
struct Unknown;

impl ProtectionSource for Unknown {
    fn protection(&self, _doctype: &str) -> Option<Arc<DocProtection>> {
        None
    }
}

#[tokio::test]
async fn a_guard_error_removes_the_column_even_for_a_principal_who_may_report() {
    let w = world().await;
    let guard = FieldGuard::new(
        w.engine.clone(),
        Arc::new(Unknown),
        Arc::new(spot_field_guard::BoundedQueue::new(16)),
        Arc::new(spot_field_guard::BoundedQueue::new(16)),
        Arc::new(spot_core::ManualClock::new(
            spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
        )),
    );
    // No `order_by`: a sort would be refused outright (also fail closed) before the columns.
    let toml = plain().replace("[[order_by]]\nfield = \"title\"\n", "");
    let report = compile(&w, &toml).expect("compiles");
    let out = runner_with(&w, guard)
        .run(&report, &principal("gina"), &ReportArgs::empty(), None)
        .await
        .expect("runs");
    assert!(!has_column(&out, "memo"));
    assert!(removed_warning(&out));
    assert!(!dump(&out).contains("memo-0"));
}

/// No cell of any result carries the planted value unless the principal may report it.
#[tokio::test]
async fn no_principal_receives_a_value_it_may_not_report() {
    let w = world().await;
    let _ = MetaProtection::new(&w.meta);
    for (name, who) in principals() {
        let may_report = matches!(name, "gina" | "system");
        for toml in [plain(), aggregate(), computed(), group_by()] {
            let report = compile(&w, &toml).expect("compiles");
            let result = runner_with(&w, w.guard.clone())
                .run(&report, &who, &ReportArgs::empty(), None)
                .await;
            match result {
                Ok(out) if !may_report => {
                    assert!(!dump(&out).contains("memo-0"), "{name} received a memo");
                }
                Ok(_) => {}
                // A query on a field the caller cannot read unmasked is refused (rule 7).
                Err(e) => assert_eq!(e.code(), "permission.field_denied", "{name}: {e}"),
            }
        }
    }
}
