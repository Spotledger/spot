//! The running-low signal of a branch series, carried to the outbox (WP-EDG-03.T7).
//!
//! [`BranchSeries::running_low`](crate::BranchSeries::running_low) says a branch's naming range
//! is nearly used up, so the hub can grant another in time. On its own it reaches nobody: a
//! branch would find out when `partition.range_exhausted` stops its users. This module writes
//! the signal as a platform outbox event, `sys_partition_range_low`
//! ([`RANGE_LOW_TOPIC`]), **in the transaction that consumed the number**, so the event exists
//! if and only if the allocation committed.
//!
//! # What the event carries
//!
//! A [`RunningLowNotice`]: the series, the node, the numbers remaining and the lease's total.
//! Counts and ids only, never a document value. The topic is a reserved `sys_` topic written
//! through the existing topic-name API; registering its payload schema in the events contract
//! is outside this crate (see the task report).
//!
//! # At most once per lease, and never a stall
//!
//! [`RunningLowNotifier`] remembers the lease it announced, so one lease raises one event; a new
//! lease re-arms it. The memory is two-phase, because the transaction decides whether the event
//! exists: [`RunningLowNotifier::allocate`] marks the notice *pending* (no second event in the
//! same transaction) and the host calls [`RunningLowNotifier::committed`] after the commit, or
//! [`RunningLowNotifier::rolled_back`] after a rollback, which leaves the series armed. A host
//! that calls neither keeps the lease pending and raises no duplicate, so it must call one.
//!
//! The notice must never become a way to stall writes. If it cannot be written, the number is
//! still issued: the failure is counted ([`RunningLowNotifier::failures`]) and reported once per
//! lease as `partition.range_low_not_recorded` ([`RunningLowNotifier::take_report`]); the range
//! running out stays the only hard stop. The high-water mark is persisted outside the
//! transaction, so a rolled-back allocation still consumes its number (it is never reissued).

use std::sync::Arc;

use serde_json::json;
use spot_app_api::BoxFuture;
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId};
use spot_events_contract::Topic;
use spot_events_contract::payload::{Actor, EventMeta, PayloadDocument};
use spot_storage_contract::feed::Outbox;
use spot_storage_contract::{NodeRef, OutboxEvent, SeriesKey};
use spot_sync_protocol::NodeId;

use crate::error::PartitionError;
use crate::lease::LeaseId;
use crate::series::BranchSeries;

/// The platform topic of the running-low event (reserved `sys_` prefix, ADR-061).
pub const RANGE_LOW_TOPIC: &str = "sys_partition_range_low";

/// The payload schema version of the running-low event.
pub const SCHEMA_VERSION: u32 = 1;

/// The entity name of the event's subject (the series is not a document).
const SUBJECT_ENTITY: &str = "sys_partition_range";

/// The platform component named as the actor of the event.
const COMPONENT: &str = "partition";

/// A branch series is running low: the numbers it has left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningLowNotice {
    /// The naming series.
    pub series: SeriesKey,
    /// The node that owns the series.
    pub node: NodeId,
    /// The numbers still unallocated in the lease.
    pub remaining: u64,
    /// The size of the lease.
    pub total: u64,
}

/// Builds the `sys_partition_range_low` outbox event for `notice`.
///
/// `event_id` is the new event's id (the consumers' deduplication key) and `at` the business
/// time; nothing here reads a clock or generates an id (ADR-077).
///
/// # Errors
///
/// [`PartitionError::InvalidArgument`] when the topic, subject or payload cannot be encoded.
pub fn running_low_event(
    notice: &RunningLowNotice,
    event_id: RecordId,
    at: OffsetDateTime,
) -> Result<OutboxEvent, PartitionError> {
    let encode = |what: &str, e: &dyn std::fmt::Display| PartitionError::InvalidArgument {
        reason: format!("the running-low event cannot be encoded ({what}): {e}"),
    };
    let topic = Topic::any(RANGE_LOW_TOPIC).map_err(|e| encode("topic", &e))?;
    let component = Name::new(COMPONENT).map_err(|e| encode("actor", &e))?;
    let entity = Name::new(SUBJECT_ENTITY).map_err(|e| encode("subject", &e))?;
    let document = PayloadDocument {
        meta: EventMeta {
            occurred_at: at,
            actor: Actor::System { component },
            command_id: None,
            causation_id: None,
            correlation_id: event_id,
            trace_id: None,
        },
        data: json!({
            "series": notice.series.as_str(),
            "node": notice.node.to_string(),
            "remaining": notice.remaining,
            "total": notice.total,
        }),
    };
    document
        .to_outbox_event(
            event_id,
            &topic,
            NodeRef {
                entity,
                id: event_id,
            },
            None,
            SCHEMA_VERSION,
        )
        .map_err(|e| encode("payload", &e))
}

/// Where the notifier writes the event: the host binds it to the business transaction.
///
/// The same shape as the audit writer of `spot-doc-audit`: it takes the transaction and the
/// event, so the event commits or rolls back with the allocation.
pub trait OutboxWriter<Tx>: Send + Sync {
    /// Writes `event` to the outbox of `tx`.
    ///
    /// # Errors
    ///
    /// [`PartitionError::Store`] when the outbox refuses.
    fn write<'a>(
        &'a self,
        tx: &'a mut Tx,
        event: &'a OutboxEvent,
    ) -> BoxFuture<'a, Result<(), PartitionError>>;
}

/// The writer for any transaction that has an outbox: it enqueues the event there.
#[derive(Clone, Copy, Debug, Default)]
pub struct TxOutbox;

impl<Tx: Outbox> OutboxWriter<Tx> for TxOutbox {
    fn write<'a>(
        &'a self,
        tx: &'a mut Tx,
        event: &'a OutboxEvent,
    ) -> BoxFuture<'a, Result<(), PartitionError>> {
        Box::pin(async move {
            tx.enqueue(event).await.map_err(|e| PartitionError::Store {
                reason: format!(
                    "the outbox refused the running-low event: {}",
                    e.explanation
                ),
            })
        })
    }
}

/// A number allocated through a [`RunningLowNotifier`].
///
/// Hand it back to [`RunningLowNotifier::committed`] once the transaction commits, or to
/// [`RunningLowNotifier::rolled_back`] when it rolls back.
#[derive(Debug)]
#[must_use = "report the transaction's outcome with `committed` or `rolled_back`"]
pub struct Allocated {
    /// The number allocated.
    pub number: u64,
    announced: Option<LeaseId>,
}

impl Allocated {
    /// Whether this allocation enqueued a running-low event in the transaction.
    #[must_use]
    pub fn announced(&self) -> bool {
        self.announced.is_some()
    }
}

/// Allocates from a [`BranchSeries`] and announces, once per lease, that it is running low.
pub struct RunningLowNotifier<W> {
    writer: Arc<W>,
    notified: Option<LeaseId>,
    pending: Option<LeaseId>,
    failures: u64,
    reported: Option<LeaseId>,
    report: Option<PartitionError>,
}

impl<W> RunningLowNotifier<W> {
    /// A notifier that writes through `writer`.
    #[must_use]
    pub fn new(writer: Arc<W>) -> Self {
        Self {
            writer,
            notified: None,
            pending: None,
            failures: 0,
            reported: None,
            report: None,
        }
    }

    /// Allocates the next number of `series` and, when that leaves the series running low and
    /// the lease has not been announced, enqueues the event through `tx` (the transaction that
    /// consumes the number).
    ///
    /// A failing outbox write does not stop numbering: the number is returned, the failure is
    /// counted and reported once per lease.
    ///
    /// # Errors
    ///
    /// As [`BranchSeries::allocate`]: `partition.range_exhausted` and `partition.store`. Never
    /// an error of the notice.
    pub async fn allocate<Tx>(
        &mut self,
        series: &mut BranchSeries,
        tx: &mut Tx,
        event_id: RecordId,
        at: OffsetDateTime,
    ) -> Result<Allocated, PartitionError>
    where
        W: OutboxWriter<Tx>,
        Tx: Send,
    {
        let number = series.allocate()?;
        let lease = series.lease().lease_id;
        let mut announced = None;
        if series.running_low() && self.notified != Some(lease) && self.pending != Some(lease) {
            let notice = RunningLowNotice {
                series: series.series().clone(),
                node: series.node().clone(),
                remaining: series.remaining(),
                total: series.lease().size(),
            };
            let written = match running_low_event(&notice, event_id, at) {
                Ok(event) => self.writer.write(tx, &event).await,
                Err(e) => Err(e),
            };
            match written {
                Ok(()) => {
                    self.pending = Some(lease);
                    announced = Some(lease);
                }
                Err(_) => self.record_failure(series, lease),
            }
        }
        Ok(Allocated { number, announced })
    }

    /// The transaction that issued `allocated` committed: its lease is announced for good.
    pub fn committed(&mut self, allocated: &Allocated) {
        if let Some(lease) = allocated.announced {
            if self.pending == Some(lease) {
                self.pending = None;
            }
            self.notified = Some(lease);
        }
    }

    /// The transaction that issued `allocated` rolled back: its event is gone, so the lease
    /// stays armed and the next allocation announces it.
    pub fn rolled_back(&mut self, allocated: &Allocated) {
        if let Some(lease) = allocated.announced
            && self.pending == Some(lease)
        {
            self.pending = None;
        }
    }

    /// How many running-low notices could not be written.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures
    }

    /// The `partition.range_low_not_recorded` to log, once per lease; `None` when there is
    /// nothing new to report.
    pub fn take_report(&mut self) -> Option<PartitionError> {
        self.report.take()
    }

    fn record_failure(&mut self, series: &BranchSeries, lease: LeaseId) {
        self.failures = self.failures.saturating_add(1);
        if self.reported != Some(lease) {
            self.reported = Some(lease);
            self.report = Some(PartitionError::RangeLowNotRecorded {
                series: series.series().as_str().to_owned(),
                node: series.node().to_string(),
            });
        }
    }
}
