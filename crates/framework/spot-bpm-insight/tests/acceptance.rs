//! The acceptance fixture: a seeded bottleneck is detected and shown, plus the stuck queue, the
//! permission rule and the report definitions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, NodeKind};
use spot_bpm_insight::{
    AllowList, AttemptRef, BottleneckPolicy, Millis, NodeRunEvent, ProcessKey, ReadNone, RunLabels,
    StuckPolicy, StuckReason, WaitReason, aggregate, aggregate_for, report_definitions,
};
use spot_dag::NodeId;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
}

fn attempt(process: &str, case: &str, node: &str, kind: NodeKind) -> AttemptRef {
    AttemptRef::new(
        ProcessKey::new(process, 1),
        CaseId::new(case).unwrap(),
        NodeId::new(node).unwrap(),
        1,
        kind,
    )
}

/// One complete run of a step: started, then done `seconds` later.
fn completed(
    process: &str,
    case: &str,
    node: &str,
    kind: NodeKind,
    seconds: i64,
) -> Vec<NodeRunEvent> {
    vec![
        NodeRunEvent::started(attempt(process, case, node, kind.clone()), at(0)),
        NodeRunEvent::done(attempt(process, case, node, kind), at(seconds)),
    ]
}

/// Eight steps of one process version; `approve` is ten times slower than the rest.
fn seeded_process() -> Vec<NodeRunEvent> {
    let steps: [(&str, NodeKind, i64); 8] = [
        ("draft", NodeKind::Document, 10),
        ("check_stock", NodeKind::Integration, 10),
        ("reserve", NodeKind::Document, 10),
        ("pick", NodeKind::Document, 10),
        ("pack", NodeKind::Document, 10),
        ("ship", NodeKind::Document, 10),
        ("invoice", NodeKind::Document, 10),
        ("approve", NodeKind::Decision, 100),
    ];
    let mut events = Vec::new();
    for (node, kind, seconds) in steps {
        events.extend(completed("order_flow", "case1", node, kind, seconds));
    }
    events
}

#[test]
fn the_seeded_bottleneck_is_detected_and_shown() {
    let insight = aggregate(seeded_process());
    assert_eq!(insight.steps().len(), 8, "one step per node");

    let ranked = insight.bottlenecks(&BottleneckPolicy::default());
    assert_eq!(
        ranked.len(),
        1,
        "only the slow approval step passes the threshold"
    );
    assert_eq!(ranked[0].node.as_str(), "approve");
    assert_eq!(ranked[0].share_per_mille, 588);
    assert_eq!(
        ranked[0].explanation,
        "step `approve` of order_flow v1 uses 588 per-mille of cycle time \
         (p90 100000 ms of 170000 ms) across 1 completed attempt(s) and its queue grew by 0"
    );
}

#[test]
fn percentiles_are_exact_integers() {
    let mut events = Vec::new();
    // Ten runs of `approve`, each 1 s longer than the last: 1..=10 s.
    for (index, seconds) in (1..=10).enumerate() {
        let case = format!("case{index}");
        events.extend(completed(
            "order_flow",
            &case,
            "approve",
            NodeKind::Decision,
            seconds,
        ));
    }
    let insight = aggregate(events);
    let step = &insight.steps()[0];
    assert_eq!(step.p50, Some(Millis::from_seconds(5)));
    assert_eq!(step.p90, Some(Millis::from_seconds(9)));
    assert_eq!(step.p99, Some(Millis::from_seconds(10)));
    assert_eq!(step.min, Some(Millis::from_seconds(1)));
    assert_eq!(step.max, Some(Millis::from_seconds(10)));
}

#[test]
fn the_stuck_queue_names_the_reason_and_the_owner() {
    let events = vec![
        NodeRunEvent::waiting(
            attempt("order_flow", "case1", "approve", NodeKind::Decision),
            at(0),
            WaitReason::HumanTask,
        )
        .with_owner("ada"),
        NodeRunEvent::retrying(
            attempt("order_flow", "case2", "call", NodeKind::Integration),
            at(0),
            "conn.timeout",
        ),
    ];
    let insight = aggregate(events);
    let stuck = insight.stuck_cases(at(4000), &StuckPolicy::new(Millis::from_seconds(1800)));
    assert_eq!(stuck.len(), 2);
    assert_eq!(stuck[0].reason, StuckReason::WaitingForTask);
    assert_eq!(stuck[0].owner.as_deref(), Some("ada"));
    assert_eq!(stuck[1].reason, StuckReason::IntegrationRetry);
    assert_eq!(stuck[0].elapsed, Millis::from_seconds(4000));
}

#[test]
fn a_process_the_viewer_may_not_read_is_absent_not_zeros() {
    let mut events = seeded_process();
    events.extend(completed(
        "other_flow",
        "case2",
        "step",
        NodeKind::Document,
        5,
    ));

    let mut access = AllowList::new();
    access.insert(ProcessKey::new("order_flow", 1));

    let visible = aggregate_for(events.clone(), &access);
    assert_eq!(
        visible.steps().len(),
        8,
        "only the readable process is present"
    );
    assert!(
        visible
            .steps()
            .iter()
            .all(|step| step.process.name == "order_flow"),
        "no row leaks the other process"
    );

    let nothing = aggregate_for(events, &ReadNone);
    assert!(nothing.is_empty(), "a viewer with no access sees nothing");
    assert!(nothing.steps().is_empty());
    assert!(nothing.open_attempts().is_empty());
}

#[test]
fn the_shipped_reports_parse_validate_and_use_the_run_label() {
    let defs = report_definitions(&RunLabels::defaults()).expect("definitions validate");
    assert_eq!(defs.len(), 3);
    let stuck = defs
        .iter()
        .find(|def| def.name == "bpm_stuck_queue")
        .unwrap();
    assert_eq!(stuck.label, "Stuck Requests");
    assert_eq!(stuck.base, "bpm_node_run");
    let run_column = stuck
        .columns
        .iter()
        .find(|column| column.field == "run")
        .unwrap();
    assert_eq!(run_column.label.as_deref(), Some("Request"));

    let labels = RunLabels::new(Some("Ticket"), Some("Tickets")).unwrap();
    let overridden = report_definitions(&labels).unwrap();
    let stuck = overridden
        .iter()
        .find(|def| def.name == "bpm_stuck_queue")
        .unwrap();
    assert_eq!(stuck.label, "Stuck Tickets");
    let cycle = overridden
        .iter()
        .find(|def| def.name == "bpm_cycle_time_by_step")
        .unwrap();
    let completed = cycle
        .columns
        .iter()
        .find(|column| column.field == "attempt")
        .unwrap();
    assert_eq!(completed.label.as_deref(), Some("Completed Tickets"));
}

#[test]
fn no_user_visible_string_says_case() {
    let mut strings: Vec<String> = Vec::new();

    for def in report_definitions(&RunLabels::defaults()).unwrap() {
        strings.push(def.label.clone());
        for column in &def.columns {
            if let Some(label) = &column.label {
                strings.push(label.clone());
            }
        }
        for computed in &def.computed {
            if let Some(label) = &computed.label {
                strings.push(label.clone());
            }
        }
        for parameter in &def.parameters {
            strings.push(parameter.label.clone());
        }
    }

    for reason in [
        StuckReason::WaitingForTask,
        StuckReason::Timer,
        StuckReason::IntegrationRetry,
        StuckReason::Running,
    ] {
        strings.push(reason.label().to_owned());
    }

    let insight = aggregate(seeded_process());
    for bottleneck in insight.bottlenecks(&BottleneckPolicy::new(0, 0)) {
        strings.push(bottleneck.explanation);
    }

    assert!(!strings.is_empty());
    for text in &strings {
        assert!(
            !text.contains("Case"),
            "user-visible text must not say the internal word: {text}"
        );
    }
}

#[test]
fn every_raised_code_has_a_catalogue_entry() {
    use spot_bpm_insight::InsightError;
    use spot_errors::Catalogue;

    let catalogue = Catalogue::load();
    assert!(catalogue.load_error().is_none(), "the catalogue loads");
    let errors = [
        InsightError::ReportDefinition {
            name: "bpm_x".to_owned(),
            detail: "d".to_owned(),
        },
        InsightError::LabelInvalid {
            which: "singular".to_owned(),
        },
    ];
    for error in &errors {
        assert!(
            catalogue.lookup(error.code()).is_some(),
            "no catalogue entry for {}",
            error.code()
        );
    }
}
