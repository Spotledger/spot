//! Driving the scenarios and checking what happened.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::context::{AMENDED_FROM, DocumentService, Invocation, Outcome};
use spot_app_api::lifecycle::op_name;
use spot_app_api::prelude::{Decimal, RecordId, Version};
use spot_app_api::{
    AppResult, BoxFuture, DocState, Document, EffectKind, EffectReceipt, OpaqueRef, Operation,
    OperationReceipt, StepName, StoreName,
};
use spot_seams::{DeclaredEffect, Registry, check_receipt};

use crate::failure::{Report, Violation, codes};
use crate::probe::{Snapshot, StoreFilter, StoreProbe, snapshot};
use crate::recorder::Recorder;

const REMEDY_REVERSE: &str = "add the reversing step to cancel: an after_cancel step writing the same store with the opposite sign, declared as reversing the post step";
const REMEDY_OVER: &str = "make the cancel step reverse exactly what post wrote, no more";
const REMEDY_APPEND: &str =
    "never delete or rewrite a store row; reverse a row by appending a new one";
const REMEDY_REPORT: &str = "report every row the step writes with ctx.report_effect, and declare the store in the step's effect declaration";
const REMEDY_OVER_REPORT: &str =
    "report only rows the step actually wrote; fix the count in the receipt";
const REMEDY_DECLARE: &str = "declare the effect on the step (class and kind) in Controller::step_effects, or stop reporting it";
const REMEDY_LINK: &str = "give each reversing row a reference to the posted row it reverses";
const REMEDY_RECEIPT_REFS: &str =
    "put the reversing rows' references in store_refs of the effect the cancel step reports";
const REMEDY_CANCEL_TWICE: &str = "refuse cancel unless the document is posted: let the lifecycle state check run before any step writes";
const REMEDY_CORRECT: &str = "correct must cancel the original and return a new draft whose amended_from names it; do not post or write stores in the draft";

/// What the app's factory builds for one scenario: the engine's document service, the seam
/// registry it was built from, and a probe over the app's stores.
pub struct World {
    /// The document engine, as the native API layer sees it.
    pub service: Arc<dyn DocumentService>,
    /// The registry the engine runs (the kit reads its declared effects).
    pub registry: Arc<Registry>,
    /// A read-only view of the app's append-only stores.
    pub probe: Arc<dyn StoreProbe>,
}

/// Builds a fresh [`World`] for each scenario. The app must pass every controller through
/// [`Recorder::wrap`] before giving it to the registry builder, so the kit sees the reports.
pub trait EngineFactory: Send + Sync {
    /// A fresh, empty engine and stores.
    fn build(&self, recorder: Recorder) -> BoxFuture<'_, AppResult<World>>;
}

impl<F> EngineFactory for F
where
    F: Fn(Recorder) -> BoxFuture<'static, AppResult<World>> + Send + Sync,
{
    fn build(&self, recorder: Recorder) -> BoxFuture<'_, AppResult<World>> {
        self(recorder)
    }
}

/// One DocType to drive: how to make a valid draft of it, and who acts at what time.
#[derive(Clone)]
pub struct Scenario {
    /// The DocType; it must be posted (post and cancel are the operations under test).
    pub doctype: String,
    /// The invocation every operation runs under; its `now` is the kit's only clock.
    pub invocation: Invocation,
    draft: Arc<dyn Fn() -> Document + Send + Sync>,
}

impl Scenario {
    /// A scenario. `draft` returns a new valid draft (with a fresh id) each time it is called.
    pub fn new(
        doctype: &str,
        invocation: Invocation,
        draft: impl Fn() -> Document + Send + Sync + 'static,
    ) -> Self {
        Self {
            doctype: doctype.to_owned(),
            invocation,
            draft: Arc::new(draft),
        }
    }
}

/// Runs every scenario on a fresh engine and returns what was found. An `Err` means the kit
/// could not run (a factory or probe failure); a finding about the app is in the [`Report`].
pub async fn run(factory: &dyn EngineFactory, scenarios: &[Scenario]) -> AppResult<Report> {
    let mut report = Report::default();
    for scenario in scenarios {
        let recorder = Recorder::new();
        let world = factory.build(recorder.clone()).await?;
        let mut run = Run {
            world: &world,
            recorder: &recorder,
            sc: scenario,
            found: Vec::new(),
        };
        run.scenario().await?;
        report.scenarios += 1;
        report.violations.extend(run.found);
    }
    Ok(report)
}

/// The operations a check covers, with the label the report shows.
struct Ops {
    label: &'static str,
    ops: Vec<Operation>,
}

impl Ops {
    fn one(op: Operation) -> Self {
        Self {
            label: op_name(op),
            ops: vec![op],
        }
    }

    fn correct() -> Self {
        Self {
            label: "correct",
            ops: vec![Operation::Cancel, Operation::Create],
        }
    }
}

struct Run<'a> {
    world: &'a World,
    recorder: &'a Recorder,
    sc: &'a Scenario,
    found: Vec<Violation>,
}

impl Run<'_> {
    fn filter(&self, id: RecordId) -> StoreFilter {
        StoreFilter {
            doctype: self.sc.doctype.clone(),
            id,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn note(
        &mut self,
        code: &'static str,
        ops: &Ops,
        step: Option<StepName>,
        store: Option<&StoreName>,
        delta: Option<String>,
        what: String,
        remedy: &'static str,
    ) {
        self.found.push(Violation {
            code,
            doctype: self.sc.doctype.clone(),
            operation: ops.label,
            step,
            store: store.cloned(),
            delta,
            what,
            remedy,
        });
    }

    fn declared(&self, ops: &Ops) -> Vec<DeclaredEffect> {
        ops.ops
            .iter()
            .flat_map(|op| self.world.registry.effects(&self.sc.doctype, *op))
            .collect()
    }

    fn step_writing(&self, ops: &Ops, store: &StoreName) -> Option<StepName> {
        self.declared(ops)
            .into_iter()
            .find(|d| d.decl.kind == EffectKind::Internal && d.decl.writes.contains(store))
            .map(|d| d.step)
    }

    /// The scenario's full run.
    async fn scenario(&mut self) -> AppResult<()> {
        let inv = self.sc.invocation.clone();
        let service = Arc::clone(&self.world.service);
        let probe = Arc::clone(&self.world.probe);

        // Create, post, cancel, cancel again.
        let draft = (self.sc.draft)();
        self.recorder.clear();
        let created = match service.create(&inv, draft).await {
            Ok(out) => out.value,
            Err(e) => {
                self.failed(&Ops::one(Operation::Create), &e.code);
                return Ok(());
            }
        };
        let id = created.id();
        let filter = self.filter(id);
        let start = snapshot(&*probe, &filter).await?;
        let after_create = self
            .after(&Ops::one(Operation::Create), &created, &start, &filter)
            .await?;

        let Some(posted) = self
            .step(
                Operation::Post,
                service
                    .post(&inv, &self.sc.doctype, id, version_of(&created))
                    .await,
            )
            .await
        else {
            return Ok(());
        };
        let (after_post, post_receipt) = self
            .after(
                &Ops::one(Operation::Post),
                &posted,
                &after_create.0,
                &filter,
            )
            .await?;

        let Some(cancelled) = self
            .step(
                Operation::Cancel,
                service
                    .cancel(&inv, &self.sc.doctype, id, version_of(&posted))
                    .await,
            )
            .await
        else {
            return Ok(());
        };
        let cancel_ops = Ops::one(Operation::Cancel);
        let (after_cancel, cancel_receipt) = self
            .after(&cancel_ops, &cancelled, &after_post, &filter)
            .await?;
        self.nets(&cancel_ops, &start, &after_post, &after_cancel);
        self.links(
            &post_receipt,
            &cancel_receipt,
            &filter,
            &after_post,
            &after_cancel,
        )
        .await?;

        // A second cancel must be refused by the lifecycle and must not touch a store.
        self.recorder.clear();
        let again = service
            .cancel(&inv, &self.sc.doctype, id, version_of(&cancelled))
            .await;
        let after_again = snapshot(&*probe, &filter).await?;
        if again.is_ok() {
            self.note(
                codes::CANCEL_NOT_REFUSED,
                &cancel_ops,
                None,
                None,
                None,
                "a second cancel of an already cancelled document was accepted".to_owned(),
                REMEDY_CANCEL_TWICE,
            );
        }
        for (store, reading) in &after_cancel {
            let now = after_again.get(store);
            if now != Some(reading) {
                let delta = now.map(|n| format_delta(reading.sum, reading.rows, n.sum, n.rows));
                let step = self.step_writing(&cancel_ops, store);
                self.note(
                    codes::CANCEL_DUPLICATED,
                    &cancel_ops,
                    step,
                    Some(store),
                    delta,
                    "cancelling a cancelled document changed the store".to_owned(),
                    REMEDY_CANCEL_TWICE,
                );
            }
        }
        self.recorder.clear();

        // Correct is cancel plus a new document.
        self.correct().await
    }

    async fn correct(&mut self) -> AppResult<()> {
        let inv = self.sc.invocation.clone();
        let service = Arc::clone(&self.world.service);
        let doctype = self.sc.doctype.clone();
        let ops = Ops::correct();

        self.recorder.clear();
        let created = match service.create(&inv, (self.sc.draft)()).await {
            Ok(out) => out.value,
            Err(e) => {
                self.failed(&Ops::one(Operation::Create), &e.code);
                return Ok(());
            }
        };
        let id = created.id();
        let filter = self.filter(id);
        let probe = Arc::clone(&self.world.probe);
        let start = snapshot(&*probe, &filter).await?;
        let (created_snap, _) = self
            .after(&Ops::one(Operation::Create), &created, &start, &filter)
            .await?;
        let Some(posted) = self
            .step(
                Operation::Post,
                service.post(&inv, &doctype, id, version_of(&created)).await,
            )
            .await
        else {
            return Ok(());
        };
        let (posted_snap, _) = self
            .after(&Ops::one(Operation::Post), &posted, &created_snap, &filter)
            .await?;

        let corrected = match service
            .correct(&inv, &doctype, id, version_of(&posted))
            .await
        {
            Ok(out) => out.value,
            Err(e) => {
                self.failed(&ops, &e.code);
                return Ok(());
            }
        };
        let (corrected_snap, _) = self.after(&ops, &corrected, &posted_snap, &filter).await?;
        self.nets(&ops, &start, &posted_snap, &corrected_snap);

        // The new draft is not posted: it must own no store rows yet.
        let draft_filter = self.filter(corrected.id());
        for (store, reading) in snapshot(&*probe, &draft_filter).await? {
            if reading.rows > 0 || !reading.sum.is_zero() {
                let step = self.step_writing(&ops, &store);
                let delta = format!(
                    "sum {}, rows {}",
                    signed_decimal(reading.sum),
                    signed(i128::from(reading.rows))
                );
                self.note(
                    codes::CORRECT_BROKEN,
                    &ops,
                    step,
                    Some(&store),
                    Some(delta),
                    "the new draft returned by correct already owns store rows".to_owned(),
                    REMEDY_CORRECT,
                );
            }
        }

        let original = service.load(&inv, &doctype, id).await.map(|o| o.value);
        let cancelled = matches!(&original, Ok(d) if d.state() == Some(DocState::Cancelled));
        let linked = matches!(corrected.link(AMENDED_FROM), Ok(Some(from)) if from == id);
        let is_new = corrected.id() != id && corrected.state() == Some(DocState::Draft);
        if !(cancelled && linked && is_new) {
            let what = format!(
                "after correct the original is {}, the result is {} and {}",
                if cancelled {
                    "cancelled"
                } else {
                    "not cancelled"
                },
                if is_new {
                    "a new draft"
                } else {
                    "not a new draft"
                },
                if linked {
                    "links the original"
                } else {
                    "does not link the original through amended_from"
                },
            );
            self.note(
                codes::CORRECT_BROKEN,
                &ops,
                None,
                None,
                None,
                what,
                REMEDY_CORRECT,
            );
        }
        self.recorder.clear();
        Ok(())
    }

    /// Records a failed operation as a finding; `None` ends the scenario.
    async fn step(
        &mut self,
        op: Operation,
        result: AppResult<Outcome<Document>>,
    ) -> Option<Document> {
        match result {
            Ok(out) => Some(out.value),
            Err(e) => {
                self.failed(&Ops::one(op), &e.code);
                None
            }
        }
    }

    fn failed(&mut self, ops: &Ops, code: &str) {
        self.note(
            codes::OPERATION_FAILED,
            ops,
            None,
            None,
            None,
            format!("the operation was refused with {code}, so nothing after it could be checked"),
            "give the scenario a draft that passes validation, and make sure its steps succeed in the in-memory engine",
        );
    }

    /// Everything checked after one operation: reads the stores, builds the receipt from what
    /// the recorder saw, and checks receipt against declarations, writes against the receipt,
    /// and that no row went away. Returns the new reading and the receipt.
    async fn after(
        &mut self,
        ops: &Ops,
        doc: &Document,
        before: &Snapshot,
        filter: &StoreFilter,
    ) -> AppResult<(Snapshot, OperationReceipt)> {
        let after = snapshot(&*self.world.probe, filter).await?;
        let reported = self.recorder.take();
        let receipt = self.receipt(ops.ops[0], doc, reported);
        self.undeclared(ops, &receipt);
        self.writes(ops, &receipt, before, &after);
        for (store, was) in before {
            let now = after.get(store).map_or(0, |r| r.rows);
            if now < was.rows {
                let step = self.step_writing(ops, store);
                self.note(
                    codes::ROW_DELETED,
                    ops,
                    step,
                    Some(store),
                    Some(format!(
                        "rows {}",
                        signed(i128::from(now) - i128::from(was.rows))
                    )),
                    "the store lost rows; stores are append-only".to_owned(),
                    REMEDY_APPEND,
                );
            }
        }
        Ok((after, receipt))
    }

    fn receipt(
        &self,
        op: Operation,
        doc: &Document,
        effects: Vec<EffectReceipt>,
    ) -> OperationReceipt {
        OperationReceipt {
            operation: op,
            doctype: self.sc.doctype.clone(),
            name: doc.id().to_string(),
            version: doc.version().unwrap_or(Version::FIRST),
            effects,
            committed_at: self.sc.invocation.now,
        }
    }

    /// Effects reported that no declaration of the operation's pipeline covers. For `correct`
    /// (cancel plus create) an effect is undeclared only when neither pipeline declares it.
    fn undeclared(&mut self, ops: &Ops, receipt: &OperationReceipt) {
        let mut per_op = ops.ops.iter().map(|op| {
            let mut r = receipt.clone();
            r.operation = *op;
            check_receipt(&self.world.registry, &self.sc.doctype, *op, &r)
        });
        let Some(first) = per_op.next() else { return };
        let rest: Vec<_> = per_op.collect();
        for u in first {
            if rest.iter().all(|other| {
                other
                    .iter()
                    .any(|o| o.step == u.step && o.class == u.class && o.kind == u.kind)
            }) {
                self.note(
                    codes::UNDECLARED_REPORT,
                    ops,
                    Some(u.step.clone()),
                    None,
                    None,
                    format!("step {} reported a {:?} effect of class {} that no declaration covers ({:?})", u.step, u.kind, u.class, u.reason),
                    REMEDY_DECLARE,
                );
            }
        }
    }

    /// The rows each store gained against the rows the receipt reports. Effects that write
    /// several stores are compared as a group, since the receipt counts rows, not stores.
    fn writes(
        &mut self,
        ops: &Ops,
        receipt: &OperationReceipt,
        before: &Snapshot,
        after: &Snapshot,
    ) {
        let declared = self.declared(ops);
        let mut groups: BTreeMap<BTreeSet<StoreName>, (u64, Option<StepName>)> = BTreeMap::new();
        for e in receipt
            .effects
            .iter()
            .filter(|e| e.kind == EffectKind::Internal)
        {
            let stores: BTreeSet<StoreName> = declared
                .iter()
                .filter(|d| {
                    d.step == e.step
                        && d.decl.class == e.class
                        && d.decl.kind == EffectKind::Internal
                })
                .flat_map(|d| d.decl.writes.iter().cloned())
                .collect();
            if stores.is_empty() {
                continue; // undeclared: reported by `undeclared`
            }
            let entry = groups.entry(stores).or_insert((0, Some(e.step.clone())));
            entry.0 += u64::from(e.count);
        }
        let covered: BTreeSet<StoreName> = groups.keys().flatten().cloned().collect();
        for store in after.keys().filter(|s| !covered.contains(*s)) {
            groups.insert(BTreeSet::from([store.clone()]), (0, None));
        }
        for (stores, (reported, step)) in groups {
            let mut gained: i128 = 0;
            let mut sum = Decimal::ZERO;
            for store in &stores {
                let was = before.get(store).copied();
                let now = after.get(store).copied();
                let rows_was = was.map_or(0, |r| r.rows);
                let rows_now = now.map_or(0, |r| r.rows);
                gained += i128::from(rows_now.saturating_sub(rows_was));
                sum += now.map_or(Decimal::ZERO, |r| r.sum) - was.map_or(Decimal::ZERO, |r| r.sum);
            }
            let reported = i128::from(reported);
            if gained == reported {
                continue;
            }
            let first = stores.iter().next();
            let step = step.or_else(|| first.and_then(|s| self.step_writing(ops, s)));
            let (code, what, remedy) = if gained > reported {
                (
                    codes::UNREPORTED_WRITE,
                    format!("the store gained {gained} row(s) but the receipt reports {reported}"),
                    REMEDY_REPORT,
                )
            } else {
                (
                    codes::OVER_REPORTED,
                    format!("the receipt reports {reported} row(s) but the store gained {gained}"),
                    REMEDY_OVER_REPORT,
                )
            };
            let delta = format!("sum {}, rows {}", signed_decimal(sum), signed(gained));
            self.note(code, ops, step, first, Some(delta), what, remedy);
        }
    }

    /// After post then cancel (or correct) every store's aggregate must be back where it began.
    fn nets(&mut self, ops: &Ops, start: &Snapshot, posted: &Snapshot, end: &Snapshot) {
        for (store, begin) in start {
            let Some(last) = end.get(store) else { continue };
            let net = last.sum - begin.sum;
            if net.is_zero() {
                continue;
            }
            let wrote = posted.get(store).map_or(Decimal::ZERO, |p| p.sum) - begin.sum;
            let over = wrote.is_zero() || net.is_sign_negative() != wrote.is_sign_negative();
            let (code, what, remedy) = if over {
                (
                    codes::OVER_REVERSAL,
                    "cancel reversed more than post wrote",
                    REMEDY_OVER,
                )
            } else {
                (
                    codes::NOT_NETTED,
                    "post then cancel does not net the store to zero",
                    REMEDY_REVERSE,
                )
            };
            let step = self
                .step_writing(&Ops::one(Operation::Cancel), store)
                .or_else(|| self.step_writing(&Ops::one(Operation::Post), store));
            let delta = format!(
                "sum {}, rows {}",
                signed_decimal(net),
                signed(i128::from(last.rows) - i128::from(begin.rows))
            );
            self.note(
                code,
                ops,
                step,
                Some(store),
                Some(delta),
                what.to_owned(),
                remedy,
            );
        }
    }

    /// The reversing rows must point at the rows post wrote, and the cancel receipt must name the
    /// rows it appended. Both checks apply only when the probe records the link
    /// ([`StoreProbe::reversal_targets`] returns `Some`) and cancel appended rows: a probe that
    /// returns `None` opts out of them.
    async fn links(
        &mut self,
        post: &OperationReceipt,
        cancel: &OperationReceipt,
        filter: &StoreFilter,
        posted: &Snapshot,
        cancelled: &Snapshot,
    ) -> AppResult<()> {
        let ops = Ops::one(Operation::Cancel);
        let declared = self
            .world
            .registry
            .effects(&self.sc.doctype, Operation::Post);
        let stores = self.world.probe.stores();
        for store in stores {
            // Only a store the cancel appended to has reversing rows to check.
            let grew = cancelled.get(&store).map_or(0, |r| r.rows)
                > posted.get(&store).map_or(0, |r| r.rows);
            if !grew {
                continue;
            }
            let Some(targets) = self.world.probe.reversal_targets(&store, filter).await? else {
                continue;
            };
            let declared_cancel = self
                .world
                .registry
                .effects(&self.sc.doctype, Operation::Cancel);
            let named = cancel
                .effects
                .iter()
                .filter(|e| {
                    declared_cancel.iter().any(|d| {
                        d.step == e.step
                            && d.decl.class == e.class
                            && d.decl.writes.contains(&store)
                    })
                })
                .map(|e| e.store_refs.len())
                .sum::<usize>();
            if named == 0 {
                let step = self.step_writing(&ops, &store);
                self.note(
                    codes::REVERSAL_UNLINKED,
                    &ops,
                    step,
                    Some(&store),
                    None,
                    "the cancel appended rows but its receipt names none of them".to_owned(),
                    REMEDY_RECEIPT_REFS,
                );
            }
            let wrote: Vec<&OpaqueRef> = post
                .effects
                .iter()
                .filter(|e| {
                    declared.iter().any(|d| {
                        d.step == e.step
                            && d.decl.class == e.class
                            && d.decl.writes.contains(&store)
                    })
                })
                .flat_map(|e| e.store_refs.iter())
                .collect();
            let missing = wrote.iter().filter(|r| !targets.contains(r)).count();
            if missing > 0 {
                let step = self.step_writing(&ops, &store);
                self.note(
                    codes::REVERSAL_UNLINKED,
                    &ops,
                    step,
                    Some(&store),
                    None,
                    format!("{missing} of the {} row(s) post wrote are not referenced by the reversing rows", wrote.len()),
                    REMEDY_LINK,
                );
            }
        }
        Ok(())
    }
}

fn version_of(doc: &Document) -> Version {
    doc.version().unwrap_or(Version::FIRST)
}

fn signed(n: i128) -> String {
    if n > 0 {
        format!("+{n}")
    } else {
        n.to_string()
    }
}

fn signed_decimal(d: Decimal) -> String {
    if d.is_sign_positive() && !d.is_zero() {
        format!("+{d}")
    } else {
        d.to_string()
    }
}

fn format_delta(sum_a: Decimal, rows_a: u64, sum_b: Decimal, rows_b: u64) -> String {
    format!(
        "sum {}, rows {}",
        signed_decimal(sum_b - sum_a),
        signed(i128::from(rows_b) - i128::from(rows_a))
    )
}
