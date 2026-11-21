//! Shared fakes for the replay tests: a fixed journal, a toy document engine over the storage
//! fake, a held gate and a stepping clock. No real database is involved.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_app_api::prelude::{Decimal, Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_app_api::{Actor, Document};
use spot_core::{Clock, ManualClock};
use spot_effect_hold::{
    Admission, EffectClass, Gate, HoldState, InMemoryLedger, LedgerStore, MigrationId, PolicyTable,
};
use spot_replay::{
    BoxFuture, CloneRef, Cursor, Error, JournalEntry, JournalPage, LedgerEntry, Outcome, Principal,
    ReplayOperation, ReplayRequest, ReplayRunner, ReplaySource, Result, Sensitivity, Status,
};
use spot_storage_contract::{
    Catalog, DocumentData, DocumentStore, Isolation, RowLock, StorageBackend, Transaction,
    TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;

/// A site id for tests.
pub fn site() -> SiteId {
    SiteId::new("acme").expect("site id")
}

/// The clone a replay writes to.
pub fn clone_ref() -> CloneRef {
    CloneRef::new("acme", "spot_scratch")
}

/// The label of the live site a replay must never touch.
pub const LIVE_LABEL: &str = "acme_live";

/// The instant every test journal is measured from.
pub fn reference_at() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("reference instant")
}

/// A gate over a held site, with the ledger it holds effects in.
pub async fn held_gate() -> (Arc<Gate>, Arc<InMemoryLedger>, SiteId) {
    let site = site();
    let ledger = Arc::new(InMemoryLedger::new());
    ledger
        .compare_and_set_hold(
            &site,
            &HoldState::Open,
            HoldState::Held {
                migration_id: MigrationId::new("mig-1").expect("migration id"),
                reason: "rehearsal".to_owned(),
                since: 0,
                expected_by: None,
            },
        )
        .await
        .expect("hold");
    let gate = Arc::new(Gate::new(
        site.clone(),
        Arc::clone(&ledger) as Arc<dyn LedgerStore>,
        Arc::new(PolicyTable::default()),
    ));
    (gate, ledger, site)
}

/// The tax rounding a toy release applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// Round half up.
    HalfUp,
    /// Truncate towards zero.
    Floor,
}

/// A toy release: how it computes tax from a subtotal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToyRelease {
    /// The rounding the release applies to tax.
    pub rounding: Rounding,
}

impl ToyRelease {
    /// The release the site runs before the upgrade.
    pub fn old() -> Self {
        ToyRelease {
            rounding: Rounding::HalfUp,
        }
    }

    /// A release with no behaviour change: the same rounding.
    pub fn unchanged() -> Self {
        Self::old()
    }

    /// The new release with a **seeded rounding change** in its tax calculation.
    pub fn seeded_rounding_change() -> Self {
        ToyRelease {
            rounding: Rounding::Floor,
        }
    }

    /// Tax on `base` minor units at 5%, in minor units, under this release's rounding.
    ///
    /// `base = 1010` gives `50` under `Floor` and `51` under `HalfUp`: exactly one minor unit.
    pub fn tax(&self, base: i128) -> i128 {
        let numerator = base * 500;
        let denominator = 10_000;
        match self.rounding {
            Rounding::HalfUp => (numerator + denominator / 2) / denominator,
            Rounding::Floor => numerator / denominator,
        }
    }
}

/// A `party` document with `credit_limit` = `base` minor units, for a journal entry.
pub fn party_doc(sequence: u64, base: i128) -> Document {
    let mut doc = Document::new(name("party"), RecordId::new_v7());
    doc.set(name("name"), format!("party-{sequence}"));
    doc.set(
        name("credit_limit"),
        Value::Decimal(Decimal::new(i64::try_from(base).expect("base fits i64"), 2)),
    );
    doc
}

/// A `party` create operation for entry `sequence`, with the outcome the **old** release records.
pub fn journal_entry(sequence: u64, base: i128, old: &ToyRelease) -> JournalEntry {
    let doc = party_doc(sequence, base);
    let record = doc.id();
    let tax = old.tax(base);
    let recorded = Outcome::new(
        "party",
        record.to_string(),
        Status::Created,
        BTreeMap::from([
            ("subtotal".to_owned(), base),
            ("tax".to_owned(), tax),
            ("total".to_owned(), base + tax),
        ]),
        vec![LedgerEntry::new("tax_payable", 0, tax)],
        Vec::new(),
        Vec::new(),
    );
    JournalEntry {
        source: "party".to_owned(),
        sequence,
        doctype: "party".to_owned(),
        record,
        operation: ReplayOperation::Create { document: doc },
        occurred_at: OffsetDateTime::from_unix_timestamp(
            1_800_000_000 + i64::try_from(sequence).expect("sequence fits i64"),
        )
        .expect("instant"),
        principal: Principal::system(name("replay_test")),
        sensitivity: Sensitivity::Normal,
        recorded,
    }
}

/// A journal of `count` entries, all with base `1010`, recorded by `old`.
pub fn journal(count: u64, old: &ToyRelease) -> Vec<JournalEntry> {
    (1..=count)
        .map(|seq| journal_entry(seq, 1010, old))
        .collect()
}

fn name(value: &str) -> Name {
    Name::new(value).expect("storage name")
}

// ---------------------------------------------------------------------------------------------
// The journal source
// ---------------------------------------------------------------------------------------------

/// A [`ReplaySource`] over a fixed, already-sorted list of entries.
pub struct FixedJournal {
    entries: Vec<JournalEntry>,
}

impl FixedJournal {
    /// A journal over `entries`, sorted into causal order.
    pub fn new(mut entries: Vec<JournalEntry>) -> Self {
        entries.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
        FixedJournal { entries }
    }

    /// The entries it holds.
    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }
}

impl ReplaySource for FixedJournal {
    fn read<'a>(
        &'a self,
        cursor: Option<&'a Cursor>,
        limit: usize,
    ) -> BoxFuture<'a, Result<JournalPage>> {
        Box::pin(async move {
            let start = match cursor {
                None => 0,
                Some(cursor) => self
                    .entries
                    .iter()
                    .position(|e| e.source == cursor.source && e.sequence == cursor.sequence)
                    .map_or(self.entries.len(), |index| index + 1),
            };
            let page: Vec<JournalEntry> =
                self.entries[start..].iter().take(limit).cloned().collect();
            let next = page.last().map(Cursor::of);
            let exhausted = start + page.len() >= self.entries.len();
            Ok(JournalPage {
                entries: page,
                next,
                exhausted,
            })
        })
    }
}

/// A [`ReplaySource`] that always fails, to prove a read failure aborts the run.
pub struct FailingJournal;

impl ReplaySource for FailingJournal {
    fn read<'a>(
        &'a self,
        _cursor: Option<&'a Cursor>,
        _limit: usize,
    ) -> BoxFuture<'a, Result<JournalPage>> {
        Box::pin(async move {
            Err(Error::JournalFailed {
                reason: "seeded journal failure".to_owned(),
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The toy document engine
// ---------------------------------------------------------------------------------------------

/// A [`ReplayRunner`] standing in for the document engine on the clone: it writes each party to
/// the clone's storage fake, attempts one external effect through the hold, and returns the new
/// release's outcome.
pub struct ToyEngine {
    release: ToyRelease,
    catalog: Arc<Catalog>,
    stores: BTreeMap<String, FakeBackend>,
    targets: Mutex<Vec<String>>,
    writes: Mutex<Vec<(String, RecordId)>>,
    attempts: AtomicUsize,
    held: AtomicUsize,
    refused: AtomicUsize,
    sends: AtomicUsize,
}

impl ToyEngine {
    /// A toy engine over two stores: the clone (`clone_label`) and the live site (`live_label`).
    pub async fn new(release: ToyRelease, clone_label: &str, live_label: &str) -> Self {
        let catalog = Arc::new(contract_tests::catalog().expect("storage catalog"));
        let mut stores = BTreeMap::new();
        for label in [clone_label, live_label] {
            let backend = FakeBackend::new();
            backend.prepare(&catalog).await.expect("prepare");
            stores.insert(label.to_owned(), backend);
        }
        ToyEngine {
            release,
            catalog,
            stores,
            targets: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            attempts: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
        }
    }

    /// Every clone label the engine was handed, in order.
    pub fn targets(&self) -> Vec<String> {
        self.targets.lock().expect("targets lock").clone()
    }

    /// Every write as `(label, record)`.
    pub fn writes(&self) -> Vec<(String, RecordId)> {
        self.writes.lock().expect("writes lock").clone()
    }

    /// How many external effects were attempted through the hold.
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    /// How many were recorded and held.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }

    /// How many the hold refused outright (a never-held class).
    pub fn refused(&self) -> usize {
        self.refused.load(Ordering::SeqCst)
    }

    /// How many effects actually left: the sentinel. Always zero in a replay.
    pub fn sends(&self) -> usize {
        self.sends.load(Ordering::SeqCst)
    }

    /// Whether `label`'s store holds the document `record`.
    pub async fn has_document(&self, label: &str, record: RecordId) -> bool {
        let backend = self.stores.get(label).expect("store");
        let mut tx = backend
            .begin(
                self.catalog.clone(),
                TxOptions::new(name("replay_check"), Isolation::ReadCommitted).read_only(),
            )
            .await
            .expect("begin");
        let found = tx
            .load(&name("party"), record, RowLock::None)
            .await
            .expect("load");
        let _ = tx.rollback().await;
        found.is_some()
    }

    /// Writes one party to the clone: the whole declared field set, with `credit_limit` set to the
    /// new release's total.
    async fn write_party(
        &self,
        label: &str,
        sequence: u64,
        record: RecordId,
        total: i128,
    ) -> Result<()> {
        let backend = self.stores.get(label).ok_or_else(|| Error::NotClone {
            label: label.to_owned(),
        })?;
        let mut fields: BTreeMap<Name, Value> = BTreeMap::new();
        fields.insert(name("name"), Value::Text(format!("party-{sequence}")));
        fields.insert(
            name("credit_limit"),
            Value::Decimal(Decimal::new(
                i64::try_from(total).expect("total fits i64"),
                2,
            )),
        );
        for nullable in [
            "parent_party",
            "note",
            "since",
            "opens_at",
            "seen_at",
            "active",
            "visits",
            "profile",
            "logo",
        ] {
            fields.insert(name(nullable), Value::Null);
        }
        let data = DocumentData {
            entity: name("party"),
            id: record,
            fields,
            children: BTreeMap::new(),
        };
        let mut tx = backend
            .begin(
                self.catalog.clone(),
                TxOptions::new(name("party_create"), Isolation::ReadCommitted),
            )
            .await
            .map_err(|e| Error::ExecutionFailed {
                doctype: "party".to_owned(),
                record: record.to_string(),
                reason: e.to_string(),
            })?;
        tx.insert(&data).await.map_err(|e| Error::ExecutionFailed {
            doctype: "party".to_owned(),
            record: record.to_string(),
            reason: e.to_string(),
        })?;
        tx.commit().await.map_err(|e| Error::ExecutionFailed {
            doctype: "party".to_owned(),
            record: record.to_string(),
            reason: e.to_string(),
        })?;
        self.writes
            .lock()
            .expect("writes lock")
            .push((label.to_owned(), record));
        Ok(())
    }
}

impl ReplayRunner for ToyEngine {
    fn run<'a>(&'a self, request: ReplayRequest<'a>) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            self.targets
                .lock()
                .expect("targets lock")
                .push(request.clone.label().to_owned());
            let ReplayOperation::Create { document } = request.operation() else {
                return Err(Error::ExecutionFailed {
                    doctype: request.doctype().to_owned(),
                    record: request.entry.record_text(),
                    reason: "the toy engine only creates parties".to_owned(),
                });
            };
            let base = document
                .decimal("credit_limit")
                .ok()
                .flatten()
                .map(|d| d.mantissa())
                .unwrap_or(0);
            let tax = self.release.tax(base);
            let total = base + tax;
            self.write_party(
                request.clone.label(),
                request.entry.sequence,
                request.entry.record,
                total,
            )
            .await?;

            // The old release would have sent the party a welcome mail. Attempt it through the
            // hold: a held class is recorded and deferred; nothing leaves.
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let effect = request.hold.effect(
                EffectClass::Mail,
                "party welcome",
                format!("party:{}", request.entry.record),
                0,
            );
            match request.hold.guard(&effect, 0).await {
                Ok(Admission::Held(_)) => {
                    self.held.fetch_add(1, Ordering::SeqCst);
                }
                Ok(Admission::Proceed) => {
                    // The only path that would transmit: a never-held class. A replay refuses it,
                    // so this is unreachable in practice; the sentinel would catch it if not.
                    self.sends.fetch_add(1, Ordering::SeqCst);
                }
                Err(_) => {
                    self.refused.fetch_add(1, Ordering::SeqCst);
                }
            }

            Ok(Outcome::new(
                "party",
                request.entry.record_text(),
                Status::Created,
                BTreeMap::from([
                    ("subtotal".to_owned(), base),
                    ("tax".to_owned(), tax),
                    ("total".to_owned(), total),
                ]),
                vec![LedgerEntry::new("tax_payable", 0, tax)],
                Vec::new(),
                Vec::new(),
            ))
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------------------------

/// A clock whose monotonic reading advances by `step` on every read, so a duration budget can be
/// exercised without a real wait.
pub struct SteppingClock {
    step: Duration,
    reads: AtomicUsize,
}

impl SteppingClock {
    /// A clock that adds `step` on every monotonic read.
    pub fn new(step: Duration) -> Self {
        SteppingClock {
            step,
            reads: AtomicUsize::new(0),
        }
    }
}

impl Clock for SteppingClock {
    fn now_utc(&self) -> OffsetDateTime {
        reference_at()
    }

    fn monotonic(&self) -> Duration {
        let reads = self.reads.fetch_add(1, Ordering::SeqCst);
        self.step * u32::try_from(reads).unwrap_or(u32::MAX)
    }
}

/// A manual clock fixed at the reference instant.
pub fn manual_clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(reference_at()))
}

/// The actor a test principal uses.
pub fn user_actor() -> Actor {
    Actor::User {
        id: RecordId::new_v7(),
        via: None,
    }
}
