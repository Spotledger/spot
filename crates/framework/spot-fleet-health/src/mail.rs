//! The `spot-mail` adapter behind the `mail` feature.
//!
//! [`MailAlertSink`] turns one grouped [`AlertNotification`] into a plain-text e-mail: the
//! title and one line per alert. It sends through the `spot-mail` transport the host already
//! owns, so alert mail shares its retry, TLS and accounting behaviour; this crate never opens
//! a socket itself.
//!
//! The transport future is completed on the calling thread. `spot-mail`'s `CaptureTransport`
//! (and a transport already inside a runtime) completes immediately; a host that must drive a
//! real SMTP future should call it from the delivery layer instead.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_mail::{MailTransport, Mailbox, OutgoingMessage};

use crate::alerts::AlertNotification;
use crate::sinks::{AlertSink, SinkError};

/// An [`AlertSink`] that sends grouped notifications through `spot-mail`.
pub struct MailAlertSink {
    transport: Arc<dyn MailTransport>,
    from: Mailbox,
    to: Vec<Mailbox>,
}

impl MailAlertSink {
    /// A sink sending from `from` to `to` through `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn MailTransport>, from: Mailbox, to: Vec<Mailbox>) -> Self {
        Self {
            transport,
            from,
            to,
        }
    }

    /// The body of the mail for `notification`, in plain text.
    #[must_use]
    pub fn body(notification: &AlertNotification) -> String {
        let mut body = String::new();
        body.push_str(&notification.title());
        body.push('\n');
        for alert in &notification.alerts {
            body.push_str(&format!(
                "- {} ({}) {}\n",
                alert.health,
                alert.subject.display(),
                alert.sentence
            ));
        }
        body
    }
}

impl AlertSink for MailAlertSink {
    fn name(&self) -> &'static str {
        "mail"
    }

    fn deliver(&self, notification: &AlertNotification) -> Result<(), SinkError> {
        let mut message = OutgoingMessage::new(self.from.clone(), notification.title());
        message.text = Some(Self::body(notification));
        message.to.clone_from(&self.to);
        message
            .validate()
            .map_err(|error| SinkError::new(error.to_string()))?;
        let receipt = block_on(self.transport.send(&message))?
            .map_err(|error| SinkError::new(error.to_string()))?;
        if !receipt.rejected.is_empty() {
            return Err(SinkError::new(format!(
                "{} alert recipients were rejected",
                receipt.rejected.len()
            )));
        }
        Ok(())
    }
}

/// Completes an already-ready (or cooperatively progressing) future without a runtime.
fn block_on<F: Future>(future: F) -> Result<F::Output, SinkError> {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    for _ in 0..1_000_000 {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return Ok(value),
            Poll::Pending => std::thread::yield_now(),
        }
    }
    Err(SinkError::new(
        "the mail transport did not complete on this thread",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::{AlertSummary, NotificationReason};
    use crate::health::Health;
    use crate::rules::RuleId;
    use crate::signal::{Subject, TenantId};

    #[test]
    fn the_mail_sink_sends_the_grouped_sentences() {
        let transport = Arc::new(spot_mail::CaptureTransport::new());
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
                sentence: "The health probe for tenant acme has been failing for 1 minute."
                    .to_owned(),
            }],
        };
        sink.deliver(&notification).expect("delivery");
        assert_eq!(transport.messages().len(), 1);
        let body = MailAlertSink::body(&notification);
        assert!(body.contains("tenant acme"));
        assert!(body.contains("has been failing"));
    }
}
