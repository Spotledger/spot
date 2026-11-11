//! The driver: what moves a conversation between engine steps.
//!
//! [`Conversations`] is the async side of the crate. The handlers are synchronous and run inside
//! the engine's step; everything that happens *because the outside world moved* happens here:
//!
//! - [`process_arrivals`](Conversations::process_arrivals) reads the delivery notice feed. A
//!   notice for a message that opens a conversation starts a request through the trigger intake;
//!   a notice for a message a request waits for completes that wait, exactly once; a peer's
//!   cancellation runs the compensation hook and ends the waits of the conversation.
//! - [`tick`](Conversations::tick) takes the time-driven turns: a wait that ran out of time takes
//!   its timeout branch (and the contract's timeout outcome), and a send that waited for delivery
//!   continues, or takes its refusal branch, when the outbox says so.
//!
//! Both are idempotent and safe to repeat: they keep their place in the notice feed with an
//! acknowledgement, and every effect is guarded by the conversation record, so a crash anywhere
//! only means the next pass does the rest. Time is always an argument.

use std::sync::Arc;

use serde_json::{Value, json};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_contract::OutcomeKind;
use spot_bpm_engine::{CaseStore, Engine, TimerPort};
use spot_bpm_model::TimeoutAction;
use spot_bpm_triggers::TriggerIntake;
use spot_link_exchange::{Conversation, InboxStore, Notice, NoticeFeed};
use spot_tenant_link::LinkId;

use crate::error::{ConversationError, Result, codes};
use crate::handlers::{
    Delivery, classify, received_resolution, refusal_resolution, sent_resolution,
    timeout_resolution, unix_ms,
};
use crate::ledger::repair;
use crate::order::{check_entry, timeout_outcome};
use crate::record::{ConversationRecord, Ending, Wait, WaitKind, update};
use crate::resolve::{Resolution, Resolved, resolve_wait};
use crate::runtime::Runtime;
use crate::trigger::Arrival;
use crate::view::{ConversationView, ViewThresholds, build};
use crate::wire::decode;

/// The notice consumer name the driver acknowledges under.
pub const ARRIVAL_CONSUMER: &str = concat!("bpm", ".conversations.arrivals");

/// The most notices one read takes.
const ARRIVAL_BATCH: usize = 64;

/// A peer's cancellation, as the compensation hook receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cancellation {
    /// The request the conversation belongs to.
    pub case: String,
    /// The conversation.
    pub conversation: Conversation,
    /// The contract.
    pub contract: String,
    /// The outcome the contract declares for the cancellation.
    pub outcome: String,
}

/// The compensation could not be started now; the driver tries again on its next pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompensationDeferred;

/// The seam to the process's declared compensation path (WP-BPM-09, WP-BPM-15).
///
/// The driver calls it once per cancelled conversation, after the request's waits in that
/// conversation were failed to their error branches. A hook that returns
/// [`CompensationDeferred`] is called again on the next pass.
pub trait CompensationHook: Send + Sync {
    /// Starts the compensation path for a peer's cancellation.
    ///
    /// # Errors
    /// [`CompensationDeferred`] when it cannot start now.
    fn compensate(
        &self,
        cancellation: &Cancellation,
    ) -> std::result::Result<(), CompensationDeferred>;
}

/// The hook of a tenant whose processes declare no compensation: the cancellation ends the
/// conversation and fails its waits, and nothing more.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCompensation;

impl CompensationHook for NoCompensation {
    fn compensate(
        &self,
        _cancellation: &Cancellation,
    ) -> std::result::Result<(), CompensationDeferred> {
        Ok(())
    }
}

/// What [`Conversations::process_arrivals`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArrivalReport {
    /// Notices handled.
    pub handled: usize,
    /// Requests started (or found already started) by an arrival.
    pub started: usize,
    /// Waits completed by an arrival.
    pub woken: usize,
    /// Conversations a peer cancelled.
    pub cancelled: usize,
}

/// What [`Conversations::tick`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Message waits that ran out of time.
    pub timed_out: usize,
    /// Sends whose delivery wait ended.
    pub delivered: usize,
}

/// The driver of the conversations of one tenant.
pub struct Conversations<S: CaseStore + 'static, T: TimerPort + 'static> {
    runtime: Arc<Runtime>,
    engine: Arc<Engine<S, T>>,
    intake: Arc<TriggerIntake>,
    hook: Arc<dyn CompensationHook>,
}

fn store_error() -> ConversationError {
    ConversationError::Store
}

impl<S: CaseStore + 'static, T: TimerPort + 'static> Conversations<S, T> {
    /// A driver over the shared runtime and the engine, starting requests through `intake`.
    #[must_use]
    pub fn new(
        runtime: Arc<Runtime>,
        engine: Arc<Engine<S, T>>,
        intake: Arc<TriggerIntake>,
    ) -> Self {
        Self {
            runtime,
            engine,
            intake,
            hook: Arc::new(NoCompensation),
        }
    }

    /// The same driver with a compensation hook.
    #[must_use]
    pub fn with_compensation(mut self, hook: Arc<dyn CompensationHook>) -> Self {
        self.hook = hook;
        self
    }

    /// Reads the delivery notice feed and acts on every arrival it has not handled.
    ///
    /// # Errors
    /// A store, trigger or engine failure. Nothing past the failing notice is acknowledged, so
    /// the next call resumes there.
    pub async fn process_arrivals(
        &self,
        feed: &dyn NoticeFeed,
        inbox: &dyn InboxStore,
        now: OffsetDateTime,
    ) -> Result<ArrivalReport> {
        repair(&self.runtime, feed, inbox)?;
        let mut report = ArrivalReport::default();
        loop {
            let after = feed.acked(ARRIVAL_CONSUMER).map_err(|_| store_error())?;
            let notices = feed
                .read_from(after, ARRIVAL_BATCH)
                .map_err(|_| store_error())?;
            if notices.is_empty() {
                return Ok(report);
            }
            for notice in &notices {
                self.handle(notice, inbox, now, &mut report).await?;
                feed.ack(ARRIVAL_CONSUMER, notice.cursor)
                    .map_err(|_| store_error())?;
                report.handled += 1;
            }
        }
    }

    async fn handle(
        &self,
        notice: &Notice,
        inbox: &dyn InboxStore,
        now: OffsetDateTime,
        report: &mut ArrivalReport,
    ) -> Result<()> {
        let Some(entry) = inbox
            .entry(&notice.link, &notice.message_id)
            .map_err(|_| store_error())?
        else {
            return Ok(());
        };
        let link = notice.link;
        if entry.message.tombstone {
            return Ok(());
        }
        let Ok(decoded) = decode(&entry.message.payload) else {
            return Ok(());
        };
        let rt = &self.runtime;
        let Some(mut record) = rt.store.load(&link, &notice.conversation)? else {
            return Ok(());
        };
        if record.arrivals_done.contains(&notice.message_id)
            || !record.has_event(&notice.message_id)
        {
            return Ok(());
        }
        let Some(contract) = rt
            .book
            .get(&record.contract, record.contract_version)
            .cloned()
        else {
            return Ok(());
        };

        let starts = record.case.is_none()
            && record
                .events
                .first()
                .is_some_and(|event| event.id == notice.message_id)
            && check_entry(&contract, &record.peer_role, &decoded.header.message).is_ok();
        if starts {
            let arrival = Arrival {
                message_id: notice.message_id,
                conversation: notice.conversation.clone(),
                link,
                contract: record.contract.clone(),
                version: record.contract_version,
                message: decoded.header.message.clone(),
                payload: decoded.payload.clone(),
                received_at_ms: notice.received_at,
            };
            let started = self
                .intake
                .process(&arrival.source_event())
                .await
                .map_err(|_| ConversationError::Resolve {
                    code: codes::RESOLVE_FAILED,
                })?;
            report.started += usize::try_from(started.cases()).unwrap_or(0);
            record = rt
                .store
                .load(&link, &notice.conversation)?
                .ok_or(ConversationError::NoConversation)?;
        }

        report.woken += self.wake(&record, now).await?;

        let record = rt
            .store
            .load(&link, &notice.conversation)?
            .ok_or(ConversationError::NoConversation)?;
        if self.cancelled_by_peer(&record, now).await? {
            report.cancelled += 1;
        }
        update(&*rt.store, &link, &notice.conversation, |r| {
            r.arrivals_done.insert(notice.message_id);
            Ok(())
        })?;
        Ok(())
    }

    /// Completes the waits a recorded message satisfies.
    async fn wake(&self, record: &ConversationRecord, now: OffsetDateTime) -> Result<usize> {
        let Some(case) = record.case.clone() else {
            return Ok(0);
        };
        let mut woken = 0;
        for wait in &record.waits {
            let WaitKind::Message { message, .. } = &wait.kind else {
                continue;
            };
            // Each wait looks at the record as it is now: an earlier wait of this pass may have
            // consumed the message, and one message satisfies one wait.
            let Some(fresh) = self
                .runtime
                .store
                .load(&record.link, &record.conversation)?
            else {
                continue;
            };
            if !fresh.waits.contains(wait) {
                continue;
            }
            let Some(event) = fresh.unconsumed(message) else {
                continue;
            };
            let payload = event.payload.clone().unwrap_or(Value::Null);
            let id = event.id;
            let resolution = received_resolution(&wait.node, message, payload);
            let resolved = resolve_wait(
                &self.engine,
                &case,
                &wait.node,
                wait.attempt,
                resolution,
                now,
            )
            .await?;
            if resolved == Resolved::NotYet {
                continue;
            }
            self.finish_wait(&fresh, wait, Some(id))?;
            if resolved == Resolved::Completed {
                woken += 1;
            }
        }
        Ok(woken)
    }

    /// Drops a resolved wait from the record, consuming the message that satisfied it.
    fn finish_wait(
        &self,
        record: &ConversationRecord,
        wait: &Wait,
        consumed: Option<spot_link_exchange::MessageId>,
    ) -> Result<()> {
        update(
            &*self.runtime.store,
            &record.link,
            &record.conversation,
            |r| {
                r.waits
                    .retain(|w| !(w.node == wait.node && w.attempt == wait.attempt));
                if let Some(id) = consumed {
                    r.consumed.insert(id);
                    r.consumed_by.insert((wait.node.clone(), wait.attempt), id);
                }
                Ok(())
            },
        )
        .map(|_| ())
    }

    /// A conversation the peer ended with the contract's cancellation: fail the request's waits
    /// in it to their error branches, then start the declared compensation.
    async fn cancelled_by_peer(
        &self,
        record: &ConversationRecord,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let (Some(case), Some(ending)) = (record.case.clone(), record.ending.clone()) else {
            return Ok(false);
        };
        if ending.kind != OutcomeKind::Cancelled || !ending.by_peer || record.compensated {
            return Ok(false);
        }
        for wait in &record.waits {
            let resolved = resolve_wait(
                &self.engine,
                &case,
                &wait.node,
                wait.attempt,
                Resolution::Failed {
                    code: codes::CONVERSATION_CANCELLED,
                    reason: "the peer cancelled the conversation".to_owned(),
                },
                now,
            )
            .await?;
            if resolved != Resolved::NotYet {
                self.finish_wait(record, wait, None)?;
            }
        }
        let cancellation = Cancellation {
            case,
            conversation: record.conversation.clone(),
            contract: record.contract.clone(),
            outcome: ending.outcome,
        };
        if self.hook.compensate(&cancellation).is_err() {
            return Ok(false);
        }
        update(
            &*self.runtime.store,
            &record.link,
            &record.conversation,
            |r| {
                r.compensated = true;
                Ok(())
            },
        )?;
        Ok(true)
    }

    /// Takes the time-driven turns at `now`.
    ///
    /// # Errors
    /// A store or engine failure; the waits not yet completed stay registered for the next pass.
    pub async fn tick(&self, now: OffsetDateTime) -> Result<TickReport> {
        let now_ms = unix_ms(now);
        let mut report = TickReport::default();
        for record in self.runtime.store.waiting()? {
            let Some(case) = record.case.clone() else {
                continue;
            };
            for wait in record.waits.clone() {
                match &wait.kind {
                    WaitKind::Message {
                        message,
                        on_timeout,
                    } if wait.due_ms.is_some_and(|due| due <= now_ms) => {
                        // A message that arrived before the deadline wins: the wait is resolved
                        // with it here (the arrival pass may have found the engine not yet
                        // waiting and will not look again). When the engine is still not ready
                        // the wait stays as it is and the next tick tries again.
                        let Some(fresh) = self
                            .runtime
                            .store
                            .load(&record.link, &record.conversation)?
                        else {
                            continue;
                        };
                        // The pass took its snapshot of the waits before this load. A wake that
                        // finished this wait for another message of the same name (`wake` checks
                        // the same way) has removed it from the record: the message that is left
                        // is not this wait's, so nothing is consumed for a wait that never
                        // received it.
                        if !fresh.waits.contains(&wait) {
                            continue;
                        }
                        if let Some(event) = fresh.unconsumed(message) {
                            let id = event.id;
                            let payload = event.payload.clone().unwrap_or(Value::Null);
                            let resolved = resolve_wait(
                                &self.engine,
                                &case,
                                &wait.node,
                                wait.attempt,
                                received_resolution(&wait.node, message, payload),
                                now,
                            )
                            .await?;
                            if resolved != Resolved::NotYet {
                                self.finish_wait(&fresh, &wait, Some(id))?;
                            }
                            continue;
                        }
                        let (resolution, ending) =
                            self.timeout(&fresh, &wait.node, message, *on_timeout);
                        let resolved = resolve_wait(
                            &self.engine,
                            &case,
                            &wait.node,
                            wait.attempt,
                            resolution,
                            now,
                        )
                        .await?;
                        if resolved != Resolved::NotYet {
                            // `Gone` with the wait still registered is the crash window between
                            // `resolve_wait` and the two writes: the node is finished and the
                            // ending the timeout names was never written, so write it now rather
                            // than leave the conversation open without it.
                            self.end_by_timeout(&fresh, ending, now_ms)?;
                            self.finish_wait(&fresh, &wait, None)?;
                            report.timed_out += 1;
                        }
                    }
                    WaitKind::Delivery {
                        id,
                        wait_for,
                        on_reject,
                    } => {
                        let Some(sender) = self.runtime.peers.sender(&record.link) else {
                            continue;
                        };
                        let Some(state) = sender.state(id)? else {
                            continue;
                        };
                        let resolution = match classify(state, *wait_for) {
                            Delivery::Pending => continue,
                            Delivery::Reached => {
                                sent_resolution(&wait.node, json!({ "message_id": id.to_string() }))
                            }
                            Delivery::Refused(reason) => {
                                refusal_resolution(*on_reject, &wait.node, reason)
                            }
                        };
                        let resolved = resolve_wait(
                            &self.engine,
                            &case,
                            &wait.node,
                            wait.attempt,
                            resolution,
                            now,
                        )
                        .await?;
                        if resolved != Resolved::NotYet {
                            self.finish_wait(&record, &wait, None)?;
                            report.delivered += 1;
                        }
                    }
                    WaitKind::Message { .. } => {}
                }
            }
        }
        Ok(report)
    }

    /// The resolution of a wait that ran out of time, and the ending the contract names for it.
    /// Nothing is written: the ending is recorded by [`Self::end_by_timeout`] once the wait was
    /// actually resolved.
    fn timeout(
        &self,
        record: &ConversationRecord,
        node: &str,
        message: &str,
        on_timeout: TimeoutAction,
    ) -> (Resolution, Option<(String, OutcomeKind)>) {
        let ending = self
            .runtime
            .book
            .get(&record.contract, record.contract_version)
            .and_then(|contract| timeout_outcome(contract, message, &record.role))
            .map(|outcome| (outcome.name.clone(), outcome.kind));
        let resolution = match on_timeout {
            TimeoutAction::TimeoutEdge => timeout_resolution(node),
            _ => Resolution::Failed {
                code: codes::WAIT_TIMEOUT,
                reason: "the message did not arrive in time".to_owned(),
            },
        };
        (resolution, ending)
    }

    /// Records how the conversation ended when a wait timed out and was resolved.
    fn end_by_timeout(
        &self,
        record: &ConversationRecord,
        ending: Option<(String, OutcomeKind)>,
        now_ms: u64,
    ) -> Result<()> {
        let Some((name, kind)) = ending else {
            return Ok(());
        };
        update(
            &*self.runtime.store,
            &record.link,
            &record.conversation,
            |r| {
                if r.ending.is_none() {
                    r.ending = Some(Ending {
                        outcome: name.clone(),
                        kind,
                        at_ms: now_ms,
                        by_peer: false,
                        timed_out: true,
                    });
                }
                if kind == OutcomeKind::NeedsAttention {
                    r.mark_attention(codes::WAIT_TIMEOUT, now_ms);
                }
                Ok(())
            },
        )
        .map(|_| ())
    }

    /// The request-visible state of a conversation.
    ///
    /// # Errors
    /// [`ConversationError::NoConversation`] when there is none, or a store failure.
    pub fn view(
        &self,
        link: &LinkId,
        conversation: &Conversation,
        now: OffsetDateTime,
        thresholds: &ViewThresholds,
    ) -> Result<ConversationView> {
        let record = self
            .runtime
            .store
            .load(link, conversation)?
            .ok_or(ConversationError::NoConversation)?;
        build(&self.runtime, &record, unix_ms(now), thresholds)
    }

    /// The views of every conversation a request holds.
    ///
    /// # Errors
    /// A store failure.
    pub fn views_of_case(
        &self,
        case: &str,
        now: OffsetDateTime,
        thresholds: &ViewThresholds,
    ) -> Result<Vec<ConversationView>> {
        self.runtime
            .store
            .of_case(case)?
            .iter()
            .map(|record| build(&self.runtime, record, unix_ms(now), thresholds))
            .collect()
    }
}
