//! System paths (ADR-070 section 12 rule 4): jobs, sync and migrations act as the platform and
//! may read protected fields when their manifest declares the group. The audit and trace
//! records of that access carry names, never values, and a job that runs an export **as a
//! user** gets that user's masked form, whatever its manifest declares.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bulk_data::Format;
use spot_field_guard::leakcheck::{Output, assert_no_leak, find_leaks};
use spot_field_guard::{ActorManifest, AuditRecord, Via};
use spot_permissions::Principal;

fn records_text(core: &Core) -> Vec<Output> {
    let trace: Vec<String> = core
        .trace
        .drain()
        .iter()
        .map(|r| format!("{r:?}"))
        .collect();
    let audit: Vec<String> = core
        .audit
        .drain()
        .iter()
        .map(|r| format!("{r:?}"))
        .collect();
    vec![
        Output::Text(trace.join("\n")),
        Output::Text(audit.join("\n")),
    ]
}

#[tokio::test]
async fn a_job_reads_a_declared_group_and_its_records_carry_no_value() {
    let core = core();
    let corpus = corpus();
    let store = store(&core.meta, &corpus, 2).await;
    let job = ActorManifest::new("payroll_export", &["hr", "finance"]);

    // The job declares the access, then reads as the platform.
    for field in ["salary", "annual_cost"] {
        core.guard.system_access(&job, DOCTYPE, field).unwrap();
    }
    let system = Principal::system();
    let read = run_export(
        &core,
        &store,
        &system,
        Format::Csv,
        &["id", "salary", "annual_cost"],
        &[],
    )
    .await
    .unwrap();
    // The positive control: the system read really returns the protected values.
    let seen: Vec<String> = find_leaks(&corpus.only(&["salary", "annual"]), &[Output::Csv(read)])
        .into_iter()
        .map(|l| l.secret)
        .collect();
    assert!(seen.iter().any(|s| s == "salary") && seen.iter().any(|s| s == "annual"));

    // `hr` is a trace group: the access is traced as the system, without the value.
    let trace = core.trace.drain();
    assert!(
        trace
            .iter()
            .all(|r| r.via == Via::System && r.authorised && r.who == "payroll_export")
    );
    assert!(!trace.is_empty());
    let text = format!("{trace:?}");
    assert_no_leak(
        &corpus,
        &[
            Output::Text(text),
            Output::Text(format!("{:?}", core.audit.drain())),
        ],
    );
}

#[tokio::test]
async fn an_undeclared_group_is_refused_and_the_audit_record_names_no_value() {
    let core = core();
    let corpus = corpus();
    let job = ActorManifest::new("nightly_sync", &["hr"]);
    for field in [
        "card_number",
        "holder",
        "profile_json",
        "items.iban",
        "items.memo",
    ] {
        let err = core.guard.system_access(&job, DOCTYPE, field).unwrap_err();
        assert_eq!(err.code(), "fieldguard.undeclared_system_access", "{field}");
        assert_no_leak(&corpus, &[Output::Text(format!("{} {}", err.code(), err))]);
    }
    let audit = core.audit.drain();
    assert_eq!(audit.len(), 5);
    assert!(audit
        .iter()
        .all(|r| matches!(r, AuditRecord::UndeclaredSystemAccess { actor, .. } if actor == "nightly_sync")));
    assert_no_leak(&corpus, &[Output::Text(format!("{audit:?}"))]);
}

#[tokio::test]
async fn a_job_that_exports_as_the_user_gets_the_users_masked_form() {
    let core = core();
    let corpus = corpus();
    let store = store(&core.meta, &corpus, 2).await;
    // The job declares every group for its own system reads; that is its manifest, not the
    // user's rights. The export runs as the user.
    let job = ActorManifest::new("scheduled_export", &["pci", "hr", "finance", "bank"]);
    for field in ["card_number", "salary"] {
        core.guard.system_access(&job, DOCTYPE, field).unwrap();
    }
    let _ = core.trace.drain();
    let columns = ["id", "card_number", "holder", "salary"];
    let masked = run_export(
        &core,
        &store,
        &principal("masked_reader"),
        Format::Csv,
        &columns,
        &["iban"],
    )
    .await
    .unwrap();
    assert!(masked.contains("***"), "the masked form is exported");
    assert_no_leak(&corpus, &[Output::Csv(masked)]);

    // And a user with no report right on the group is refused, not served by the job's
    // manifest.
    let refused = run_export(
        &core,
        &store,
        &principal("read_no_report"),
        Format::Csv,
        &columns,
        &[],
    )
    .await
    .unwrap_err();
    assert_no_leak(
        &corpus,
        &[Output::Text(format!(
            "{} {}",
            refused.code(),
            refused.message()
        ))],
    );
    assert_no_leak(&corpus, &records_text(&core));
}
