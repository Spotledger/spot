//! Notifications: the [`Notifier`] port and the rule that a notification carries ids and titles
//! and nothing else.
//!
//! A [`Notification`] is deliberately thin — the task id, the Case and node, the kind, the run
//! label and the title — so no document content, comment or form value can leak through a
//! channel that is logged and stored. The e-mail adapter behind the `mail` feature sends exactly
//! these fields through `spot-mail`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::prelude::OffsetDateTime;

use crate::task::TaskId;

/// Why a notification was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationKind {
    /// A task was created and assigned.
    Assigned,
    /// An SLA reminder fired.
    Reminder,
    /// An SLA escalation moved the task.
    Escalated,
    /// A task was delegated.
    Delegated,
    /// A task was reassigned.
    Reassigned,
    /// A task was completed.
    Completed,
    /// The SLA approved a task automatically; sent to its supervisors.
    AutoApproved,
}

impl NotificationKind {
    /// The stable text form, used in a subject line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::Reminder => "reminder",
            Self::Escalated => "escalated",
            Self::Delegated => "delegated",
            Self::Reassigned => "reassigned",
            Self::Completed => "completed",
            Self::AutoApproved => "auto_approved",
        }
    }
}

/// One notification: ids and titles only, never content.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    /// The task the notification is about.
    pub task: TaskId,
    /// The Case, as text.
    pub case: String,
    /// The Decision node, as text.
    pub node: String,
    /// Why it was sent.
    pub kind: NotificationKind,
    /// The user-facing singular name of a run (ADR-080), e.g. "Request".
    pub run_singular: String,
    /// The task's title, which the process author wrote.
    pub title: String,
    /// The task's due instant, when it has one.
    pub due: Option<OffsetDateTime>,
    /// The recipients' user names.
    pub recipients: Vec<String>,
}

/// The future a [`Notifier`] returns: boxed so the trait stays object-safe and the service can
/// hold an `Arc<dyn Notifier>`.
pub type NotifyFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Where notifications go.
pub trait Notifier: Send + Sync {
    /// Delivers one notification. A delivery failure must not fail the action that caused it;
    /// the output is `()` and the caller does not treat this as fallible.
    ///
    /// The service sends a notification after the task state is stored, so a notification is
    /// never a precondition of a state change. The future may be dropped before it finishes (the
    /// caller was cancelled); the notification may then be lost, which is acceptable, and an
    /// implementation must be safe to drop at any `.await`.
    fn notify<'a>(&'a self, notification: &'a Notification) -> NotifyFuture<'a>;
}

/// A notifier that drops everything: the honest default when no channel is configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn notify<'a>(&'a self, _notification: &'a Notification) -> NotifyFuture<'a> {
        Box::pin(std::future::ready(()))
    }
}

/// A notifier that records what it was asked to send, for tests.
#[derive(Debug, Clone, Default)]
pub struct RecordingNotifier {
    sent: Arc<Mutex<Vec<Notification>>>,
}

impl RecordingNotifier {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Notification>> {
        self.sent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every notification sent so far, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<Notification> {
        self.lock().clone()
    }

    /// How many notifications were sent.
    #[must_use]
    pub fn count(&self) -> usize {
        self.lock().len()
    }
}

impl Notifier for RecordingNotifier {
    fn notify<'a>(&'a self, notification: &'a Notification) -> NotifyFuture<'a> {
        self.lock().push(notification.clone());
        Box::pin(std::future::ready(()))
    }
}

/// The subject line for a notification, from the run label (ADR-080): a run is a "Request" to a
/// user, never a "Case".
#[must_use]
pub fn subject(notification: &Notification) -> String {
    crate::text::notification_subject(notification.kind, &notification.run_singular)
}

/// The e-mail adapter: sends notifications through `spot-mail`.
#[cfg(feature = "mail")]
pub mod mail {
    use spot_mail::{MailTransport, Mailbox, OutgoingMessage};

    use super::{Notification, Notifier, NotifyFuture, subject};

    /// A [`Notifier`](super::Notifier) that delivers each notification by e-mail.
    ///
    /// The body is the subject line and the ids only; the adapter never sends form values,
    /// comments or document content. A recipient that is not a valid address is skipped.
    pub struct MailNotifier<T: MailTransport> {
        transport: T,
        from: Mailbox,
        /// The address suffix recipients are built from (`<user>@<domain>`).
        domain: String,
    }

    impl<T: MailTransport> MailNotifier<T> {
        /// Builds the adapter from a transport, the sender mailbox and the recipient domain.
        #[must_use]
        pub fn new(transport: T, from: Mailbox, domain: &str) -> Self {
            Self {
                transport,
                from,
                domain: domain.to_owned(),
            }
        }
    }

    impl<T: MailTransport> Notifier for MailNotifier<T> {
        fn notify<'a>(&'a self, notification: &'a Notification) -> NotifyFuture<'a> {
            let mut message = OutgoingMessage::new(self.from.clone(), subject(notification));
            message.text = Some(format!(
                "{} (task {}, {} {}, node {})",
                subject(notification),
                notification.task,
                notification.run_singular,
                notification.case,
                notification.node,
            ));
            for recipient in &notification.recipients {
                if let Ok(mailbox) = Mailbox::new(None, format!("{recipient}@{}", self.domain)) {
                    message.to.push(mailbox);
                }
            }
            if message.validate().is_err() {
                return Box::pin(std::future::ready(()));
            }
            Box::pin(async move {
                // A delivery failure never fails the action that caused it.
                let _ = self.transport.send(&message).await;
            })
        }
    }
}
