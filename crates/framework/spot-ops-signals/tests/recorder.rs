//! Recording through the installed recorder: exact label sets, the cardinality bound, the
//! severities set every time, the clamp. One test function: the recorder is process-global.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use spot_ops_signals::catalogue::{Kind, SERIES};
use spot_ops_signals::{
    MAX_EXACT_COUNT, conditions, describe_all, journal, migration, quarantine, sync,
};

fn text() -> String {
    spot_telemetry::render_prometheus()
}

/// The value of the sample whose line starts with `prefix` (name and labels).
fn value(prefix: &str) -> Option<f64> {
    text()
        .lines()
        .find(|l| l.starts_with(prefix))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

/// How many samples of the series `name` the text has (its own lines only).
fn samples(text: &str, name: &str) -> usize {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter(|l| l.starts_with(&format!("{name}{{")) || l.starts_with(&format!("{name} ")))
        .count()
}

#[test]
fn every_series_records_with_its_exact_labels_and_no_more() {
    spot_telemetry::metrics::install().unwrap();
    describe_all().unwrap();
    // Twice is safe.
    describe_all().unwrap();

    // One of everything, every enum value of every labelled series.
    journal::set_ship_lag(Duration::from_secs(7));
    journal::set_unshipped_entries(12);
    for v in journal::BatchOutcome::ALL {
        journal::record_ship_batch(*v);
    }
    journal::record_durability_wait(Duration::from_millis(40));
    for v in journal::Decision::ALL {
        journal::record_durability_decision(*v);
    }
    journal::set_snapshot_age(Duration::from_secs(3600));
    journal::set_audit_age(Duration::from_secs(86_400));
    for v in journal::AuditResult::ALL {
        journal::record_audit_run(*v);
    }
    for v in sync::ApplyOutcome::ALL {
        sync::record_apply(*v);
    }
    sync::set_send_queue_depth(5);
    sync::set_node_lag(&sync::NodeLag {
        p50: Duration::from_secs(1),
        p95: Duration::from_secs(9),
        max: Duration::from_secs(30),
    });
    sync::set_nodes(&sync::NodeCounts {
        healthy: 90,
        lagging: 3,
        silent: 1,
        suspended: 0,
        too_old: 2,
    });
    sync::set_obligations(&sync::ObligationCounts {
        due: 4,
        overdue: 2,
        red: 0,
    });
    sync::set_holds(8);
    quarantine::set_open(2);
    quarantine::set_oldest_age(Duration::from_secs(600));
    conditions::set_open(&conditions::OpenCounts {
        info: 0,
        amber: 3,
        red: 1,
    });
    migration::set_gate_closed(true);
    for verb in migration::Verb::ALL {
        for outcome in migration::VerbOutcome::ALL {
            migration::record_verb(*verb, *outcome);
        }
    }

    let rendered = text();
    for series in SERIES {
        if series.kind == Kind::Histogram {
            // Whatever the exporter renders a histogram as, the series is present.
            assert!(rendered.contains(series.name), "{} is missing", series.name);
            continue;
        }
        assert_eq!(
            samples(&rendered, series.name),
            series.max_series(),
            "{}: the number of samples is not the declared bound\n{rendered}",
            series.name
        );
    }

    // Exact label sets and values.
    assert_eq!(value("spot_journal_ship_lag_seconds "), Some(7.0));
    assert_eq!(
        value(r#"spot_journal_ship_batches_total{outcome="retry"}"#),
        Some(1.0)
    );
    assert_eq!(value(r#"spot_sync_nodes{state="too_old"}"#), Some(2.0));
    assert_eq!(
        value(r#"spot_sync_node_lag_seconds{stat="p95"}"#),
        Some(9.0)
    );
    assert_eq!(value(r#"spot_sync_obligations{level="red"}"#), Some(0.0));
    assert_eq!(value("spot_migration_gate_closed "), Some(1.0));
    assert_eq!(
        value(r#"spot_migration_verbs_total{verb="rescue",outcome="refused"}"#),
        Some(1.0)
    );

    // (d) all three severities, zeros included; a cleared condition goes to 0.
    assert_eq!(value(r#"spot_conditions_open{severity="info"}"#), Some(0.0));
    assert_eq!(
        value(r#"spot_conditions_open{severity="amber"}"#),
        Some(3.0)
    );
    conditions::set_open(&conditions::OpenCounts::default());
    assert_eq!(value(r#"spot_conditions_open{severity="red"}"#), Some(0.0));
    assert_eq!(samples(&text(), "spot_conditions_open"), 3);

    // Counters go up by increments.
    let before = value(r#"spot_sync_apply_total{outcome="applied"}"#).unwrap();
    sync::record_apply(sync::ApplyOutcome::Applied);
    assert_eq!(
        value(r#"spot_sync_apply_total{outcome="applied"}"#),
        Some(before + 1.0)
    );

    // (e) a count above 2^53 is clamped.
    quarantine::set_open(u64::MAX);
    assert_eq!(value("spot_quarantine_open "), Some(MAX_EXACT_COUNT as f64));
    sync::set_holds(MAX_EXACT_COUNT);
    assert_eq!(value("spot_sync_holds "), Some(MAX_EXACT_COUNT as f64));
}
