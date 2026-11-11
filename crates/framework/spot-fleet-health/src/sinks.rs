//! The delivery boundary: sinks receive one grouped notification at a time.
//!
//! The engine does not know about SMTP, HTTP or a console. It calls [`AlertSink::deliver`] and
//! records a [`SinkError`] in the batch when a sink refuses. Two adapters live here:
//!
//! - [`MemoryAlertSink`] records notifications in process, for tests and the desktop app.
//! - [`WebhookAlertSink`] hands a notification to a [`WebhookPoster`], which the host wires to
//!   the outbound webhook machinery (`spot-webhooks`, WP-INT-01). Keeping the poster a trait
//!   means this crate does not depend on the whole delivery stack to describe the boundary.
//!
//! The `mail` feature adds [`crate::mail::MailAlertSink`], which sends the same notification
//! through a `spot-mail` transport.

use std::sync::{Mutex, MutexGuard, PoisonError};

use thiserror::Error;

use crate::alerts::AlertNotification;

/// A sink refused or could not deliver a notification.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct SinkError {
    /// What went wrong. Never a secret.
    pub message: String,
}

impl SinkError {
    /// A refusal with `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// One destination for grouped alert notifications.
///
/// Implementations must be thread-safe because the manager may deliver from any worker. A
/// refusal is returned, never panicked: an alert that cannot be sent must not take the health
/// loop down with it.
pub trait AlertSink: Send + Sync {
    /// The sink's stable name, used in [`crate::alerts::SinkFailure`].
    fn name(&self) -> &'static str;

    /// Delivers one notification.
    ///
    /// # Errors
    ///
    /// [`SinkError`] when the destination refused or could not be reached.
    fn deliver(&self, notification: &AlertNotification) -> Result<(), SinkError>;
}

/// A sink that keeps every notification in memory.
#[derive(Debug, Default)]
pub struct MemoryAlertSink {
    notifications: Mutex<Vec<AlertNotification>>,
}

impl MemoryAlertSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The notifications received so far, in delivery order.
    #[must_use]
    pub fn notifications(&self) -> Vec<AlertNotification> {
        self.lock().clone()
    }

    /// How many notifications were received.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing has been delivered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<AlertNotification>> {
        self.notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl AlertSink for MemoryAlertSink {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn deliver(&self, notification: &AlertNotification) -> Result<(), SinkError> {
        self.lock().push(notification.clone());
        Ok(())
    }
}

/// Posts one alert notification to a webhook destination.
///
/// The production implementation belongs to the delivery layer (WP-INT-01's `spot-webhooks`),
/// which already owns signing, retries and the egress policy; this trait is the seam it
/// implements.
pub trait WebhookPoster: Send + Sync {
    /// Posts `notification`.
    ///
    /// # Errors
    ///
    /// [`SinkError`] when the destination refused or could not be reached.
    fn post(&self, notification: &AlertNotification) -> Result<(), SinkError>;
}

/// An [`AlertSink`] over a [`WebhookPoster`].
pub struct WebhookAlertSink<P: WebhookPoster> {
    poster: P,
}

impl<P: WebhookPoster> WebhookAlertSink<P> {
    /// A sink that posts through `poster`.
    pub fn new(poster: P) -> Self {
        Self { poster }
    }

    /// The poster, for inspection.
    #[must_use]
    pub fn poster(&self) -> &P {
        &self.poster
    }
}

impl<P: WebhookPoster> AlertSink for WebhookAlertSink<P> {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn deliver(&self, notification: &AlertNotification) -> Result<(), SinkError> {
        self.poster.post(notification)
    }
}

/// A [`WebhookPoster`] from a closure, so a host can wire a small adapter without a new type.
pub struct WebhookPosterFn<F>(pub F);

impl<F> WebhookPoster for WebhookPosterFn<F>
where
    F: Fn(&AlertNotification) -> Result<(), SinkError> + Send + Sync,
{
    fn post(&self, notification: &AlertNotification) -> Result<(), SinkError> {
        (self.0)(notification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::NotificationReason;

    #[test]
    fn the_memory_sink_records_and_counts() {
        let sink = MemoryAlertSink::new();
        assert!(sink.is_empty());
        let notification = AlertNotification {
            tenant: None,
            reason: NotificationReason::Opened,
            alerts: Vec::new(),
        };
        assert!(sink.deliver(&notification).is_ok());
        assert_eq!(sink.len(), 1);
        assert_eq!(sink.notifications(), vec![notification]);
    }
}
