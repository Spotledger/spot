//! The optional persistence capability (feature `storage`): the pending state-changing calls,
//! tagged with their owner; never the session token, never a password and never a request to
//! a path class that [`crate::persist_policy`] denies.
//!
//! The stored value is `{"v":2,"owner":{"site":"..","user_id":".."},"calls":[..]}`. The owner is
//! the host's site (given to [`Core::start`]) and the user id the sign-in answer carried. A
//! queue is never written without both. A restored queue is held apart until a sign-in has
//! established who the user is: the same owner gets its calls sent once, anyone else (or an
//! untagged or unreadable value) gets them discarded with `auth.session_changed` failures, a
//! [`Event::QueueDiscarded`] and a `StorageWrite` with a `null` value.

use serde::{Deserialize, Serialize};

use crate::core::{Core, Op, Pending};
use crate::persist_policy::may_persist;
use crate::types::{DiscardReason, Event, Output};

const QUEUE_KEY: &str = "queue";
/// The id of the one `StorageRead` the core issues.
const READ_ID: u64 = 1;
/// The version of the stored value.
const VERSION: u8 = 2;

#[derive(Serialize, Deserialize)]
struct StoredCall {
    req_id: u64,
    method: String,
    path: String,
    body: Option<String>,
    if_match: Option<u64>,
    idempotency_key: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Owner {
    site: String,
    user_id: String,
}

#[derive(Serialize)]
struct WriteQueue<'a> {
    v: u8,
    owner: &'a Owner,
    calls: &'a [StoredCall],
}

#[derive(Deserialize)]
struct ReadQueue {
    v: u8,
    owner: Owner,
    calls: Vec<StoredCall>,
}

/// What a restored queue is, until a sign-in decides.
pub(crate) enum Verdict {
    /// A version 2 queue of this owner.
    Owned(Owner),
    /// Never replayed, for this reason.
    Discard(DiscardReason),
}

/// A restored queue, held apart from the live requests until a sign-in has established the user.
pub(crate) struct Restored {
    verdict: Verdict,
    calls: Vec<Pending>,
}

fn stored_call(p: &Pending) -> StoredCall {
    StoredCall {
        req_id: p.req_id,
        method: p.method.clone(),
        path: p.path.clone(),
        body: p.body.clone(),
        if_match: p.if_match,
        idempotency_key: p.idempotency_key.clone(),
    }
}

fn pending_of(call: StoredCall) -> Pending {
    Pending {
        req_id: call.req_id,
        op: Op::Call,
        method: call.method,
        path: call.path,
        body: call.body,
        if_match: call.if_match,
        idempotency_key: call.idempotency_key,
        sends: 0,
        reauthed: false,
        restored: true,
    }
}

impl Core {
    /// Asks the host for the stored queue. Call once at start; `site` names the site this core
    /// serves and, with the signed-in user id, is the owner written with the queue.
    pub fn start(&mut self, site: &str) -> Vec<Output> {
        self.site = site.to_owned();
        vec![Output::StorageRead {
            id: READ_ID,
            key: QUEUE_KEY.to_owned(),
        }]
    }

    pub(crate) fn on_storage_reply(
        &mut self,
        id: u64,
        value: Option<String>,
        out: &mut Vec<Output>,
    ) {
        if id != READ_ID || self.restored.is_some() {
            return;
        }
        let Some(text) = value else {
            return;
        };
        let (verdict, stored) = match serde_json::from_str::<ReadQueue>(&text) {
            Ok(queue) if queue.v == VERSION => (Verdict::Owned(queue.owner), queue.calls),
            Ok(_) => (Verdict::Discard(DiscardReason::Unreadable), Vec::new()),
            Err(_) => match serde_json::from_str::<Vec<StoredCall>>(&text) {
                Ok(calls) => (Verdict::Discard(DiscardReason::Untagged), calls),
                Err(_) => (Verdict::Discard(DiscardReason::Unreadable), Vec::new()),
            },
        };
        // What the host holds now, so that `persist` writes again when the queue it would
        // store differs (a denied call dropped) and not when it is the same.
        self.persisted = Some(match &verdict {
            Verdict::Owned(owner) => Self::queue_text(owner, &stored).unwrap_or(text),
            Verdict::Discard(_) => text,
        });
        // A call the policy denies is dropped, never held and never sent.
        let calls: Vec<Pending> = stored
            .into_iter()
            .filter(|c| may_persist(&c.method, &c.path))
            .map(pending_of)
            .collect();
        for call in &calls {
            out.push(Output::Emit(Event::ReauthRequired {
                req_id: call.req_id,
            }));
        }
        self.restored = Some(Restored { verdict, calls });
        // A reply that arrives after a sign-in is judged against that user at once.
        if self.session.is_some() || self.cache_user.is_some() {
            let user = self.cache_user.clone();
            self.settle_restored(user.as_deref(), out);
        }
    }

    /// Decides the restored queue once the user is known: the owner's calls are held for
    /// sending, everything else is discarded.
    pub(crate) fn settle_restored(&mut self, user: Option<&str>, out: &mut Vec<Output>) {
        let Some(restored) = self.restored.take() else {
            return;
        };
        let reason = match restored.verdict {
            Verdict::Owned(owner) => {
                let mine = !self.site.is_empty()
                    && owner.site == self.site
                    && user.is_some_and(|u| !u.is_empty() && u == owner.user_id);
                if mine {
                    self.held.extend(restored.calls);
                    return;
                }
                DiscardReason::OwnerChanged
            }
            Verdict::Discard(reason) => reason,
        };
        let count = restored.calls.len();
        for call in restored.calls {
            out.push(Self::local_failure(
                call.req_id,
                "auth.session_changed",
                "A different user signed in; the request was not sent.",
            ));
        }
        out.push(Output::Emit(Event::QueueDiscarded {
            count: u64::try_from(count).unwrap_or(u64::MAX),
            reason,
        }));
    }

    fn queue_text(owner: &Owner, calls: &[StoredCall]) -> Option<String> {
        serde_json::to_string(&WriteQueue {
            v: VERSION,
            owner,
            calls,
        })
        .ok()
    }

    /// Takes the restored mark off every call whose DocType (or child table) is now known to
    /// hold a secret, for good: forgetting that metadata later does not bring the call back.
    fn unmark_unsafe_restored(&mut self) {
        let meta = &self.meta;
        let unmark = |p: &mut Pending| {
            if p.restored && !meta.may_keep(&p.method, &p.path) {
                p.restored = false;
            }
        };
        self.inflight.values_mut().for_each(unmark);
        self.blocked.iter_mut().for_each(unmark);
        self.retrying.iter_mut().for_each(|(_, p)| unmark(p));
        self.held.iter_mut().for_each(unmark);
        if let Some(r) = &mut self.restored {
            r.calls.iter_mut().for_each(unmark);
        }
    }

    /// Takes the restored mark off every call in the live queues (a user change).
    pub(crate) fn unmark_all_restored(&mut self) {
        let unmark = |p: &mut Pending| p.restored = false;
        self.inflight.values_mut().for_each(unmark);
        self.blocked.iter_mut().for_each(unmark);
        self.retrying.iter_mut().for_each(|(_, p)| unmark(p));
        self.held.iter_mut().for_each(unmark);
    }

    /// Writes the queue when it changed since the last write.
    pub(crate) fn persist(&mut self, out: &mut Vec<Output>) {
        self.unmark_unsafe_restored();
        let owner = match &self.restored {
            // Before the first sign-in the stored owner still owns the restored calls.
            Some(Restored {
                verdict: Verdict::Owned(owner),
                ..
            }) => Some(owner.clone()),
            // A queue waiting to be discarded stays as the host holds it until then.
            Some(_) => return,
            None => self.cache_user.as_ref().map(|user| Owner {
                site: self.site.clone(),
                user_id: user.clone(),
            }),
        };
        let restored = self.restored.iter().flat_map(|r| r.calls.iter());
        let calls: Vec<StoredCall> = self
            .inflight
            .values()
            .chain(self.blocked.iter())
            .chain(self.retrying.iter().map(|(_, p)| p))
            .chain(self.held.iter())
            .chain(restored)
            .filter(|p| {
                matches!(p.op, Op::Call)
                    && if p.restored {
                        self.meta.may_keep(&p.method, &p.path)
                    } else {
                        self.meta.may_store(&p.method, &p.path, p.body.is_some())
                    }
            })
            .map(stored_call)
            .collect();
        let text = if calls.is_empty() {
            None
        } else {
            match owner {
                // Never a queue without a site and a user: keep the calls in memory only.
                Some(owner) if !owner.site.is_empty() && !owner.user_id.is_empty() => {
                    Self::queue_text(&owner, &calls)
                }
                _ => return,
            }
        };
        if text != self.persisted {
            self.persisted = text.clone();
            out.push(Output::StorageWrite {
                key: QUEUE_KEY.to_owned(),
                value: text,
            });
        }
    }
}
