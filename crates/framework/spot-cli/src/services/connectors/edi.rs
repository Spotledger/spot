//! The built-in connector kind `edi` (WP-INT-06.T2): an X12 or EDIFACT interchange becomes
//! mapped documents through the lifecycle **as the connector's service principal**, an
//! acknowledgement (997 or CONTRL) is produced and queued, and a document is rendered back to an
//! interchange with a durable control number.
//!
//! The module is written over small seams so it is testable without a host, and so it has no
//! `crate::` path: [`DocumentSink`] creates a document as a named principal (the host adapter
//! calls the lifecycle with that principal's rights, never the administrator's),
//! [`ControlStore`] holds the durable, tenant-scoped control numbers, [`AckOutbox`] queues the
//! acknowledgement on the connector's outbound path, [`Delivery`] sends an interchange through
//! the connector's egress and [`ReceiptAudit`] records one event per interchange.
//!
//! # Receive
//!
//! 1. The input is bounded by [`spot_edi::Limits`] before anything else; a refusal is audited and
//!    acknowledged by nothing (nothing parsed, so there is nothing to acknowledge).
//! 2. The interchange control number is claimed in the [`ControlStore`] under the tenant, the
//!    connector and the sender. A repeat is refused and acknowledged as such (every set
//!    rejected with `duplicate_interchange`); no document is created twice.
//! 3. Each transaction set is handled alone: no mapping, a schema violation, a mapping failure,
//!    a refusal by the principal's rights or a failure of the lifecycle rejects that set in the
//!    acknowledgement and never aborts the others. A set the principal may not create is
//!    rejected, never escalated.
//! 4. The acknowledgement is built, written and queued. The audit event carries control
//!    numbers, counts and the outcome, never a segment value.
//!
//! Element values are data: they reach the mapped document as plain text and are never
//! interpreted as a path, a query or a formula by this module.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_edi::Limits;
use spot_edi::ack::{AckRequest, TransactionResult, acknowledge};
use spot_edi::delimiters::Syntax;
use spot_edi::envelope;
use spot_edi::mapping::EdiMapping;
use spot_edi::model::{Interchange, Message, Segment};
use spot_edi::parser::parse_str;
use spot_edi::schema::{Schema, validate};
use spot_edi::writer::write;

/// The connector kind.
pub const KIND: &str = "edi";

/// The error code of a set that no mapping covers.
pub const UNSUPPORTED_SET: &str = "unsupported_set";
/// The error code of a repeated interchange control number.
pub const DUPLICATE_INTERCHANGE: &str = "duplicate_interchange";
/// The error code of a set the principal may not create.
pub const NOT_PERMITTED: &str = "not_permitted";
/// The error code of a set whose mapping failed.
pub const MAPPING_FAILED: &str = "mapping_failed";
/// The error code of a set the lifecycle rejected as invalid.
pub const DOCUMENT_INVALID: &str = "document_invalid";
/// The error code of a set the lifecycle could not take now.
pub const UNAVAILABLE: &str = "unavailable";

/// The tenant, the connector and the trading partner a control number belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Scope {
    /// The site (tenant).
    pub site: String,
    /// The connector's name.
    pub connector: String,
    /// The partner's interchange id (inbound: the sender; outbound: the receiver).
    pub partner: String,
}

/// Why the lifecycle did not create a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkRefusal {
    /// The principal lacks the right to create this DocType.
    PermissionDenied,
    /// The document does not satisfy the DocType.
    Invalid,
    /// The lifecycle could not take it now.
    Unavailable,
}

/// Creates documents through the lifecycle as a named principal.
pub trait DocumentSink: Send + Sync {
    /// Creates `doc` of `doctype` as `principal` (the connector's service principal); returns the
    /// new record's id. The adapter must run the lifecycle with **that principal's** rights.
    fn create(&self, principal: &str, doctype: &str, doc: &Json) -> Result<String, SinkRefusal>;
}

/// A control store failure; the text is a code, never a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreError;

/// Durable, tenant-scoped control numbers.
pub trait ControlStore: Send + Sync {
    /// Records an inbound interchange control number; `Ok(true)` for a new one, `Ok(false)` for
    /// a repeat. Atomic: two concurrent claims of one number get one `true`.
    fn claim_inbound(&self, scope: &Scope, control: &str) -> Result<bool, StoreError>;
    /// The next outbound control number for `scope`, durable and never repeated, also under
    /// concurrent callers (the adapter takes a lock or uses a sequence).
    fn next_outbound(&self, scope: &Scope) -> Result<u64, StoreError>;
}

/// Queues an interchange on the connector's outbound path.
pub trait AckOutbox: Send + Sync {
    /// Queues `text` for `scope`.
    fn queue(&self, scope: &Scope, text: &str) -> Result<(), StoreError>;
}

/// Sends an interchange through the connector's egress.
pub trait Delivery: Send + Sync {
    /// Delivers `text`; the egress policy (allow-list, no private or metadata address) is the
    /// implementation's frame.
    fn deliver(&self, scope: &Scope, text: &str) -> Result<(), StoreError>;
}

/// What the audit records about one received interchange: control numbers, counts and the
/// outcome. There is no field for a segment or a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptAudit {
    /// Who.
    pub scope: Scope,
    /// The interchange control number (empty when nothing parsed).
    pub control: String,
    /// The outcome: `accepted`, `partial`, `rejected`, `duplicate` or `refused`.
    pub outcome: &'static str,
    /// Sets accepted.
    pub accepted: usize,
    /// Sets rejected.
    pub rejected: usize,
    /// The stable code of a refusal.
    pub code: Option<String>,
}

/// Records one event per interchange.
pub trait AuditSink: Send + Sync {
    /// Records `event`.
    fn record(&self, event: &ReceiptAudit);
}

/// How one transaction set becomes a document.
#[derive(Clone, Debug)]
pub struct SetMapping {
    /// The DocType the set creates.
    pub doctype: String,
    /// The shared mapping spec over the message view.
    pub mapping: EdiMapping,
    /// An optional schema the set is validated against first.
    pub schema: Option<Schema>,
}

/// The configuration of one `edi` connector.
#[derive(Clone, Debug)]
pub struct EdiConfig {
    /// The service principal documents are created as (an ordinary user with narrow roles).
    pub principal: String,
    /// The limits every parse runs under.
    pub limits: Limits,
    /// The mapping per set id (`850`, `ORDERS`).
    pub mappings: BTreeMap<String, SetMapping>,
}

/// What a receive did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiveReport {
    /// The interchange control number.
    pub control: String,
    /// The documents created: (set control number, doctype, record id).
    pub created: Vec<(String, String, String)>,
    /// The per-set results, as acknowledged.
    pub results: Vec<TransactionResult>,
    /// The acknowledgement text that was queued.
    pub acknowledgement: String,
}

/// Why a receive produced nothing to acknowledge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiveError {
    /// The input was refused (limits, syntax); the code is the `edi.*` code.
    Refused(String),
    /// A seam failed (control store, outbox).
    Unavailable,
}

/// The `edi` connector kind bound to its seams.
pub struct EdiHost<'a> {
    /// The configuration.
    pub config: &'a EdiConfig,
    /// The lifecycle.
    pub sink: &'a dyn DocumentSink,
    /// The control numbers.
    pub store: &'a dyn ControlStore,
    /// The acknowledgement outbox.
    pub outbox: &'a dyn AckOutbox,
    /// The audit.
    pub audit: &'a dyn AuditSink,
    /// The egress for sent interchanges.
    pub delivery: &'a dyn Delivery,
}

/// The date and time an acknowledgement carries; passed in, never read from a clock here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// `YYMMDD` for X12, `CCYYMMDD` for EDIFACT.
    pub date: String,
    /// `HHMM`.
    pub time: String,
    /// Microseconds since the Unix epoch, for the mapping's time functions.
    pub micros: i64,
}

impl EdiHost<'_> {
    fn audit(
        &self,
        scope: &Scope,
        control: &str,
        outcome: &'static str,
        a: usize,
        r: usize,
        code: Option<String>,
    ) {
        self.audit.record(&ReceiptAudit {
            scope: scope.clone(),
            control: control.to_owned(),
            outcome,
            accepted: a,
            rejected: r,
            code,
        });
    }

    /// Receives one interchange (`text`) for the connector `site`/`connector`.
    ///
    /// # Errors
    /// [`ReceiveError::Refused`] when the input cannot be parsed within the limits;
    /// [`ReceiveError::Unavailable`] when the control store or outbox fails.
    pub fn receive(
        &self,
        site: &str,
        connector: &str,
        text: &str,
        stamp: &Stamp,
    ) -> Result<ReceiveReport, ReceiveError> {
        let blank = Scope {
            site: site.to_owned(),
            connector: connector.to_owned(),
            partner: String::new(),
        };
        let interchange = match parse_str(text, self.config.limits) {
            Ok(i) => i,
            Err(error) => {
                let code = error.code().to_owned();
                self.audit(&blank, "", "refused", 0, 0, Some(code.clone()));
                return Err(ReceiveError::Refused(code));
            }
        };
        let scope = Scope {
            partner: interchange.sender.id.clone(),
            ..blank
        };
        let messages = interchange.messages();
        let first_claim = self
            .store
            .claim_inbound(&scope, &interchange.control)
            .map_err(|_| ReceiveError::Unavailable)?;
        let envelope_codes: Vec<String> = envelope::check(&interchange)
            .iter()
            .map(|v| v.code.to_owned())
            .collect();
        let mut created = Vec::new();
        let mut results = Vec::new();
        for message in &messages {
            let errors = if !first_claim {
                vec![DUPLICATE_INTERCHANGE.to_owned()]
            } else if !envelope_codes.is_empty() {
                envelope_codes.clone()
            } else {
                match self.handle_set(message, stamp) {
                    Ok(done) => {
                        created.push((message.control().to_owned(), done.0, done.1));
                        Vec::new()
                    }
                    Err(code) => vec![code],
                }
            };
            results.push(if errors.is_empty() {
                TransactionResult::accepted(message.id(), message.control())
            } else {
                TransactionResult::rejected(message.id(), message.control(), errors)
            });
        }
        let accepted = results.iter().filter(|r| r.accepted).count();
        let rejected = results.len() - accepted;
        let ack = self.acknowledgement(&interchange, &scope, &results, stamp)?;
        self.outbox
            .queue(&scope, &ack)
            .map_err(|_| ReceiveError::Unavailable)?;
        let (outcome, code) = if !first_claim {
            ("duplicate", Some(DUPLICATE_INTERCHANGE.to_owned()))
        } else if rejected == 0 {
            ("accepted", None)
        } else if accepted == 0 {
            ("rejected", None)
        } else {
            ("partial", None)
        };
        self.audit(
            &scope,
            &interchange.control,
            outcome,
            accepted,
            rejected,
            code,
        );
        Ok(ReceiveReport {
            control: interchange.control,
            created,
            results,
            acknowledgement: ack,
        })
    }

    /// One set: mapping lookup, schema, mapping, then the lifecycle as the principal. The error
    /// is a stable code.
    fn handle_set(&self, message: &Message, stamp: &Stamp) -> Result<(String, String), String> {
        let Some(set) = self.config.mappings.get(message.id()) else {
            return Err(UNSUPPORTED_SET.to_owned());
        };
        if let Some(schema) = &set.schema
            && let Some(violation) = validate(message, schema).first()
        {
            return Err(violation.code.to_owned());
        }
        let doc = set
            .mapping
            .to_document(message, stamp.micros)
            .map_err(|_| MAPPING_FAILED.to_owned())?;
        match self.sink.create(&self.config.principal, &set.doctype, &doc) {
            Ok(id) => Ok((set.doctype.clone(), id)),
            Err(SinkRefusal::PermissionDenied) => Err(NOT_PERMITTED.to_owned()),
            Err(SinkRefusal::Invalid) => Err(DOCUMENT_INVALID.to_owned()),
            Err(SinkRefusal::Unavailable) => Err(UNAVAILABLE.to_owned()),
        }
    }

    fn acknowledgement(
        &self,
        original: &Interchange,
        scope: &Scope,
        results: &[TransactionResult],
        stamp: &Stamp,
    ) -> Result<String, ReceiveError> {
        let number = self
            .store
            .next_outbound(&Scope {
                partner: format!("ack:{}", scope.partner),
                ..scope.clone()
            })
            .map_err(|_| ReceiveError::Unavailable)?;
        let control = format!("{number:09}");
        let mut request = AckRequest::new(control, stamp.date.clone(), stamp.time.clone());
        request.results = results.to_vec();
        if original.syntax == Syntax::X12
            && let Some(group) = original.groups().first()
        {
            request.functional_group = Some(group.header.text(0).to_owned());
            request.group_control = Some(group.header.text(5).to_owned());
        }
        let ack = acknowledge(original, &request).map_err(|_| ReceiveError::Unavailable)?;
        write(&ack).map_err(|_| ReceiveError::Unavailable)
    }

    /// Renders `doc` to an interchange from `template` (a parsed interchange with the message
    /// to fill), assigns the next control number and delivers it through the egress. Returns the
    /// text and the control number used.
    ///
    /// # Errors
    /// [`ReceiveError::Refused`] when the mapping cannot fill the message;
    /// [`ReceiveError::Unavailable`] when the control store or delivery fails.
    pub fn send(
        &self,
        site: &str,
        connector: &str,
        set_id: &str,
        template: &Interchange,
        doc: &Json,
    ) -> Result<(String, String), ReceiveError> {
        let set = self
            .config
            .mappings
            .get(set_id)
            .ok_or_else(|| ReceiveError::Refused(UNSUPPORTED_SET.to_owned()))?;
        let Some(prototype) = template.messages().into_iter().find(|m| m.id() == set_id) else {
            return Err(ReceiveError::Refused(UNSUPPORTED_SET.to_owned()));
        };
        let message = set
            .mapping
            .from_document(doc, &prototype)
            .map_err(|_| ReceiveError::Refused(MAPPING_FAILED.to_owned()))?;
        let scope = Scope {
            site: site.to_owned(),
            connector: connector.to_owned(),
            partner: template.receiver.id.clone(),
        };
        let number = self
            .store
            .next_outbound(&scope)
            .map_err(|_| ReceiveError::Unavailable)?;
        let (header_at, trailer_at) = match template.syntax {
            Syntax::X12 => (12, 1),
            Syntax::Edifact => (4, 1),
        };
        let control = match template.syntax {
            Syntax::X12 => format!("{number:09}"),
            Syntax::Edifact => number.to_string(),
        };
        let mut header = template.header.clone();
        header.set(header_at, control.clone());
        let mut trailer = template.trailer.clone();
        trailer.set(trailer_at, control.clone());
        let mut segments: Vec<Segment> = Vec::new();
        let (open, close) = (
            template.syntax.message_header(),
            template.syntax.message_trailer(),
        );
        let mut skipping = false;
        for segment in &template.segments {
            if segment.tag == open
                && segment
                    .text(if open == "UNH" { 1 } else { 0 })
                    .starts_with(set_id)
            {
                skipping = true;
                segments.push(message.header.clone());
                segments.extend(message.segments.iter().cloned());
                if let Some(t) = &message.trailer {
                    segments.push(t.clone());
                }
            } else if skipping {
                if segment.tag == close {
                    skipping = false;
                }
            } else {
                segments.push(segment.clone());
            }
        }
        let rendered = Interchange::new(
            template.syntax,
            template.delimiters,
            header,
            trailer,
            segments,
        );
        let text =
            write(&rendered).map_err(|_| ReceiveError::Refused(MAPPING_FAILED.to_owned()))?;
        self.delivery
            .deliver(&scope, &text)
            .map_err(|_| ReceiveError::Unavailable)?;
        Ok((text, control))
    }
}
