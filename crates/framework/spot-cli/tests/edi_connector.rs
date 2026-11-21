//! The `edi` connector kind (WP-INT-06.T2) over in-memory seams: the reference X12 850 round
//! trip, duplicate interchange numbers, per-set rejection without aborting the others, a
//! principal that may not create, limits, injection-looking values as plain text, control
//! numbers under concurrent sends and per tenant.
//!
//! `src/services/connectors/edi.rs` has no `crate::` path and is compiled into this test crate,
//! so the tests drive the code the connector service registers.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[path = "../src/services/connectors/edi.rs"]
mod edi;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use edi::{
    AckOutbox, AuditSink, ControlStore, Delivery, DocumentSink, EdiConfig, EdiHost, ReceiptAudit,
    ReceiveError, Scope, SetMapping, SinkRefusal, Stamp, StoreError,
};
use serde_json::{Value as Json, json};
use spot_connectors::{Coercion, MappingRule, MappingSpec};
use spot_edi::{EdiMapping, Limits, parse_str};

const PO: &str = "ISA*00*          *00*          *ZZ*SENDER         *ZZ*RECEIVER       *240101*1200*^*00401*000000001*0*P*>~GS*PO*SENDER*RECEIVER*20240101*1200*1*X*004010~ST*850*0001~BEG*00*NE*PO12345**20240101~REF*DP*DEPT-1~N1*ST*Warehouse One*92*WH1~PO1*1*10*EA*12.50**BP*WIDGET-1~CTT*1~SE*7*0001~GE*1*1~IEA*1*000000001~";

#[derive(Default)]
struct Seams {
    docs: Mutex<Vec<(String, String, Json)>>,
    inbound: Mutex<HashSet<(Scope, String)>>,
    outbound: Mutex<HashMap<Scope, u64>>,
    acks: Mutex<Vec<String>>,
    sent: Mutex<Vec<String>>,
    audit: Mutex<Vec<ReceiptAudit>>,
    deny: Mutex<HashSet<String>>,
}

impl DocumentSink for Seams {
    fn create(&self, principal: &str, doctype: &str, doc: &Json) -> Result<String, SinkRefusal> {
        if self.deny.lock().unwrap().contains(doctype) {
            return Err(SinkRefusal::PermissionDenied);
        }
        let mut docs = self.docs.lock().unwrap();
        docs.push((principal.to_owned(), doctype.to_owned(), doc.clone()));
        Ok(format!("rec-{}", docs.len()))
    }
}

impl ControlStore for Seams {
    fn claim_inbound(&self, scope: &Scope, control: &str) -> Result<bool, StoreError> {
        Ok(self
            .inbound
            .lock()
            .unwrap()
            .insert((scope.clone(), control.to_owned())))
    }
    fn next_outbound(&self, scope: &Scope) -> Result<u64, StoreError> {
        let mut map = self.outbound.lock().unwrap();
        let n = map.entry(scope.clone()).or_insert(0);
        *n += 1;
        Ok(*n)
    }
}

impl AckOutbox for Seams {
    fn queue(&self, _: &Scope, text: &str) -> Result<(), StoreError> {
        self.acks.lock().unwrap().push(text.to_owned());
        Ok(())
    }
}

impl Delivery for Seams {
    fn deliver(&self, _: &Scope, text: &str) -> Result<(), StoreError> {
        self.sent.lock().unwrap().push(text.to_owned());
        Ok(())
    }
}

impl AuditSink for Seams {
    fn record(&self, event: &ReceiptAudit) {
        self.audit.lock().unwrap().push(event.clone());
    }
}

fn rule(target: &str, source: &str) -> MappingRule {
    MappingRule {
        target: target.to_owned(),
        source: source.to_owned(),
        expression: String::new(),
        coerce: Coercion::None,
    }
}

fn config(limits: Limits) -> EdiConfig {
    let spec = MappingSpec {
        rules: vec![
            rule("po_number", "segments.BEG.s0.e2"),
            rule("ship_to", "segments.N1.s0.e1"),
            rule("note", "segments.REF.s0.e2"),
        ],
    };
    let mut mappings = BTreeMap::new();
    mappings.insert(
        "850".to_owned(),
        SetMapping {
            doctype: "purchase_order".to_owned(),
            mapping: EdiMapping::new(spec),
            schema: None,
        },
    );
    EdiConfig {
        principal: "connector:edi_in".to_owned(),
        limits,
        mappings,
    }
}

fn stamp() -> Stamp {
    Stamp {
        date: "240102".to_owned(),
        time: "0930".to_owned(),
        micros: 1_800_000_000_000_000,
    }
}

fn host<'a>(cfg: &'a EdiConfig, seams: &'a Seams) -> EdiHost<'a> {
    EdiHost {
        config: cfg,
        sink: seams,
        store: seams,
        outbox: seams,
        audit: seams,
        delivery: seams,
    }
}

fn recv(h: &EdiHost, text: &str) -> Result<edi::ReceiveReport, ReceiveError> {
    h.receive("acme", "edi_in", text, &stamp())
}

#[test]
fn an_inbound_purchase_order_creates_the_document_as_the_service_principal_and_is_acked() {
    let (cfg, seams) = (config(Limits::default()), Seams::default());
    let report = recv(&host(&cfg, &seams), PO).unwrap();
    assert_eq!(report.control, "000000001");
    let docs = seams.docs.lock().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(
        docs[0].0, "connector:edi_in",
        "created as the connector's principal"
    );
    assert_eq!(docs[0].1, "purchase_order");
    assert_eq!(docs[0].2["po_number"], "PO12345");
    assert_eq!(docs[0].2["ship_to"], "Warehouse One");
    assert!(report.results.iter().all(|r| r.accepted));
    let ack = parse_str(&seams.acks.lock().unwrap()[0], Limits::default()).unwrap();
    assert!(ack.messages().iter().any(|m| m.id() == "997"));
    let audit = seams.audit.lock().unwrap();
    assert_eq!(
        (audit[0].outcome, audit[0].accepted, audit[0].rejected),
        ("accepted", 1, 0)
    );
}

#[test]
fn a_duplicate_interchange_number_is_refused_acknowledged_and_creates_nothing_twice() {
    let (cfg, seams) = (config(Limits::default()), Seams::default());
    let h = host(&cfg, &seams);
    recv(&h, PO).unwrap();
    let again = recv(&h, PO).unwrap();
    assert!(again.results.iter().all(|r| !r.accepted));
    assert_eq!(again.results[0].errors, [edi::DUPLICATE_INTERCHANGE]);
    assert_eq!(seams.docs.lock().unwrap().len(), 1);
    assert_eq!(seams.audit.lock().unwrap()[1].outcome, "duplicate");
}

#[test]
fn a_set_the_principal_may_not_create_is_rejected_in_the_ack_and_no_document_exists() {
    let (cfg, seams) = (config(Limits::default()), Seams::default());
    seams
        .deny
        .lock()
        .unwrap()
        .insert("purchase_order".to_owned());
    let report = recv(&host(&cfg, &seams), PO).unwrap();
    assert_eq!(report.results[0].errors, [edi::NOT_PERMITTED]);
    assert!(seams.docs.lock().unwrap().is_empty());
    assert_eq!(seams.audit.lock().unwrap()[0].outcome, "rejected");
}

#[test]
fn one_failing_set_does_not_abort_the_others() {
    let (cfg, seams) = (config(Limits::default()), Seams::default());
    let two = PO
        .replace(
            "SE*7*0001~",
            "SE*7*0001~ST*856*0002~BSN*00*S1*20240101*1200~SE*3*0002~",
        )
        .replace("GE*1*1", "GE*2*1");
    let report = recv(&host(&cfg, &seams), &two).unwrap();
    assert_eq!(report.results.len(), 2);
    assert!(report.results[0].accepted);
    assert_eq!(report.results[1].errors, [edi::UNSUPPORTED_SET]);
    assert_eq!(seams.docs.lock().unwrap().len(), 1);
    assert_eq!(seams.audit.lock().unwrap()[0].outcome, "partial");
}

#[test]
fn an_oversize_or_malformed_interchange_is_refused_by_the_limits_and_audited() {
    let (cfg, seams) = (config(Limits::small()), Seams::default());
    let big = PO.replace("REF*DP*DEPT-1~", &"REF*DP*X~".repeat(1000));
    let error = recv(&host(&cfg, &seams), &big).unwrap_err();
    assert!(matches!(error, ReceiveError::Refused(_)));
    assert!(matches!(
        recv(&host(&cfg, &seams), "ISA*00*").unwrap_err(),
        ReceiveError::Refused(_)
    ));
    assert!(seams.docs.lock().unwrap().is_empty());
    let audit = seams.audit.lock().unwrap();
    assert!(
        audit
            .iter()
            .all(|e| e.outcome == "refused" && e.code.is_some())
    );
}

#[test]
fn injection_looking_values_are_stored_as_plain_text_and_stay_out_of_the_audit() {
    let (cfg, seams) = (config(Limits::default()), Seams::default());
    let evil = PO.replace("Warehouse One", "=cmd|' /C calc'!A0 ../../etc DROP TABLE x");
    recv(&host(&cfg, &seams), &evil).unwrap();
    let docs = seams.docs.lock().unwrap();
    assert_eq!(
        docs[0].2["ship_to"],
        "=cmd|' /C calc'!A0 ../../etc DROP TABLE x"
    );
    let audit = format!("{:?}", seams.audit.lock().unwrap());
    assert!(!audit.contains("calc") && !audit.contains("PO12345") && !audit.contains("Warehouse"));
}

#[test]
fn an_outbound_interchange_parses_back_and_control_numbers_never_repeat() {
    let (cfg, seams) = (config(Limits::default()), Seams::default());
    let seams = Arc::new(seams);
    let template = parse_str(PO, Limits::default()).unwrap();
    let doc = json!({"po_number": "PO777", "ship_to": "Dock 4", "note": "D9"});
    let numbers: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (seams, cfg, template, doc) = (&seams, &cfg, &template, &doc);
                scope.spawn(move || {
                    let h = host(cfg, seams);
                    h.send("acme", "edi_out", "850", template, doc).unwrap().1
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let unique: HashSet<&String> = numbers.iter().collect();
    assert_eq!(unique.len(), 8, "{numbers:?}");
    let first = seams.sent.lock().unwrap()[0].clone();
    let back = parse_str(&first, Limits::default()).unwrap();
    assert_eq!(
        back.messages()[0]
            .segments
            .iter()
            .find(|s| s.tag == "BEG")
            .unwrap()
            .text(2),
        "PO777"
    );
    assert!(spot_edi::is_consistent(&back));
}

#[test]
fn control_numbers_are_scoped_per_tenant() {
    let (cfg, seams) = (config(Limits::default()), Seams::default());
    let template = parse_str(PO, Limits::default()).unwrap();
    let doc = json!({"po_number": "P", "ship_to": "S", "note": "N"});
    let h = host(&cfg, &seams);
    let a1 = h.send("acme", "edi_out", "850", &template, &doc).unwrap().1;
    let b1 = h.send("beta", "edi_out", "850", &template, &doc).unwrap().1;
    let a2 = h.send("acme", "edi_out", "850", &template, &doc).unwrap().1;
    assert_eq!(
        (a1.as_str(), b1.as_str(), a2.as_str()),
        ("000000001", "000000001", "000000002")
    );
}
