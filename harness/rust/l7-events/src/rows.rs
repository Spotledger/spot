//! Audit and job-enqueued events against their rows (WP-TST-07.T5): one event per row, same key.
//!
//! `sys_doc_audit` and `sys_job_enqueued` are outbox topics that the engine's steps write in the
//! business transaction; the thing each one describes is kept elsewhere (the audit chain's row of
//! a document change; the job store's row of an enqueued job). The conformance check reads both
//! sides through a [`RowSource`] and compares them as sets of keys:
//!
//! - an **audit** row is keyed `<subject id>:<version>` (the document and the revision the
//!   change made); its event is keyed the same from the event's subject and `subject_version`;
//! - a **job** row is keyed by the job's idempotency key; its event is keyed by the payload's
//!   `idempotency_key`, or by the event id when the payload has none.
//!
//! A row with no event, an event with no row, and a key that appears more than once on either
//! side are each a violation naming the topic and the key (keys are ids, never content).

use std::collections::BTreeMap;

use spot_events_contract::PayloadDocument;
use spot_l7_invariants::invariant::{BoxFuture, CheckContext, Invariant, Violation};
use spot_storage_contract::{OutboxRecord, StorageResult};

/// Id of [`RowsMatch`].
pub const ROWS: &str = "events.rows_match";

/// The audit event topic.
pub const AUDIT_TOPIC: &str = "sys_doc_audit";
/// The job-enqueued event topic.
pub const JOB_TOPIC: &str = "sys_job_enqueued";

/// Where the rows come from: the audit chain and the job store of the run.
pub trait RowSource: Send + Sync {
    /// The key of every audit row, `<subject id>:<version>`.
    fn audit_rows(&self) -> BoxFuture<'_, StorageResult<Vec<String>>>;
    /// The idempotency key of every job row.
    fn job_rows(&self) -> BoxFuture<'_, StorageResult<Vec<String>>>;
}

/// The key of an audit event.
pub fn audit_event_key(rec: &OutboxRecord) -> String {
    let version = rec.event.subject_version.map_or(0, |v| v.0);
    format!("{}:{version}", rec.event.subject.id)
}

/// The key of a job-enqueued event: the payload's `idempotency_key`, else the event id.
pub fn job_event_key(rec: &OutboxRecord) -> String {
    PayloadDocument::from_outbox_event(&rec.event)
        .ok()
        .and_then(|doc| {
            doc.data
                .get("idempotency_key")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| rec.event.id.to_string())
}

/// What is wrong with one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mismatch {
    /// A row has no event.
    MissingEvent(String),
    /// An event has no row.
    ExtraEvent(String),
    /// The key appears `n` times among the rows.
    DuplicateRow(String, usize),
    /// The key appears `n` times among the events.
    DuplicateEvent(String, usize),
}

impl Mismatch {
    /// The stable code and the key.
    pub fn parts(&self) -> (&'static str, &str) {
        match self {
            Mismatch::MissingEvent(k) => ("row_without_event", k),
            Mismatch::ExtraEvent(k) => ("event_without_row", k),
            Mismatch::DuplicateRow(k, _) => ("duplicate_row", k),
            Mismatch::DuplicateEvent(k, _) => ("duplicate_event", k),
        }
    }
}

fn counts(keys: &[String]) -> BTreeMap<&str, usize> {
    let mut map = BTreeMap::new();
    for key in keys {
        *map.entry(key.as_str()).or_insert(0) += 1;
    }
    map
}

/// Compares the keys of the rows with the keys of the events: one event per row, same key.
pub fn compare(rows: &[String], events: &[String]) -> Vec<Mismatch> {
    let (r, e) = (counts(rows), counts(events));
    let mut out = Vec::new();
    for (key, n) in &r {
        if *n > 1 {
            out.push(Mismatch::DuplicateRow((*key).to_owned(), *n));
        }
        if !e.contains_key(key) {
            out.push(Mismatch::MissingEvent((*key).to_owned()));
        }
    }
    for (key, n) in &e {
        if *n > 1 {
            out.push(Mismatch::DuplicateEvent((*key).to_owned(), *n));
        }
        if !r.contains_key(key) {
            out.push(Mismatch::ExtraEvent((*key).to_owned()));
        }
    }
    out
}

/// `events.rows_match`: every `sys_doc_audit` and `sys_job_enqueued` event has its row and every
/// row its event, once each.
pub struct RowsMatch {
    source: std::sync::Arc<dyn RowSource>,
}

impl RowsMatch {
    /// The invariant over `source`.
    pub fn new(source: std::sync::Arc<dyn RowSource>) -> Self {
        Self { source }
    }

    async fn run(&self, ctx: &CheckContext) -> StorageResult<Vec<Violation>> {
        let outbox = ctx.outbox().await?;
        let mut out = Vec::new();
        let audit_events: Vec<String> = outbox
            .iter()
            .filter(|r| r.event.topic.as_str() == AUDIT_TOPIC)
            .map(audit_event_key)
            .collect();
        let job_events: Vec<String> = outbox
            .iter()
            .filter(|r| r.event.topic.as_str() == JOB_TOPIC)
            .map(job_event_key)
            .collect();
        for (topic, rows, events) in [
            (AUDIT_TOPIC, self.source.audit_rows().await?, audit_events),
            (JOB_TOPIC, self.source.job_rows().await?, job_events),
        ] {
            for mismatch in compare(&rows, &events) {
                let (code, key) = mismatch.parts();
                out.push(Violation::new(ROWS, topic, key, code));
            }
        }
        Ok(out)
    }
}

impl Invariant for RowsMatch {
    fn id(&self) -> &str {
        ROWS
    }
    fn describe(&self) -> &str {
        "Every audit and job-enqueued event has its row (same key) and every row its event, once \
         each: no lost audit entry, no phantom event, no duplicate."
    }
    fn check<'a>(&'a self, ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
        Box::pin(async move {
            match self.run(ctx).await {
                Ok(v) => v,
                Err(e) => vec![Violation::read_failed(ROWS, "rows", &e)],
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn matching_rows_and_events_have_no_mismatch() {
        assert!(compare(&keys(&["a:1", "b:2"]), &keys(&["b:2", "a:1"])).is_empty());
    }

    #[test]
    fn a_missing_event_is_reported_with_its_key() {
        assert_eq!(
            compare(&keys(&["a:1", "b:2"]), &keys(&["a:1"])),
            [Mismatch::MissingEvent("b:2".into())]
        );
    }

    #[test]
    fn an_extra_event_is_reported_with_its_key() {
        assert_eq!(
            compare(&keys(&["a:1"]), &keys(&["a:1", "c:3"])),
            [Mismatch::ExtraEvent("c:3".into())]
        );
    }

    #[test]
    fn a_duplicated_event_or_row_is_reported() {
        let found = compare(&keys(&["a:1"]), &keys(&["a:1", "a:1"]));
        assert_eq!(found, [Mismatch::DuplicateEvent("a:1".into(), 2)]);
        let found = compare(&keys(&["a:1", "a:1"]), &keys(&["a:1"]));
        assert_eq!(found, [Mismatch::DuplicateRow("a:1".into(), 2)]);
    }
}
