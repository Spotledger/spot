//! A tiny two-DocType fake app: invoices and payments post signed amounts into one append-only
//! balance store and reverse them on cancel. [`Variant::Good`] passes the kit; the other
//! variants each break one rule.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation, Outcome};
use spot_app_api::prelude::{Decimal, Name, OffsetDateTime, RecordId, SiteId, Value, Version};
use spot_app_api::{
    AppResult, BoxFuture, Controller, Document, EffectClassName, EffectDecl, EffectKind,
    EffectReceipt, Hex32, HookPoint, OpaqueRef, Operation, Phase, ReceiptState, Reversal, StepDecl,
    StepEffects, StepName, StoreName, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{Engine, V7Ids};
use spot_effect_kit::{Recorder, Scenario, StoreFilter, StoreProbe, World};
use spot_meta::{Definition, FieldSpec, FieldType, Registry as MetaRegistry};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;

pub const STORE: &str = "balance_entries";
pub const CLASS: &str = "balance_movement";

/// How the app behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Posts one row and reverses it exactly, reporting both.
    Good,
    /// The cancel step is declared but writes nothing.
    MissingReversal,
    /// The post step writes a row and does not report it.
    UndeclaredWrite,
    /// The cancel step reverses twice what was posted.
    OverReversal,
    /// The cancel step deletes the posted row instead of appending a reversal.
    DeletesRow,
    /// The reversing row does not reference the row it reverses.
    Unlinked,
    /// The post step reports a class it did not declare.
    WrongClass,
    /// The cancel step appends a linked reversal but its receipt names no rows.
    CancelWithoutRefs,
}

/// How the document service misbehaves, for the checks only the lifecycle can fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tamper {
    /// The engine as it is.
    None,
    /// A second cancel is accepted and writes another reversal.
    AcceptSecondCancel,
    /// Correct only cancels: no new draft comes back.
    CorrectIsJustCancel,
    /// Correct hands back a draft that already wrote a store row.
    CorrectDraftWritesRows,
}

/// One ledger row. Rows are only ever appended (apart from the `DeletesRow` variant's cheat).
#[derive(Clone, Debug)]
struct Row {
    id: String,
    doctype: String,
    source: RecordId,
    amount: Decimal,
    reverses: Option<String>,
}

/// The append-only store.
#[derive(Default)]
pub struct Ledger {
    rows: Mutex<Vec<Row>>,
}

impl Ledger {
    fn append(
        &self,
        doctype: &str,
        source: RecordId,
        amount: Decimal,
        reverses: Option<String>,
    ) -> String {
        let mut rows = self.rows.lock().unwrap();
        let id = format!("row-{}", rows.len() + 1);
        rows.push(Row {
            id: id.clone(),
            doctype: doctype.to_owned(),
            source,
            amount,
            reverses,
        });
        id
    }

    fn rows_of(&self, doctype: &str, source: RecordId) -> Vec<Row> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.doctype == doctype && r.source == source)
            .cloned()
            .collect()
    }

    /// The `DeletesRow` cheat.
    fn remove_of(&self, doctype: &str, source: RecordId) {
        self.rows
            .lock()
            .unwrap()
            .retain(|r| !(r.doctype == doctype && r.source == source));
    }
}

impl StoreProbe for Ledger {
    fn stores(&self) -> Vec<StoreName> {
        vec![StoreName::new(STORE).unwrap()]
    }

    fn sum<'a>(
        &'a self,
        _s: &'a StoreName,
        f: &'a StoreFilter,
    ) -> BoxFuture<'a, AppResult<Decimal>> {
        let total = self
            .rows_of(&f.doctype, f.id)
            .iter()
            .map(|r| r.amount)
            .sum();
        Box::pin(async move { Ok(total) })
    }

    fn count<'a>(&'a self, _s: &'a StoreName, f: &'a StoreFilter) -> BoxFuture<'a, AppResult<u64>> {
        let n = self.rows_of(&f.doctype, f.id).len() as u64;
        Box::pin(async move { Ok(n) })
    }

    fn reversal_targets<'a>(
        &'a self,
        _s: &'a StoreName,
        f: &'a StoreFilter,
    ) -> BoxFuture<'a, AppResult<Option<Vec<OpaqueRef>>>> {
        let refs = self
            .rows_of(&f.doctype, f.id)
            .into_iter()
            .filter_map(|r| r.reverses)
            .map(|id| OpaqueRef::new(&id).unwrap())
            .collect();
        Box::pin(async move { Ok(Some(refs)) })
    }
}

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn after(op: Operation) -> HookPoint {
    HookPoint::of(Phase::After, Target::Op(op))
}

/// The controller of one DocType.
pub struct Books {
    doctype: &'static str,
    app: &'static str,
    variant: Variant,
    ledger: Arc<Ledger>,
}

impl Books {
    fn step(&self, n: &str) -> StepName {
        StepName::new(&format!("{}:{n}", self.app)).unwrap()
    }

    fn effect(&self, reversal: Reversal) -> EffectDecl {
        EffectDecl::new(
            EffectKind::Internal,
            EffectClassName::new(CLASS).unwrap(),
            vec![StoreName::new(STORE).unwrap()],
            reversal,
        )
    }

    fn report(&self, ctx: &mut dyn HookContext, step: &StepName, row: Option<&str>) {
        let class = if self.variant == Variant::WrongClass && step == &self.step("book_post") {
            "mail"
        } else {
            CLASS
        };
        ctx.report_effect(EffectReceipt {
            step: step.clone(),
            class: EffectClassName::new(class).unwrap(),
            kind: EffectKind::Internal,
            store_refs: row
                .filter(|_| {
                    !(self.variant == Variant::CancelWithoutRefs
                        && step == &self.step("book_cancel"))
                })
                .map(|r| OpaqueRef::new(r).unwrap())
                .into_iter()
                .collect(),
            count: u32::from(row.is_some()),
            digest: Hex32::new(&"0".repeat(64)).unwrap(),
            state: ReceiptState::Applied,
        });
    }
}

impl Controller for Books {
    fn doctype(&self) -> &str {
        self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        [
            ("book_post", Operation::Post),
            ("book_cancel", Operation::Cancel),
        ]
        .into_iter()
        .map(|(n, op)| StepDecl {
            name: self.step(n),
            point: after(op),
            priority: 100,
            budget: Duration::from_secs(5),
        })
        .collect()
    }

    fn step_effects(&self) -> Vec<StepEffects> {
        vec![
            StepEffects::new(
                self.step("book_post"),
                vec![self.effect(Reversal::ReversibleBy(Operation::Cancel))],
            ),
            StepEffects::new(
                self.step("book_cancel"),
                vec![self.effect(Reversal::ReversibleBy(Operation::Post))],
            ),
        ]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let id = ctx.doc().id();
            let amount = ctx.doc().decimal("amount")?.unwrap_or(Decimal::ZERO);
            let posting = step == &self.step("book_post");
            if posting {
                let row = self.ledger.append(self.doctype, id, amount, None);
                if self.variant != Variant::UndeclaredWrite {
                    self.report(ctx, step, Some(&row));
                }
                return Ok(());
            }
            let posted = self.ledger.rows_of(self.doctype, id);
            let first = posted.first().map(|r| r.id.clone());
            match self.variant {
                Variant::MissingReversal => {}
                Variant::DeletesRow => {
                    self.ledger.remove_of(self.doctype, id);
                    self.report(ctx, step, None);
                }
                Variant::OverReversal => {
                    let row = self
                        .ledger
                        .append(self.doctype, id, -(amount + amount), first);
                    self.report(ctx, step, Some(&row));
                }
                Variant::Unlinked => {
                    let row = self.ledger.append(self.doctype, id, -amount, None);
                    self.report(ctx, step, Some(&row));
                }
                Variant::Good
                | Variant::UndeclaredWrite
                | Variant::WrongClass
                | Variant::CancelWithoutRefs => {
                    let row = self.ledger.append(self.doctype, id, -amount, first);
                    self.report(ctx, step, Some(&row));
                }
            }
            Ok(())
        })
    }
}

fn definitions() -> Vec<Definition> {
    let money = |n: &str| {
        let mut f = FieldSpec::new(n, FieldType::Decimal);
        f.scale = Some(2);
        f.precision = Some(12);
        f.required = true;
        f
    };
    let mut invoice = Definition::new("ledger_invoice");
    invoice.is_posted = true;
    let mut number = FieldSpec::new("number", FieldType::Text);
    number.required = true;
    number.max_chars = Some(16);
    invoice.fields = vec![number, money("amount")];
    let mut payment = Definition::new("ledger_payment");
    payment.is_posted = true;
    payment.fields = vec![money("amount")];
    vec![invoice, payment]
}

pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn invocation() -> Invocation {
    Invocation {
        site: SiteId::new("kit.test").unwrap(),
        actor: Actor::System {
            component: name("effect_kit_tests"),
        },
        command_id: None,
        language: None,
        now: now(),
    }
}

/// A factory for the kit: a fresh engine, ledger and registry of the given variant.
pub fn factory(
    variant: Variant,
) -> impl Fn(Recorder) -> BoxFuture<'static, AppResult<World>> + Send + Sync {
    factory_with(variant, Tamper::None)
}

/// [`factory`] with a service that misbehaves as `tamper` says.
pub fn factory_with(
    variant: Variant,
    tamper: Tamper,
) -> impl Fn(Recorder) -> BoxFuture<'static, AppResult<World>> + Send + Sync {
    move |recorder: Recorder| {
        Box::pin(async move {
            let ledger = Arc::new(Ledger::default());
            let controllers: Vec<(&str, Arc<dyn Controller>)> = [
                ("invoicing", "ledger_invoice"),
                ("payments", "ledger_payment"),
            ]
            .into_iter()
            .map(|(app, doctype)| {
                let books: Arc<dyn Controller> = Arc::new(Books {
                    doctype,
                    app,
                    variant,
                    ledger: Arc::clone(&ledger),
                });
                (app, recorder.wrap(books))
            })
            .collect();
            let mut builder = RegistryBuilder::new();
            for (app, c) in &controllers {
                builder.add_controller(app, Arc::clone(c));
            }
            let registry = Arc::new(builder.build().map_err(|c| {
                spot_app_api::AppError::new(
                    spot_app_api::ErrorClass::Internal,
                    "sample.registry",
                    format!("{c:?}"),
                )
            })?);
            let meta = MetaRegistry::new(definitions())
                .map_err(|e| {
                    spot_app_api::AppError::new(
                        spot_app_api::ErrorClass::Internal,
                        "sample.meta",
                        format!("{e:?}"),
                    )
                })?
                .effective();
            let engine = Engine::new(
                FakeBackend::new(),
                meta,
                Arc::clone(&registry),
                Arc::new(ManualClock::new(now())),
                Arc::new(V7Ids),
            )?
            .with_controllers(controllers.into_iter().map(|(_, c)| c).collect());
            engine.prepare().await?;
            let service: Arc<dyn DocumentService> = match tamper {
                Tamper::None => Arc::new(engine),
                t => Arc::new(Tampered {
                    inner: engine,
                    mode: t,
                    cancels: Mutex::new(0),
                    ledger: Arc::clone(&ledger),
                }),
            };
            Ok(World {
                service,
                registry,
                probe: ledger,
            })
        }) as BoxFuture<'static, AppResult<World>>
    }
}

fn draft(doctype: &'static str) -> impl Fn() -> Document + Send + Sync {
    move || {
        let mut doc = Document::new(name(doctype), RecordId::new_v7());
        if doctype == "ledger_invoice" {
            doc.set(name("number"), "INV-1");
        }
        doc.set(name("amount"), Value::Decimal(Decimal::new(12_500, 2)));
        doc
    }
}

/// Both DocTypes.
pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario::new("ledger_invoice", invocation(), draft("ledger_invoice")),
        Scenario::new("ledger_payment", invocation(), draft("ledger_payment")),
    ]
}

type Engined = Engine<FakeBackend>;

/// The engine with one rule broken.
struct Tampered {
    inner: Engined,
    mode: Tamper,
    cancels: Mutex<u32>,
    ledger: Arc<Ledger>,
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {$(
        fn $name<'a>(&'a self, $($arg: $ty),*) -> BoxFuture<'a, AppResult<$ret>> {
            self.inner.$name($($arg),*)
        }
    )*};
}

impl DocumentService for Tampered {
    forward! {
        meta(doctype: &'a str) -> Arc<spot_app_api::DocTypeDef>;
        load(inv: &'a Invocation, doctype: &'a str, id: RecordId) -> Outcome<Document>;
        create(inv: &'a Invocation, doc: Document) -> Outcome<Document>;
        update(inv: &'a Invocation, doctype: &'a str, id: RecordId, patch: spot_app_api::DocumentPatch, expected: Version) -> Outcome<Document>;
        post(inv: &'a Invocation, doctype: &'a str, id: RecordId, expected: Version) -> Outcome<Document>;
        delete(inv: &'a Invocation, doctype: &'a str, id: RecordId, expected: Version) -> Outcome<()>;
        rename(inv: &'a Invocation, doctype: &'a str, id: RecordId, new_name: &'a str, expected: Version) -> Outcome<Document>;
        call_method(inv: &'a Invocation, doctype: &'a str, id: RecordId, method: &'a str, args: serde_json::Value, expected: Option<Version>) -> Outcome<(Document, serde_json::Value)>;
        field_changed(inv: &'a Invocation, draft: Document, field: &'a str) -> Outcome<Document>;
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let nth = {
                let mut n = self.cancels.lock().unwrap();
                *n += 1;
                *n
            };
            if self.mode == Tamper::AcceptSecondCancel && nth == 2 {
                // Accepted without the lifecycle's state check: books another reversal.
                let doc = self.inner.load(inv, doctype, id).await?.value;
                self.ledger
                    .append(doctype, id, Decimal::new(-12_500, 2), None);
                return Ok(Outcome {
                    value: doc,
                    messages: Vec::new(),
                });
            }
            self.inner.cancel(inv, doctype, id, expected).await
        })
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        if self.mode == Tamper::CorrectIsJustCancel {
            return self.inner.cancel(inv, doctype, id, expected);
        }
        if self.mode == Tamper::CorrectDraftWritesRows {
            return Box::pin(async move {
                let out = self.inner.correct(inv, doctype, id, expected).await?;
                self.ledger
                    .append(doctype, out.value.id(), Decimal::new(100, 2), None);
                Ok(out)
            });
        }
        self.inner.correct(inv, doctype, id, expected)
    }
}
