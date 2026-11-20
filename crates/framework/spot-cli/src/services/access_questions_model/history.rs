//! The permission-change history: a typed reader and writer over the tenant's access-change
//! journal, which is a chain of its own in the platform's hash-chained audit store
//! (`spot-audit-chain`, the store `spot-doc-audit` and the identity trail write to).
//!
//! # The journal
//!
//! - **Scope**: [`scope_of`] (`access-changes:<site>`), a chain apart from the sign-in trail, so the
//!   scan of "what changed between two dates" reads only permission changes and verifies on its own.
//! - **Entry**: kind [`KIND`], the actor as `user:<id>` or `system`, the time in microseconds and two
//!   payload values: the format version ([`FORMAT`]) and the event as the canonical JSON of a
//!   `spot_iam_governance::ChangeEvent`. An event over [`MAX_EVENT_BYTES`] is written without its
//!   before and after snapshots and flagged `trimmed`: the entry never fails for size, and never
//!   carries more than the 8 KiB the chain allows.
//! - **Writing**: [`record`] appends one event; it is the one way a hosted flow (a grant, an
//!   approval, an emergency start or end) lets the history question see its change.
//! - **Reading**: [`read_window`] finds the first entry of the window by binary search on the entry
//!   time, scans forward in pages and stops after the window plus [`ReadLimits::skew_seconds`] of
//!   clock skew. An entry it cannot interpret (unknown format version, not JSON, not an event) is
//!   skipped and counted, never repaired; a window that needs more than
//!   [`ReadLimits::max_scanned`] entries is refused ([`ModelError::HistoryTooLarge`]) instead of
//!   answered in part.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_audit_chain::{
    AuditChain, AuditError, AuditRecord, AuditStore, CheckpointPolicy, CheckpointSigner,
    InMemoryStore, PayloadValue,
};
use spot_http::api::service::BoxFuture;
use spot_iam_governance::{ChangeActor, ChangeEvent};

use super::ModelError;

/// The fixed prefix of an access-change journal's chain scope; the site name follows.
pub const SCOPE_PREFIX: &str = "access-changes:";
/// The kind of a journal entry.
pub const KIND: &str = "access_change";
/// The payload format this module writes and reads.
pub const FORMAT: i64 = 1;
/// The largest event, in encoded bytes, an entry carries with its snapshots.
pub const MAX_EVENT_BYTES: usize = 6 * 1024;
const VERSION_KEY: &str = "v";
const EVENT_KEY: &str = "event";
const TRIMMED_KEY: &str = "trimmed";
const MAX_ID: usize = 200;

/// The journal scope of `site`.
#[must_use]
pub fn scope_of(site: &str) -> String {
    format!("{SCOPE_PREFIX}{site}")
}

/// Why the journal could not be read or written. The text of a store failure is dropped: it can
/// name a table or a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalError {
    /// The store failed or is not prepared.
    Unavailable,
    /// The event cannot be recorded (a blank actor, an actor too long, too large after trimming).
    Invalid,
}

/// One journal entry, reduced to what the reader interprets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalEntry {
    /// The chain position, from 1.
    pub seq: u64,
    /// The entry time, in microseconds.
    pub at_micros: i64,
    /// The entry kind.
    pub kind: String,
    /// The payload format version, when present.
    pub version: Option<i64>,
    /// The event's JSON text, when present.
    pub event: Option<String>,
}

/// The tenant's access-change journal.
pub trait ChangeJournal: Send + Sync {
    /// The newest entry's sequence number, 0 for an empty journal.
    fn head<'a>(&'a self) -> BoxFuture<'a, Result<u64, JournalError>>;
    /// The entries with `from <= seq <= to`, in order.
    fn range<'a>(
        &'a self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'a, Result<Vec<JournalEntry>, JournalError>>;
    /// Appends one event; the sequence number of the new entry.
    fn append<'a>(&'a self, event: &'a ChangeEvent) -> BoxFuture<'a, Result<u64, JournalError>>;
}

/// A signer that never signs: the journal takes no automatic checkpoint (its policy is off), so
/// the signer is never asked, and a refusal keeps an unsigned checkpoint from being written.
struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }
    fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, AuditError> {
        Err(AuditError::Signer("no checkpoint key is configured".into()))
    }
}

/// The journal over an [`AuditStore`]: PostgreSQL in production, the in-memory store in tests.
pub struct ChainJournal<S: AuditStore> {
    chain: AuditChain<S>,
}

impl<S: AuditStore> ChainJournal<S> {
    /// The journal of `site` over `store`.
    #[must_use]
    pub fn new(store: S, site: &str) -> Self {
        Self {
            chain: AuditChain::new(
                store,
                scope_of(site),
                Arc::new(NoSigner),
                CheckpointPolicy { every: 0 },
            ),
        }
    }
}

impl ChainJournal<InMemoryStore> {
    /// An empty in-memory journal, for tests and harnesses.
    #[must_use]
    pub fn in_memory(site: &str) -> Self {
        Self::new(InMemoryStore::new(), site)
    }
}

fn entry_of(entry: &spot_audit_chain::ChainEntry) -> JournalEntry {
    let text = |key: &str| match entry.record.payload.get(key) {
        Some(PayloadValue::Text(t)) => Some(t.clone()),
        _ => None,
    };
    JournalEntry {
        seq: entry.seq,
        at_micros: entry.record.at,
        kind: entry.record.kind.clone(),
        version: match entry.record.payload.get(VERSION_KEY) {
            Some(PayloadValue::Int(v)) => Some(*v),
            _ => None,
        },
        event: text(EVENT_KEY),
    }
}

impl<S: AuditStore + 'static> ChangeJournal for ChainJournal<S> {
    fn head<'a>(&'a self) -> BoxFuture<'a, Result<u64, JournalError>> {
        Box::pin(async move {
            self.chain
                .head()
                .await
                .map(|h| h.map_or(0, |e| e.seq))
                .map_err(|_| JournalError::Unavailable)
        })
    }

    fn range<'a>(
        &'a self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'a, Result<Vec<JournalEntry>, JournalError>> {
        Box::pin(async move {
            let entries = self
                .chain
                .read_range(from, to)
                .await
                .map_err(|_| JournalError::Unavailable)?;
            Ok(entries.iter().map(entry_of).collect())
        })
    }

    fn append<'a>(&'a self, event: &'a ChangeEvent) -> BoxFuture<'a, Result<u64, JournalError>> {
        Box::pin(async move {
            let record = record_of(event)?;
            self.chain
                .append(record)
                .await
                .map(|e| e.seq)
                .map_err(|_| JournalError::Unavailable)
        })
    }
}

/// What an event becomes in the journal.
///
/// # Errors
///
/// [`JournalError::Invalid`] for a blank or over-long actor, or an event that does not fit the
/// chain even without its snapshots.
pub fn record_of(event: &ChangeEvent) -> Result<AuditRecord, JournalError> {
    let actor = match &event.actor {
        ChangeActor::User { id, .. } => {
            if id.trim().is_empty() || id.len() > MAX_ID {
                return Err(JournalError::Invalid);
            }
            format!("user:{id}")
        }
        ChangeActor::System { .. } => "system".to_owned(),
    };
    let mut payload = BTreeMap::new();
    payload.insert(VERSION_KEY.to_owned(), PayloadValue::Int(FORMAT));
    let mut json = serde_json::to_string(event).map_err(|_| JournalError::Invalid)?;
    if json.len() > MAX_EVENT_BYTES {
        let mut slim = event.clone();
        slim.before = None;
        slim.after = None;
        slim.scope = None;
        json = serde_json::to_string(&slim).map_err(|_| JournalError::Invalid)?;
        if json.len() > MAX_EVENT_BYTES {
            return Err(JournalError::Invalid);
        }
        payload.insert(TRIMMED_KEY.to_owned(), PayloadValue::Bool(true));
    }
    payload.insert(EVENT_KEY.to_owned(), PayloadValue::Text(json));
    let record = AuditRecord {
        kind: KIND.to_owned(),
        actor,
        on_behalf_of: None,
        subject: None,
        at: event.occurred_at.saturating_mul(1_000_000),
        payload,
        // A change event names roles, users and rule ids: never a protected value.
        masked: true,
    };
    record.validate().map_err(|_| JournalError::Invalid)?;
    Ok(record)
}

/// Appends `event` to `journal`.
///
/// # Errors
///
/// [`JournalError`]: the caller decides whether the change may proceed without its audit record;
/// every hosted flow of this repo refuses (fail closed).
pub async fn record(journal: &dyn ChangeJournal, event: &ChangeEvent) -> Result<u64, JournalError> {
    journal.append(event).await
}

/// How much a window read may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    /// Entries fetched per call.
    pub page: u64,
    /// The most entries one read scans.
    pub max_scanned: u64,
    /// The clock skew tolerated at both ends of the window, in seconds.
    pub skew_seconds: i64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            page: 500,
            max_scanned: 100_000,
            skew_seconds: 86_400,
        }
    }
}

/// What one window read found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryRead {
    /// The events whose time is in the window, in journal order.
    pub events: Vec<ChangeEvent>,
    /// Journal entries of the access-change kind that could not be interpreted.
    pub skipped: u64,
    /// Entries scanned.
    pub scanned: u64,
}

fn unavailable(_: JournalError) -> ModelError {
    ModelError::HistoryUnavailable
}

fn micros(seconds: i64) -> i64 {
    seconds.saturating_mul(1_000_000)
}

/// Reads the events whose time is in `[from, to]` (epoch seconds, inclusive).
///
/// # Errors
///
/// [`ModelError::RangeInvalid`] when `to < from`; [`ModelError::HistoryUnavailable`] when the
/// journal cannot be read or has a gap; [`ModelError::HistoryTooLarge`] when the window needs more
/// than [`ReadLimits::max_scanned`] entries.
pub async fn read_window(
    journal: &dyn ChangeJournal,
    from: i64,
    to: i64,
    limits: &ReadLimits,
) -> Result<HistoryRead, ModelError> {
    if to < from {
        return Err(ModelError::RangeInvalid);
    }
    let page = limits.page.clamp(1, 5_000);
    let head = journal.head().await.map_err(unavailable)?;
    let mut out = HistoryRead::default();
    if head == 0 {
        return Ok(out);
    }
    let start = micros(from.saturating_sub(limits.skew_seconds));
    let end = micros(to.saturating_add(limits.skew_seconds));

    // The first entry whose time is not before the start of the window, by binary search.
    let (mut lo, mut hi) = (1_u64, head.saturating_add(1));
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let found = journal.range(mid, mid).await.map_err(unavailable)?;
        let Some(entry) = found.first() else {
            return Err(ModelError::HistoryUnavailable);
        };
        if entry.at_micros < start {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    let mut seq = lo;
    'scan: while seq <= head {
        let last = seq.saturating_add(page - 1).min(head);
        let chunk = journal.range(seq, last).await.map_err(unavailable)?;
        if chunk.is_empty() {
            return Err(ModelError::HistoryUnavailable);
        }
        for entry in &chunk {
            if entry.at_micros > end {
                break 'scan;
            }
            out.scanned += 1;
            if out.scanned > limits.max_scanned {
                return Err(ModelError::HistoryTooLarge);
            }
            if entry.kind != KIND {
                continue;
            }
            match interpret(entry) {
                Some(event) if event.occurred_at >= from && event.occurred_at <= to => {
                    out.events.push(event);
                }
                Some(_) => {}
                None => out.skipped += 1,
            }
        }
        let Some(next) = chunk.last().map(|e| e.seq.saturating_add(1)) else {
            break;
        };
        if next <= seq {
            return Err(ModelError::HistoryUnavailable);
        }
        seq = next;
    }
    Ok(out)
}

/// The event of one entry, when the entry is in a format this module reads.
#[must_use]
pub fn interpret(entry: &JournalEntry) -> Option<ChangeEvent> {
    if entry.version != Some(FORMAT) {
        return None;
    }
    serde_json::from_str::<ChangeEvent>(entry.event.as_deref()?).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_iam_governance::ChangeOp;
    use spot_iam_governance::Snapshot as GovSnapshot;
    use spot_iam_roles::Source;

    use super::*;

    fn grant_event(id: &str, op: ChangeOp, at: i64) -> ChangeEvent {
        ChangeEvent::grant(
            id,
            "u-ben",
            "Reviewer",
            Source::Manual,
            op,
            ChangeActor::user("u-admin"),
            at,
        )
    }

    struct FixedJournal(Vec<JournalEntry>);

    impl ChangeJournal for FixedJournal {
        fn head<'a>(&'a self) -> BoxFuture<'a, Result<u64, JournalError>> {
            Box::pin(async move { Ok(self.0.len() as u64) })
        }
        fn range<'a>(
            &'a self,
            from: u64,
            to: u64,
        ) -> BoxFuture<'a, Result<Vec<JournalEntry>, JournalError>> {
            Box::pin(async move {
                Ok(self
                    .0
                    .iter()
                    .filter(|e| e.seq >= from && e.seq <= to)
                    .cloned()
                    .collect())
            })
        }
        fn append<'a>(&'a self, _: &'a ChangeEvent) -> BoxFuture<'a, Result<u64, JournalError>> {
            Box::pin(async { Err(JournalError::Unavailable) })
        }
    }

    fn entry(
        seq: u64,
        at: i64,
        kind: &str,
        version: Option<i64>,
        event: Option<String>,
    ) -> JournalEntry {
        JournalEntry {
            seq,
            at_micros: at * 1_000_000,
            kind: kind.to_owned(),
            version,
            event,
        }
    }

    #[tokio::test]
    async fn an_entry_the_reader_cannot_interpret_is_skipped_and_counted_never_invented() {
        let good = serde_json::to_string(&grant_event("a-1", ChangeOp::Created, 1_000)).unwrap();
        let journal = FixedJournal(vec![
            entry(1, 1_000, KIND, Some(FORMAT), Some(good.clone())),
            entry(2, 1_001, KIND, Some(FORMAT + 1), Some(good.clone())),
            entry(3, 1_002, KIND, Some(FORMAT), Some("{not json".to_owned())),
            entry(4, 1_003, KIND, Some(FORMAT), None),
            // Another kind is not a permission change: ignored, not counted.
            entry(5, 1_004, "sign_in", None, None),
        ]);
        let read = read_window(&journal, 900, 1_100, &ReadLimits::default())
            .await
            .unwrap();
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].subject, "a-1");
        assert_eq!(read.skipped, 3);
    }

    #[tokio::test]
    async fn the_reader_uses_the_window_and_refuses_one_that_is_too_large_or_malformed() {
        let event = |i: u64| {
            serde_json::to_string(&grant_event(
                &format!("a-{i}"),
                ChangeOp::Created,
                i64::try_from(i).unwrap() * 100,
            ))
            .unwrap()
        };
        let journal = FixedJournal(
            (1..=50)
                .map(|i| {
                    entry(
                        i,
                        i64::try_from(i).unwrap() * 100,
                        KIND,
                        Some(FORMAT),
                        Some(event(i)),
                    )
                })
                .collect(),
        );
        let limits = ReadLimits {
            page: 7,
            max_scanned: 1_000,
            skew_seconds: 0,
        };
        let read = read_window(&journal, 1_000, 1_450, &limits).await.unwrap();
        let subjects: Vec<&str> = read.events.iter().map(|e| e.subject.as_str()).collect();
        assert_eq!(subjects, vec!["a-10", "a-11", "a-12", "a-13", "a-14"]);
        assert!(
            read.scanned <= 8,
            "the binary search skips the first entries"
        );

        let tight = ReadLimits {
            max_scanned: 2,
            ..limits
        };
        assert_eq!(
            read_window(&journal, 1_000, 1_450, &tight).await.err(),
            Some(ModelError::HistoryTooLarge)
        );
        assert_eq!(
            read_window(&journal, 5, 4, &limits).await.err(),
            Some(ModelError::RangeInvalid)
        );
    }

    #[tokio::test]
    async fn a_recorded_event_is_read_back_and_an_oversized_one_loses_its_snapshots() {
        let journal = ChainJournal::in_memory("site-b");
        let small = grant_event("a-1", ChangeOp::Created, 5_000).with_after(
            GovSnapshot::from_value(serde_json::json!({"role": "Reviewer"})),
        );
        record(&journal, &small).await.unwrap();
        let big = ChangeEvent::role(
            "Reviewer",
            ChangeOp::Updated,
            "custom",
            ChangeActor::system("migration"),
            5_001,
        )
        .with_after(GovSnapshot::from_value(
            serde_json::json!({"padding": "x".repeat(7_000)}),
        ));
        record(&journal, &big).await.unwrap();
        let entries = journal.range(1, 2).await.unwrap();
        assert_eq!(
            interpret(&entries[0]).unwrap(),
            small,
            "a small event is kept whole"
        );
        let kept = interpret(&entries[1]).unwrap();
        assert_eq!(kept.subject, "Reviewer");
        assert!(
            kept.after.is_none(),
            "the snapshot is dropped, the change is not"
        );
        let encoded = record_of(&big).unwrap();
        assert_eq!(
            encoded.payload.get("trimmed"),
            Some(&PayloadValue::Bool(true))
        );
        assert_eq!(encoded.actor, "system");
    }

    #[tokio::test]
    async fn an_event_with_a_blank_actor_is_refused_not_recorded() {
        let journal = ChainJournal::in_memory("site-c");
        let event = ChangeEvent::role("R", ChangeOp::Created, "custom", ChangeActor::user("  "), 1);
        assert_eq!(record(&journal, &event).await, Err(JournalError::Invalid));
        assert_eq!(journal.head().await, Ok(0));
    }

    #[tokio::test]
    async fn the_journal_of_one_site_is_not_the_journal_of_another() {
        // Two journals over one store keep apart by scope: here, two stores, one event each.
        let a = ChainJournal::in_memory("site-a");
        let b = ChainJournal::in_memory("site-b");
        record(&a, &grant_event("a-1", ChangeOp::Created, 10))
            .await
            .unwrap();
        assert_eq!(a.head().await, Ok(1));
        assert_eq!(b.head().await, Ok(0));
        assert_eq!(
            read_window(&b, 0, 100, &ReadLimits::default())
                .await
                .unwrap()
                .events
                .len(),
            0
        );
    }
}
