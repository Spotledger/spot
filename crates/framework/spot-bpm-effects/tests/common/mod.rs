//! A tiny fake app: documents post signed amounts into one append-only ledger and reverse them
//! on cancel. A document with a live dependant refuses its cancel, like a real record type.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use spot_app_api::prelude::{Decimal, OffsetDateTime, RecordId, Version};
use spot_app_api::{
    ActionRef, AppResult, BoxFuture, EffectClassName, EffectDecl, EffectKind, EffectReceipt, Hex32,
    HookPoint, OpaqueRef, Operation, OperationReceipt, Phase, ReceiptState, Reversal, StepName,
    StoreName, Target,
};
use spot_bpm_effects::{
    Alternative, AlternativeId, AlternativeKind, CancellationCall, NewEntry, PartnerPort,
    PortError, RecordRef, Refusal, RequestEffects, ReversalCall, ReversalPort, StaticDeclarations,
};
use spot_bpm_engine::CaseId;
use spot_dag::NodeId;
use spot_effect_kit::{StoreFilter, StoreProbe};
use spot_seams::DeclaredEffect;

pub const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;
pub const LEDGER: &str = "ledger";

pub fn case(name: &str) -> CaseId {
    CaseId::new(name).unwrap()
}

pub fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

/// A deterministic record id for a number.
pub fn rid(n: u64) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[8..].copy_from_slice(&n.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RecordId::from_bytes(bytes).unwrap()
}

pub fn rec(n: u64) -> RecordRef {
    RecordRef::new("doc", rid(n).to_string())
}

pub fn run<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

fn effect(
    step: &str,
    class: &str,
    kind: EffectKind,
    refs: &[&str],
    state: ReceiptState,
) -> EffectReceipt {
    EffectReceipt {
        step: StepName::new(step).unwrap(),
        class: EffectClassName::new(class).unwrap(),
        kind,
        store_refs: refs.iter().map(|r| OpaqueRef::new(r).unwrap()).collect(),
        count: u32::try_from(refs.len()).unwrap(),
        digest: Hex32::new(&"0".repeat(64)).unwrap(),
        state,
    }
}

pub fn receipt(
    operation: Operation,
    record: &RecordRef,
    version: u64,
    effects: Vec<EffectReceipt>,
) -> OperationReceipt {
    OperationReceipt {
        operation,
        doctype: record.doctype.clone(),
        name: record.name.clone(),
        version: Version(version),
        effects,
        committed_at: NOW,
    }
}

pub fn post_receipt(record: &RecordRef) -> OperationReceipt {
    receipt(
        Operation::Post,
        record,
        2,
        vec![effect(
            "fake:post_ledger",
            "balance_movement",
            EffectKind::Internal,
            &["row"],
            ReceiptState::Applied,
        )],
    )
}

pub fn declarations() -> StaticDeclarations {
    let point = HookPoint::of(Phase::After, Target::Op(Operation::Post));
    let ledger = DeclaredEffect {
        step: StepName::new("fake:post_ledger").unwrap(),
        app: "fake".to_owned(),
        point: point.clone(),
        decl: EffectDecl::new(
            EffectKind::Internal,
            EffectClassName::new("balance_movement").unwrap(),
            vec![StoreName::new(LEDGER).unwrap()],
            Reversal::ReversibleBy(Operation::Cancel),
        ),
    };
    let mail = DeclaredEffect {
        step: StepName::new("fake:notify").unwrap(),
        app: "fake".to_owned(),
        point: point.clone(),
        decl: EffectDecl::new(
            EffectKind::External,
            EffectClassName::new("mail").unwrap(),
            vec![StoreName::new("outbox_mail").unwrap()],
            Reversal::CompensableBy(ActionRef::new("fake:send_correction").unwrap()),
        ),
    };
    let wire = DeclaredEffect {
        step: StepName::new("fake:wire").unwrap(),
        app: "fake".to_owned(),
        point,
        decl: EffectDecl::new(
            EffectKind::External,
            EffectClassName::new("bank_wire").unwrap(),
            vec![StoreName::new("outbox_wire").unwrap()],
            Reversal::Irreversible {
                reason: "money left".to_owned(),
            },
        ),
    };
    StaticDeclarations::new()
        .with("doc", Operation::Post, ledger)
        .with("doc", Operation::Post, mail)
        .with("doc", Operation::Post, wire)
}

pub fn service() -> RequestEffects {
    RequestEffects::new(Arc::new(declarations()))
}

pub fn new_entry(n: &str, record: &RecordRef, links: &[&RecordRef]) -> NewEntry {
    NewEntry {
        node: node(n),
        receipt: post_receipt(record),
        links: links.iter().map(|r| (*r).clone()).collect(),
        at: NOW,
    }
}

#[derive(Default)]
pub struct Doc {
    pub amount: i64,
    pub cancelled: bool,
    pub version: u64,
    /// The record this one depends on, when it does.
    pub depends_on: Option<RecordRef>,
}

#[derive(Default)]
pub struct State {
    pub docs: BTreeMap<RecordRef, Doc>,
    /// `(document name, signed amount)`, append-only.
    pub rows: Vec<(RecordRef, i64)>,
    pub done: BTreeMap<String, OperationReceipt>,
    /// Document names in the order their cancel succeeded.
    pub reversed: Vec<RecordRef>,
    pub refuse: BTreeSet<RecordRef>,
    pub crash_after: Option<usize>,
    pub calls: usize,
    pub log: Vec<String>,
}

pub struct FakeApp {
    pub state: Mutex<State>,
    /// A hook run at the start of every reversal call, for the concurrency test.
    pub on_call: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl FakeApp {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            on_call: Mutex::new(None),
        })
    }

    /// Posts a document: appends its row.
    pub fn post(&self, record: &RecordRef, amount: i64, depends_on: Option<&RecordRef>) {
        let mut s = self.state.lock().unwrap();
        s.rows.push((record.clone(), amount));
        s.docs.insert(
            record.clone(),
            Doc {
                amount,
                cancelled: false,
                version: 2,
                depends_on: depends_on.cloned(),
            },
        );
    }

    pub fn net(&self, record: &RecordRef) -> i64 {
        self.state
            .lock()
            .unwrap()
            .rows
            .iter()
            .filter(|(r, _)| r == record)
            .map(|(_, a)| *a)
            .sum()
    }

    pub fn row_count(&self) -> usize {
        self.state.lock().unwrap().rows.len()
    }

    pub fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    fn live_dependants(s: &State, record: &RecordRef) -> Vec<RecordRef> {
        s.docs
            .iter()
            .filter(|(_, d)| !d.cancelled && d.depends_on.as_ref() == Some(record))
            .map(|(r, _)| r.clone())
            .collect()
    }
}

impl ReversalPort for FakeApp {
    fn reverse(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        if let Some(hook) = self.on_call.lock().unwrap().as_ref() {
            hook();
        }
        let mut s = self.state.lock().unwrap();
        if let Some(done) = s.done.get(&call.key) {
            return Ok(done.clone());
        }
        if s.crash_after == Some(s.calls) {
            s.crash_after = None;
            return Err(PortError::Unavailable("crash".to_owned()));
        }
        let dependants = Self::live_dependants(&s, call.record);
        if s.refuse.contains(call.record) {
            return Err(PortError::Refused(Refusal {
                code: "period_closed".to_owned(),
                dependants: Vec::new(),
                alternatives: Vec::new(),
            }));
        }
        if !dependants.is_empty() {
            return Err(PortError::Refused(Refusal {
                code: "document_has_dependants".to_owned(),
                dependants,
                alternatives: vec![Alternative {
                    id: AlternativeId::new("reverse_dated_today").unwrap(),
                    kind: AlternativeKind::Action(
                        ActionRef::new("fake:issue_reversing_record").unwrap(),
                    ),
                }],
            }));
        }
        let Some(doc) = s.docs.get_mut(call.record) else {
            return Err(PortError::Unavailable("missing".to_owned()));
        };
        doc.cancelled = true;
        doc.version += 1;
        let (amount, version) = (doc.amount, doc.version);
        s.rows.push((call.record.clone(), -amount));
        s.calls += 1;
        s.reversed.push(call.record.clone());
        s.log.push(format!("reverse {}", call.record.name));
        let r = receipt(
            Operation::Cancel,
            call.record,
            version,
            vec![effect(
                "fake:cancel_ledger",
                "balance_movement",
                EffectKind::Internal,
                &["rev"],
                ReceiptState::Applied,
            )],
        );
        s.done.insert(call.key.clone(), r.clone());
        Ok(r)
    }

    fn compensate(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        let mut s = self.state.lock().unwrap();
        if let Some(done) = s.done.get(&call.key) {
            return Ok(done.clone());
        }
        s.calls += 1;
        let action = call.action.map(ToString::to_string).unwrap_or_default();
        s.log
            .push(format!("compensate {action} {}", call.record.name));
        // "issue a reversing record dated today": a new reversing row nets the original to zero
        // and the original stays posted.
        if action == "fake:issue_reversing_record" {
            let amount = s.docs.get(call.record).map_or(0, |d| d.amount);
            s.rows.push((call.record.clone(), -amount));
            if let Some(d) = s.docs.get_mut(call.record) {
                d.cancelled = true;
            }
            s.reversed.push(call.record.clone());
        }
        let r = receipt(Operation::Create, call.record, 1, Vec::new());
        s.done.insert(call.key.clone(), r.clone());
        Ok(r)
    }
}

impl StoreProbe for FakeApp {
    fn stores(&self) -> Vec<StoreName> {
        vec![StoreName::new(LEDGER).unwrap()]
    }

    fn sum<'a>(
        &'a self,
        _store: &'a StoreName,
        filter: &'a StoreFilter,
    ) -> BoxFuture<'a, AppResult<Decimal>> {
        let name = filter.id.to_string();
        let total: i64 = self
            .state
            .lock()
            .unwrap()
            .rows
            .iter()
            .filter(|(r, _)| r.name == name)
            .map(|(_, a)| *a)
            .sum();
        Box::pin(async move { Ok(Decimal::from(total)) })
    }

    fn count<'a>(
        &'a self,
        _store: &'a StoreName,
        filter: &'a StoreFilter,
    ) -> BoxFuture<'a, AppResult<u64>> {
        let name = filter.id.to_string();
        let n = self
            .state
            .lock()
            .unwrap()
            .rows
            .iter()
            .filter(|(r, _)| r.name == name)
            .count();
        Box::pin(async move { Ok(u64::try_from(n).unwrap()) })
    }
}

/// Reads every document's sum and row count through the kit's probe.
pub fn probe(app: &FakeApp, ids: &[u64]) -> Vec<(Decimal, u64)> {
    let store = StoreName::new(LEDGER).unwrap();
    ids.iter()
        .map(|n| {
            let filter = StoreFilter {
                doctype: "doc".to_owned(),
                id: rid(*n),
            };
            (
                run(app.sum(&store, &filter)).unwrap(),
                run(app.count(&store, &filter)).unwrap(),
            )
        })
        .collect()
}

pub struct Partner {
    pub app: Arc<FakeApp>,
    pub fail: Mutex<bool>,
}

impl Partner {
    pub fn new(app: &Arc<FakeApp>) -> Self {
        Self {
            app: Arc::clone(app),
            fail: Mutex::new(false),
        }
    }
}

impl PartnerPort for Partner {
    fn send_cancellation(&self, call: &CancellationCall<'_>) -> Result<(), String> {
        if *self.fail.lock().unwrap() {
            return Err("partner_down".to_owned());
        }
        self.app
            .state
            .lock()
            .unwrap()
            .log
            .push(format!("cancel {}", call.message));
        Ok(())
    }
}
