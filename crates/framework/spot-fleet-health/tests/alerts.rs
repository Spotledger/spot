//! Alerting: deduplication, flap damping, tenant grouping, silences, escalation and sinks.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{at, finding, tenant, view_of};
use spot_fleet_health::{
    AlertManager, AlertNotification, AlertSettings, AlertSink, EscalationStep, Health,
    MemoryAlertSink, RuleId, SinkError,
};

fn red_view(subject: &spot_fleet_health::Subject, rule: &str) -> spot_fleet_health::FleetHealth {
    view_of(vec![finding(
        subject,
        rule,
        Health::Red,
        "The health probe has been failing.",
    )])
}

#[test]
fn one_open_alert_per_subject_and_rule_however_often_it_is_seen() {
    let acme = tenant("acme");
    let sink = Arc::new(MemoryAlertSink::new());
    let mut manager =
        AlertManager::with_sinks(AlertSettings::default(), vec![sink.clone()]).expect("manager");
    let view = red_view(&acme, "health_probe_down");

    let first = manager.observe(&view, at(0));
    assert_eq!(first.opened.len(), 1);
    assert_eq!(sink.len(), 1);

    let second = manager.observe(&view, at(15));
    assert!(second.opened.is_empty());
    assert_eq!(manager.open_alerts().count(), 1);
    assert_eq!(sink.len(), 1, "no second notification for the same alert");
}

#[test]
fn flap_damping_keeps_one_alert_and_resolves_only_after_the_clear_delay() {
    let acme = tenant("acme");
    let sink = Arc::new(MemoryAlertSink::new());
    let mut manager =
        AlertManager::with_sinks(AlertSettings::default(), vec![sink.clone()]).expect("manager");
    let red = red_view(&acme, "health_probe_down");
    let clear = view_of(Vec::new());

    manager.observe(&red, at(0));
    manager.observe(&clear, at(10));
    let back = manager.observe(&red, at(20));
    assert_eq!(manager.open_alerts().count(), 1);
    assert!(
        back.opened.is_empty(),
        "a flap does not open a second alert"
    );
    assert_eq!(sink.len(), 1);

    manager.observe(&clear, at(30));
    let early = manager.observe(&clear, at(89));
    assert!(early.resolved.is_empty(), "not clear for 60 seconds yet");
    let resolved = manager.observe(&clear, at(90));
    assert_eq!(resolved.resolved.len(), 1);
    assert_eq!(manager.open_alerts().count(), 0);
    assert_eq!(sink.len(), 2);
}

#[test]
fn notifications_are_grouped_per_tenant() {
    let acme = tenant("acme");
    let beta = tenant("beta");
    let sink = Arc::new(MemoryAlertSink::new());
    let mut manager =
        AlertManager::with_sinks(AlertSettings::default(), vec![sink.clone()]).expect("manager");
    let view = view_of(vec![
        finding(&acme, "health_probe_down", Health::Red, "Probe down."),
        finding(&acme, "queue_lag_critical", Health::Red, "Queue lagging."),
        finding(&beta, "disk_usage_critical", Health::Red, "Disk full."),
    ]);

    let batch = manager.observe(&view, at(0));
    assert_eq!(batch.notifications.len(), 2, "one group per tenant");
    assert_eq!(sink.len(), 2);
    let acme_group = batch
        .notifications
        .iter()
        .find(|notification| notification.tenant.as_ref() == acme.tenant())
        .expect("acme group");
    assert_eq!(acme_group.alerts.len(), 2);
    let beta_group = batch
        .notifications
        .iter()
        .find(|notification| notification.tenant.as_ref() == beta.tenant())
        .expect("beta group");
    assert_eq!(beta_group.alerts.len(), 1);
}

#[test]
fn a_silence_suppresses_until_its_expiry_then_reopens_the_alert() {
    let acme = tenant("acme");
    let sink = Arc::new(MemoryAlertSink::new());
    let mut manager =
        AlertManager::with_sinks(AlertSettings::default(), vec![sink.clone()]).expect("manager");
    let id = manager
        .add_silence(
            Some(acme.tenant().expect("tenant").clone()),
            None,
            at(30),
            at(0),
            "maintenance window",
        )
        .expect("silence");
    assert_eq!(id, 1);
    let view = red_view(&acme, "health_probe_down");

    let opened = manager.observe(&view, at(0));
    assert_eq!(opened.suppressed.len(), 1);
    assert!(opened.notifications.is_empty());
    assert!(sink.is_empty());
    assert_eq!(manager.open_alerts().count(), 1, "the alert still opens");

    let early = manager.observe(&view, at(29));
    assert!(early.notifications.is_empty());
    assert!(sink.is_empty());

    let reopened = manager.observe(&view, at(31));
    assert_eq!(reopened.notifications.len(), 1);
    assert_eq!(
        reopened.notifications[0].reason,
        spot_fleet_health::NotificationReason::Reopened
    );
    assert_eq!(sink.len(), 1);
}

#[test]
fn a_rule_silence_covers_only_that_rule() {
    let acme = tenant("acme");
    let rule = RuleId::try_new("health_probe_down").expect("rule");
    let sink = Arc::new(MemoryAlertSink::new());
    let mut manager =
        AlertManager::with_sinks(AlertSettings::default(), vec![sink.clone()]).expect("manager");
    manager
        .add_silence(None, Some(rule), at(60), at(0), "known probe issue")
        .expect("silence");

    let view = view_of(vec![
        finding(&acme, "health_probe_down", Health::Red, "Probe down."),
        finding(&acme, "disk_usage_critical", Health::Red, "Disk full."),
    ]);
    let batch = manager.observe(&view, at(0));
    assert_eq!(batch.suppressed.len(), 1);
    assert_eq!(
        batch.notifications.len(),
        1,
        "the other rule still notifies"
    );
    assert_eq!(batch.notifications[0].alerts.len(), 1);
}

#[test]
fn escalation_steps_fire_once_each_on_a_red_alert() {
    let acme = tenant("acme");
    let sink = Arc::new(MemoryAlertSink::new());
    let settings = AlertSettings {
        resolve_after_seconds: 60,
        escalation: vec![
            EscalationStep {
                after_seconds: 60,
                note: "Still open after one minute.".to_owned(),
            },
            EscalationStep {
                after_seconds: 120,
                note: "Still open after two minutes.".to_owned(),
            },
        ],
    };
    let mut manager = AlertManager::with_sinks(settings, vec![sink.clone()]).expect("manager");
    let view = red_view(&acme, "health_probe_down");

    manager.observe(&view, at(0));
    let before = manager.observe(&view, at(59));
    assert!(before.escalated.is_empty());

    let first = manager.observe(&view, at(60));
    assert_eq!(first.escalated.len(), 1);
    assert!(first.escalated[0].sentence.contains("one minute"));
    let again = manager.observe(&view, at(61));
    assert!(again.escalated.is_empty(), "a step fires only once");

    let second = manager.observe(&view, at(120));
    assert_eq!(second.escalated.len(), 1);
    assert!(second.escalated[0].sentence.contains("two minutes"));
}

#[test]
fn an_amber_alert_becoming_red_escalates() {
    let acme = tenant("acme");
    let mut manager = AlertManager::default();
    let amber = view_of(vec![finding(
        &acme,
        "error_rate_high",
        Health::Amber,
        "Error rate is elevated.",
    )]);
    let red = view_of(vec![finding(
        &acme,
        "error_rate_high",
        Health::Red,
        "Error rate is critical.",
    )]);
    manager.observe(&amber, at(0));
    let batch = manager.observe(&red, at(10));
    assert_eq!(batch.escalated.len(), 1);
    assert_eq!(
        manager.open_alerts().next().expect("alert").health,
        Health::Red
    );
}

struct FailingSink;

impl AlertSink for FailingSink {
    fn name(&self) -> &'static str {
        "failing"
    }

    fn deliver(&self, _notification: &AlertNotification) -> Result<(), SinkError> {
        Err(SinkError::new("the destination is down"))
    }
}

#[test]
fn a_sink_failure_is_recorded_and_does_not_stop_the_manager() {
    let acme = tenant("acme");
    let mut manager =
        AlertManager::with_sinks(AlertSettings::default(), vec![Arc::new(FailingSink)])
            .expect("manager");
    let batch = manager.observe(&red_view(&acme, "health_probe_down"), at(0));
    assert_eq!(batch.sink_failures.len(), 1);
    assert_eq!(batch.sink_failures[0].sink, "failing");
    assert_eq!(manager.open_alerts().count(), 1);
}

#[test]
fn an_invalid_silence_is_refused() {
    let mut manager = AlertManager::default();
    assert!(
        manager
            .add_silence(None, None, at(0), at(0), "already expired")
            .is_err()
    );
    assert!(
        manager
            .add_silence(None, None, at(60), at(0), "  ")
            .is_err()
    );
    assert!(
        manager
            .add_silence(None, None, at(60), at(0), "planned work")
            .is_ok()
    );
}
