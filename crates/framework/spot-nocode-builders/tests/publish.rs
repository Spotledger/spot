//! Publish: the `designer.publish` permission is required, the provenance is recorded for
//! bundling, and the published artifact reaches the sink.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_nocode_builders::{
    ApprovalTemplate, Approver, DashboardDraft, Designer, DraftColumn, MemorySink,
    PUBLISH_PERMISSION, Place, ReportDraft, Tile, build, compile_dashboard, publish_dashboard,
    publish_process, publish_report,
};
use spot_reports::ReportRegistry;
use support::{World, world};

fn draft() -> ReportDraft {
    let mut draft = ReportDraft::new("draft-pub", "gina", "journals", "Journals", "journal");
    draft.columns = vec![DraftColumn::new("title").with_label("Journal")];
    draft.limit = Some(100);
    draft.roles = vec!["reader".to_owned()];
    draft
}

fn designer() -> Designer {
    Designer::new("gina").with_permission(PUBLISH_PERMISSION)
}

#[tokio::test]
async fn publishing_requires_the_permission() {
    let world = world().await;
    let plan = support::compile(&world, &draft()).expect("compiles");
    let sink = MemorySink::new();
    let issues = publish_report(
        &plan,
        &draft(),
        &Designer::new("gina"),
        "2026-10-20T00:00:00Z",
        &sink,
    )
    .expect_err("no publish permission");
    assert!(
        issues
            .iter()
            .any(|i| i.code == "nocode_builders.publish_forbidden"),
        "{issues:?}"
    );
    assert_eq!(sink.reports(), 0);
}

#[tokio::test]
async fn publishing_records_provenance_and_a_stable_digest() {
    let world = world().await;
    let draft = draft();
    let plan = support::compile(&world, &draft).expect("compiles");
    let sink = MemorySink::new();
    let release = publish_report(&plan, &draft, &designer(), "2026-10-20T00:00:00Z", &sink)
        .expect("publishes");
    assert_eq!(release.name, "journals");
    assert_eq!(release.definition.base, "journal");
    assert_eq!(release.provenance.kind, "report");
    assert_eq!(release.provenance.designer, "gina");
    assert_eq!(release.provenance.draft_id, "draft-pub");
    assert_eq!(release.provenance.created_at, "2026-10-20T00:00:00Z");
    assert_eq!(release.provenance.digest.len(), 64);
    assert_eq!(sink.reports(), 1);
    assert_eq!(
        sink.last_report()
            .expect("the stored release")
            .provenance
            .digest,
        release.provenance.digest
    );

    // The same content, designer and time give the same digest; a different time does not.
    let again = publish_report(&plan, &draft, &designer(), "2026-10-20T00:00:00Z", &sink)
        .expect("publishes again");
    assert_eq!(again.provenance.digest, release.provenance.digest);
    let later = publish_report(&plan, &draft, &designer(), "2026-10-21T00:00:00Z", &sink)
        .expect("publishes later");
    assert_ne!(later.provenance.digest, release.provenance.digest);
}

fn dashboard_registry(world: &World) -> ReportRegistry {
    let set = world.meta_set();
    let plan = support::compile(world, &draft()).expect("compiles");
    let mut registry = ReportRegistry::new();
    registry
        .register(
            "nocode",
            Arc::new(plan.definition),
            &support::report_compiler(world, &set),
        )
        .expect("registers");
    registry
}

#[tokio::test]
async fn a_dashboard_publishes_with_its_provenance() {
    let world = world().await;
    let registry = dashboard_registry(&world);
    let mut draft = DashboardDraft::new("dash-pub", "gina", "journals_board", "Journals board");
    draft.roles = vec!["reader".to_owned()];
    draft
        .add_tile(Tile::list(
            "recent",
            "Recent",
            "journals",
            &["title"],
            Place::new(1, 1, 6, 2),
        ))
        .expect("fits");
    let def = compile_dashboard(&draft, &registry).expect("compiles");
    let sink = MemorySink::new();
    let release = publish_dashboard(&def, &draft, &designer(), "2026-10-20T00:00:00Z", &sink)
        .expect("publishes");
    assert_eq!(release.definition.schema, 1);
    assert_eq!(release.provenance.kind, "dashboard");
    assert_eq!(
        sink.last_dashboard()
            .expect("stored")
            .definition
            .tiles
            .len(),
        1
    );
}

#[test]
fn an_approval_publishes_as_a_draft_version() {
    let template = ApprovalTemplate::new(
        "journal_approval",
        "Journal approval",
        "journal",
        Approver::role("manager"),
        Approver::role("director"),
    );
    let plan = build(&template, &spot_bpm_model::AllowAllMeta, 1).expect("builds");
    let sink = MemorySink::new();
    let release =
        publish_process(&plan, &designer(), "2026-10-20T00:00:00Z", &sink).expect("publishes");
    assert_eq!(release.provenance.kind, "process");
    assert_eq!(release.version.status, spot_bpm_model::VersionStatus::Draft);
    assert!(release.text.contains("first_approval"));
    assert_eq!(
        sink.last_process().expect("stored").version.status,
        spot_bpm_model::VersionStatus::Draft
    );

    // Without the permission nothing is stored.
    let issues = publish_process(&plan, &Designer::new("gina"), "2026-10-20T00:00:00Z", &sink)
        .expect_err("no permission");
    assert!(
        issues
            .iter()
            .any(|i| i.code == "nocode_builders.publish_forbidden")
    );
}
