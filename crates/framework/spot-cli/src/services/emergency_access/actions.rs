//! Recording what a user does under an emergency grant (WP-IAM-08.T16).
//!
//! The door that honours an emergency assignment calls [`EmergencyActions::record`] for every
//! action the user takes while the assignment is in force: the action's name and the target's type
//! and id, never a payload. [`ActionRecorder`] finds the user's active grant and records the action
//! under it (`Emergency::record_action`), so the controller's review has something to review.
//!
//! A recording that fails (the store is unreachable, a concurrent change won) is not lost: it goes
//! to a bounded retry queue that the service's sweep drains on every pass, oldest first. When the
//! queue is full the oldest entry is dropped and counted, never the newest, and the count is
//! readable ([`ActionRecorder::dropped`]). An action by a user with no active emergency grant
//! records nothing.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_http::api::service::BoxFuture;

use super::flow::Emergency;
use super::model::EmergencyError;

/// The retry queue's bound.
pub const QUEUE_MAX: usize = 1024;

/// What the recorder needs from the grants.
pub trait GrantActions: Send + Sync {
    /// The ids of the active grants of `user`.
    fn active_grants<'a>(
        &'a self,
        user: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, EmergencyError>>;
    /// Records one action under grant `id`.
    fn record<'a>(
        &'a self,
        id: &'a str,
        user: &'a str,
        action: &'a str,
        detail: &'a str,
    ) -> BoxFuture<'a, Result<(), EmergencyError>>;
}

impl GrantActions for Emergency {
    fn active_grants<'a>(
        &'a self,
        user: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, EmergencyError>> {
        Box::pin(async move {
            let (views, _unreadable) = self.list().await?;
            Ok(views
                .into_iter()
                .filter(|v| v.user == user && v.phase == "active")
                .map(|v| v.id)
                .collect())
        })
    }

    fn record<'a>(
        &'a self,
        id: &'a str,
        user: &'a str,
        action: &'a str,
        detail: &'a str,
    ) -> BoxFuture<'a, Result<(), EmergencyError>> {
        Box::pin(async move { self.record_action(id, user, action, Some(detail)).await })
    }
}

impl GrantActions for std::sync::Arc<Emergency> {
    fn active_grants<'a>(
        &'a self,
        user: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, EmergencyError>> {
        (**self).active_grants(user)
    }

    fn record<'a>(
        &'a self,
        id: &'a str,
        user: &'a str,
        action: &'a str,
        detail: &'a str,
    ) -> BoxFuture<'a, Result<(), EmergencyError>> {
        GrantActions::record(&**self, id, user, action, detail)
    }
}

/// The port the permission door calls.
pub trait EmergencyActions: Send + Sync {
    /// `user` did `action` on the record of `target_type` named `target_id`. Never fails the
    /// caller: a recording that cannot be made now is queued and retried.
    fn record<'a>(
        &'a self,
        user: &'a str,
        action: &'a str,
        target_type: &'a str,
        target_id: &'a str,
    ) -> BoxFuture<'a, ()>;
}

struct Queued {
    id: String,
    user: String,
    action: String,
    detail: String,
}

/// The recorder over any [`GrantActions`].
pub struct ActionRecorder<G: GrantActions> {
    grants: G,
    queue: Mutex<VecDeque<Queued>>,
    dropped: AtomicU64,
}

fn detail_of(target_type: &str, target_id: &str) -> String {
    // A type and an id only; the length is bounded so the detail always fits the record.
    let mut text = format!("{target_type}:{target_id}");
    text.truncate(200);
    text
}

impl<G: GrantActions> ActionRecorder<G> {
    /// A recorder over `grants`.
    pub fn new(grants: G) -> Self {
        Self {
            grants,
            queue: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// The grants this recorder acts through.
    pub fn grants(&self) -> &G {
        &self.grants
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<Queued>> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn enqueue(&self, item: Queued) {
        let mut queue = self.queue();
        if queue.len() >= QUEUE_MAX {
            queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(item);
    }

    /// Entries waiting for a retry.
    pub fn pending(&self) -> usize {
        self.queue().len()
    }

    /// Entries dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Records the action under the user's active grant, or queues it when that fails.
    pub async fn record_action(
        &self,
        user: &str,
        action: &str,
        target_type: &str,
        target_id: &str,
    ) {
        let detail = detail_of(target_type, target_id);
        let ids = match self.grants.active_grants(user).await {
            Ok(ids) => ids,
            Err(_) => {
                // The grants could not be read: the grant is unknown, so keep the facts and try
                // again from the sweep.
                self.enqueue(Queued {
                    id: String::new(),
                    user: user.to_owned(),
                    action: action.to_owned(),
                    detail,
                });
                return;
            }
        };
        for id in ids {
            if self
                .grants
                .record(&id, user, action, &detail)
                .await
                .is_err()
            {
                self.enqueue(Queued {
                    id,
                    user: user.to_owned(),
                    action: action.to_owned(),
                    detail: detail.clone(),
                });
            }
        }
    }

    /// Retries the queue, oldest first; an entry that fails again goes back to the end. Returns
    /// how many were recorded (or found to need no recording because no grant is active).
    pub async fn retry(&self) -> usize {
        let waiting: Vec<Queued> = self.queue().drain(..).collect();
        let mut done = 0;
        for item in waiting {
            let ids = if item.id.is_empty() {
                match self.grants.active_grants(&item.user).await {
                    Ok(ids) => ids,
                    Err(_) => {
                        self.enqueue(item);
                        continue;
                    }
                }
            } else {
                vec![item.id.clone()]
            };
            let mut failed = false;
            for id in &ids {
                if self
                    .grants
                    .record(id, &item.user, &item.action, &item.detail)
                    .await
                    .is_err()
                {
                    failed = true;
                }
            }
            if failed {
                self.enqueue(Queued {
                    id: if ids.len() == 1 {
                        ids[0].clone()
                    } else {
                        String::new()
                    },
                    ..item
                });
            } else {
                done += 1;
            }
        }
        done
    }
}

impl<G: GrantActions> EmergencyActions for ActionRecorder<G> {
    fn record<'a>(
        &'a self,
        user: &'a str,
        action: &'a str,
        target_type: &'a str,
        target_id: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.record_action(user, action, target_type, target_id))
    }
}
