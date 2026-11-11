//! The optional `spot-mail` adapter, compiled only with `--features mail`.

#![cfg(feature = "mail")]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_fleet_health::mail::MailAlertSink;
use spot_fleet_health::{
    AlertNotification, AlertSink, AlertSummary, Health, NotificationReason, RuleId, Subject,
    TenantId,
};
use spot_mail::{CaptureTransport, Mailbox};

#[test]
fn the_mail_adapter_sends_one_grouped_message() {
    let transport = Arc::new(CaptureTransport::new());
    let from = Mailbox::new(None, "fleet@example.test").expect("from");
    let to = Mailbox::new(None, "ops@example.test").expect("to");
    let sink = MailAlertSink::new(transport.clone(), from, vec![to]);
    let tenant = TenantId::try_new("acme").expect("tenant");
    let notification = AlertNotification {
        tenant: Some(tenant.clone()),
        reason: NotificationReason::Opened,
        alerts: vec![AlertSummary {
            subject: Subject::Tenant(tenant),
            rule_id: RuleId::try_new("health_probe_down").expect("rule"),
            health: Health::Red,
            sentence: "The probe has been failing for 1 minute.".to_owned(),
        }],
    };
    sink.deliver(&notification).expect("send");
    assert_eq!(transport.messages().len(), 1);
    assert!(MailAlertSink::body(&notification).contains("The probe has been failing"));
}
