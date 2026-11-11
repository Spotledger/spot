//! Acknowledgements: the X12 `997` and the EDIFACT `CONTRL` (WP-INT-06.T1, brief item 5).
//!
//! Given the results of validating or applying an interchange, [`acknowledge`] builds the
//! matching acknowledgement interchange: a `997` functional acknowledgement wrapped in
//! `ISA`/`GS`/`IEA`/`GE` for X12, or a `CONTRL` message wrapped in `UNB`/`UNZ` for EDIFACT.
//! Each transaction gets an accept or reject action and, when it is not clean, the error codes
//! that go with it. The sender and receiver are swapped and the acknowledgement's own control
//! number and date/time are arguments — the wall clock is never read.

use serde::{Deserialize, Serialize};

use crate::delimiters::{Delimiters, Syntax};
use crate::error::Result;
use crate::model::{Interchange, Message, Party, Segment};

/// How one transaction fared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionResult {
    /// The transaction set id (X12 `ST01`) or message type (EDIFACT `UNH02`), for example `850`
    /// or `ORDERS`.
    pub set_id: String,
    /// The transaction's control number (`ST02`/`UNH01`).
    pub control: String,
    /// Whether the transaction was accepted.
    pub accepted: bool,
    /// The error codes that apply, when it was not.
    #[serde(default)]
    pub errors: Vec<String>,
}

impl TransactionResult {
    /// An accepted transaction.
    pub fn accepted(set_id: impl Into<String>, control: impl Into<String>) -> Self {
        Self {
            set_id: set_id.into(),
            control: control.into(),
            accepted: true,
            errors: Vec::new(),
        }
    }

    /// A rejected transaction with its error codes.
    pub fn rejected(
        set_id: impl Into<String>,
        control: impl Into<String>,
        errors: Vec<String>,
    ) -> Self {
        Self {
            set_id: set_id.into(),
            control: control.into(),
            accepted: false,
            errors,
        }
    }

    /// The acknowledgement's action code for this transaction: accepted, accepted with errors,
    /// or rejected.
    pub fn action(&self) -> Action {
        if self.accepted && self.errors.is_empty() {
            Action::Accepted
        } else if self.accepted {
            Action::AcceptedWithErrors
        } else {
            Action::Rejected
        }
    }
}

/// The acknowledgement action for a transaction or a whole interchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Everything was accepted.
    Accepted,
    /// Accepted, but with errors noted.
    AcceptedWithErrors,
    /// Rejected.
    Rejected,
}

impl Action {
    /// The X12 `AK5`/`AK9` action code.
    pub fn x12(self) -> &'static str {
        match self {
            Self::Accepted => "A",
            Self::AcceptedWithErrors => "E",
            Self::Rejected => "R",
        }
    }

    /// The EDIFACT `CONTRL` action code.
    pub fn edifact(self) -> &'static str {
        match self {
            Self::Accepted => "7",
            Self::AcceptedWithErrors => "4",
            Self::Rejected => "8",
        }
    }
}

/// What an acknowledgement needs besides the results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckRequest {
    /// The acknowledgement interchange's control number.
    pub control: String,
    /// The date (`YYMMDD` for X12, `CCYYMMDD` for EDIFACT).
    pub date: String,
    /// The time (`HHMM`).
    pub time: String,
    /// The X12 functional group code (`AK1`), for example `PO`; ignored for EDIFACT.
    #[serde(default)]
    pub functional_group: Option<String>,
    /// The X12 functional group control number; ignored for EDIFACT.
    #[serde(default)]
    pub group_control: Option<String>,
    /// The results, one per transaction.
    #[serde(default)]
    pub results: Vec<TransactionResult>,
}

impl AckRequest {
    /// A request with no results yet.
    pub fn new(
        control: impl Into<String>,
        date: impl Into<String>,
        time: impl Into<String>,
    ) -> Self {
        Self {
            control: control.into(),
            date: date.into(),
            time: time.into(),
            functional_group: None,
            group_control: None,
            results: Vec::new(),
        }
    }

    /// The overall action: rejected when any transaction is rejected, accepted with errors when
    /// any is accepted with errors, accepted otherwise.
    pub fn overall(&self) -> Action {
        if self.results.iter().any(|r| r.action() == Action::Rejected) {
            Action::Rejected
        } else if self
            .results
            .iter()
            .any(|r| r.action() == Action::AcceptedWithErrors)
        {
            Action::AcceptedWithErrors
        } else {
            Action::Accepted
        }
    }
}

/// Builds the acknowledgement interchange for `original` under `request`.
pub fn acknowledge(original: &Interchange, request: &AckRequest) -> Result<Interchange> {
    match original.syntax {
        Syntax::X12 => acknowledge_x12(original, request),
        Syntax::Edifact => acknowledge_edifact(original, request),
    }
}

/// Builds a `997` transaction set (not yet wrapped in an interchange).
pub fn message_997(request: &AckRequest, group_control: &str) -> Result<Message> {
    let mut segments = Vec::new();
    let mut header = Segment::new("ST");
    header.push("997");
    header.push(request.control.clone());
    segments.push(header.clone());

    let mut ak1 = Segment::new("AK1");
    ak1.push(
        request
            .functional_group
            .clone()
            .unwrap_or_else(|| "FA".to_owned()),
    );
    ak1.push(group_control.to_owned());
    segments.push(ak1);

    for result in &request.results {
        let mut ak2 = Segment::new("AK2");
        ak2.push(result.set_id.clone());
        ak2.push(result.control.clone());
        segments.push(ak2);
        for error in &result.errors {
            let mut ak3 = Segment::new("AK3");
            ak3.push("SE");
            ak3.push("1");
            segments.push(ak3);
            let mut ak4 = Segment::new("AK4");
            ak4.push("0");
            ak4.push("0");
            ak4.push(error.clone());
            segments.push(ak4);
        }
        let mut ak5 = Segment::new("AK5");
        ak5.push(result.action().x12());
        segments.push(ak5);
    }

    let mut ak9 = Segment::new("AK9");
    ak9.push(request.overall().x12());
    ak9.push(request.results.len().to_string());
    ak9.push(request.results.len().to_string());
    ak9.push(
        request
            .results
            .iter()
            .filter(|r| r.action() != Action::Rejected)
            .count()
            .to_string(),
    );
    segments.push(ak9);

    let count = (segments.len() + 1).to_string();
    let mut trailer = Segment::new("SE");
    trailer.push(count);
    trailer.push(request.control.clone());
    segments.push(trailer.clone());
    Ok(Message {
        header,
        trailer: Some(trailer),
        segments: segments[1..segments.len() - 1].to_vec(),
    })
}

/// Builds a `CONTRL` message (not yet wrapped in an interchange).
pub fn message_contrl(request: &AckRequest, interchange_control: &str) -> Result<Message> {
    let control = if request.control.is_empty() {
        "1".to_owned()
    } else {
        request.control.clone()
    };
    let mut header = Segment::new("UNH");
    header.push(control.clone());
    header.push("CONTRL:D:3:1:UN");
    let mut segments = vec![header.clone()];

    let mut uci = Segment::new("UCI");
    uci.push(interchange_control.to_owned());
    uci.push(String::new());
    uci.push(String::new());
    uci.push(request.overall().edifact());
    segments.push(uci);

    for result in &request.results {
        let mut ucm = Segment::new("UCM");
        ucm.push(result.control.clone());
        ucm.push(result.set_id.clone());
        ucm.push(result.action().edifact());
        segments.push(ucm);
        for error in &result.errors {
            let mut ucd = Segment::new("UCD");
            ucd.push(error.clone());
            ucd.push("0");
            ucd.push(String::new());
            segments.push(ucd);
        }
    }

    let count = (segments.len() + 1).to_string();
    let mut trailer = Segment::new("UNT");
    trailer.push(count);
    trailer.push(control);
    segments.push(trailer.clone());
    Ok(Message {
        header,
        trailer: Some(trailer),
        segments: segments[1..segments.len() - 1].to_vec(),
    })
}

/// Wraps a `997` in an X12 `ISA`/`GS`/`GE`/`IEA` interchange.
fn acknowledge_x12(original: &Interchange, request: &AckRequest) -> Result<Interchange> {
    let delimiters = original.delimiters;
    let group_control = request
        .group_control
        .clone()
        .unwrap_or_else(|| request.control.clone());
    let message = message_997(request, &group_control)?;

    let header = build_isa(original, request, delimiters)?;
    let mut gs = Segment::new("GS");
    gs.push("FA");
    gs.push(original.receiver.id.clone());
    gs.push(original.sender.id.clone());
    gs.push(request.date.clone());
    gs.push(request.time.clone());
    gs.push(group_control.clone());
    gs.push("X");
    gs.push(original_group_version(original));
    let mut ge = Segment::new("GE");
    ge.push("1");
    ge.push(group_control);

    let mut body = vec![gs];
    body.extend(message.all_segments().into_iter().cloned());
    body.push(ge);

    let mut trailer = Segment::new("IEA");
    trailer.push("1");
    trailer.push(request.control.clone());
    Ok(Interchange::new(
        Syntax::X12,
        delimiters,
        header,
        trailer,
        body,
    ))
}

/// Wraps a `CONTRL` in an EDIFACT `UNB`/`UNZ` interchange.
fn acknowledge_edifact(original: &Interchange, request: &AckRequest) -> Result<Interchange> {
    let delimiters = original.delimiters;
    let message = message_contrl(request, &original.control)?;
    let mut unb = Segment::new("UNB");
    unb.push(original.header.text(0).to_owned());
    unb.push(party_text(&original.receiver));
    unb.push(party_text(&original.sender));
    unb.push(format!("{}:{}", request.date, request.time));
    unb.push(request.control.clone());
    unb.push(String::new());
    unb.push(String::new());
    unb.push(original.header.text(7).to_owned());

    let body = message.all_segments().into_iter().cloned().collect();
    let mut trailer = Segment::new("UNZ");
    trailer.push("1");
    trailer.push(request.control.clone());
    Ok(Interchange::new(
        Syntax::Edifact,
        delimiters,
        unb,
        trailer,
        body,
    ))
}

/// Builds an `ISA` header with sender and receiver swapped.
fn build_isa(
    original: &Interchange,
    request: &AckRequest,
    delimiters: Delimiters,
) -> Result<Segment> {
    let mut isa = Segment::new("ISA");
    isa.push(original.header.text(0).to_owned());
    isa.push(original.header.text(1).to_owned());
    isa.push(original.header.text(2).to_owned());
    isa.push(original.header.text(3).to_owned());
    // Sender becomes the original receiver, and vice versa.
    isa.push(original.header.text(6).to_owned());
    isa.push(original.header.text(7).to_owned());
    isa.push(original.header.text(4).to_owned());
    isa.push(original.header.text(5).to_owned());
    isa.push(request.date.clone());
    isa.push(request.time.clone());
    isa.push(
        delimiters
            .repetition
            .map(|c| c.to_string())
            .unwrap_or_default(),
    );
    isa.push(original.header.text(11).to_owned());
    isa.push(request.control.clone());
    isa.push("0".to_owned());
    isa.push(original.header.text(14).to_owned());
    isa.push(delimiters.component.to_string());
    Ok(isa)
}

/// The X12 version from the original's `GS08`, or the common `004010`.
fn original_group_version(original: &Interchange) -> String {
    let version = original
        .segments
        .iter()
        .find(|s| s.tag == "GS")
        .map(|s| s.text(7).to_owned())
        .unwrap_or_default();
    if version.is_empty() {
        "004010".to_owned()
    } else {
        version
    }
}

/// Renders a party as EDIFACT `id:qualifier`.
fn party_text(party: &Party) -> String {
    match &party.qualifier {
        Some(qualifier) => format!("{}:{}", party.id, qualifier),
        None => party.id.clone(),
    }
}

/// Reads the transaction results out of an interchange's messages (id and control only; the
/// caller supplies accept/reject). A convenience for the common "acknowledge what I parsed".
pub fn results_from(interchange: &Interchange, accepted: bool) -> Vec<TransactionResult> {
    interchange
        .messages()
        .iter()
        .map(|message| {
            if accepted {
                TransactionResult::accepted(message.id().to_owned(), message.control().to_owned())
            } else {
                TransactionResult::rejected(
                    message.id().to_owned(),
                    message.control().to_owned(),
                    Vec::new(),
                )
            }
        })
        .collect()
}
