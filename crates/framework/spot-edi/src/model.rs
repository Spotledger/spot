//! The typed segment/element model (WP-INT-06.T1, brief item 2).
//!
//! An [`Interchange`] is the header segment, the trailer segment and every segment in between.
//! The header and trailer are kept verbatim so a parse/write round trip is exact, while
//! [`Interchange::sender`], [`Interchange::receiver`] and [`Interchange::control`] expose the
//! envelope's identity and [`Interchange::groups`] and [`Interchange::messages`] give the
//! nested view (functional groups and transaction sets or messages).
//!
//! A [`Segment`] is a tag and a list of [`Element`]s. An element is either a [`Element::Simple`]
//! value, a [`Element::Composite`] of components, or a [`Element::Repeated`] list of simple or
//! composite elements. Elements are indexed from 0; EDI's "element 1" is `elements[0]`.
//!
//! Every value is the text as it appears in the document, with EDIFACT release sequences
//! removed. Whitespace is significant: X12 pads fixed-width envelope fields with spaces, and
//! those spaces live in the element's text.

use serde::{Deserialize, Serialize};

use crate::delimiters::{Delimiters, Syntax};

/// A party in an envelope: an identifier and, when the syntax carries one, a qualifier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    /// The identifier (X12 ISA06/ISA08, EDIFACT UNB02/UNB03), trimmed of padding.
    pub id: String,
    /// The qualifier (X12 ISA05/ISA07, the second component of EDIFACT UNB02/UNB03), when
    /// present.
    pub qualifier: Option<String>,
}

impl Party {
    /// A party with an identifier and no qualifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            qualifier: None,
        }
    }

    /// A party with an identifier and a qualifier.
    pub fn qualified(id: impl Into<String>, qualifier: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            qualifier: Some(qualifier.into()),
        }
    }
}

/// One element of a segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Element {
    /// A single value (`Simple("ABC")`).
    Simple(String),
    /// A composite of one or more components (`Composite(vec!["A", "B"])`).
    Composite(Vec<String>),
    /// A repeated element: one or more simple or composite elements.
    Repeated(Vec<Element>),
}

impl Element {
    /// A simple element.
    pub fn simple(value: impl Into<String>) -> Self {
        Self::Simple(value.into())
    }

    /// A composite element.
    pub fn composite(components: Vec<String>) -> Self {
        Self::Composite(components)
    }

    /// A repeated element.
    pub fn repeated(items: Vec<Element>) -> Self {
        Self::Repeated(items)
    }

    /// The element's text when it is simple.
    pub fn as_simple(&self) -> Option<&str> {
        match self {
            Self::Simple(s) => Some(s),
            _ => None,
        }
    }

    /// The element's first value: a simple's text, a composite's first component, or a
    /// repetition's first value. `None` for an empty composite or repetition.
    pub fn first(&self) -> Option<&str> {
        match self {
            Self::Simple(s) => Some(s),
            Self::Composite(parts) => parts.first().map(String::as_str),
            Self::Repeated(items) => items.first().and_then(Element::first),
        }
    }

    /// Whether the element carries no value at all (`Simple("")`).
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Simple(s) if s.is_empty())
    }

    /// Every component of a composite, the single value of a simple, or the first values of a
    /// repetition.
    pub fn components(&self) -> Vec<&str> {
        match self {
            Self::Simple(s) => vec![s],
            Self::Composite(parts) => parts.iter().map(String::as_str).collect(),
            Self::Repeated(items) => items.iter().filter_map(Element::first).collect(),
        }
    }
}

/// One segment: a tag and its elements.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    /// The segment tag (`BEG`, `N1`, `UNH`, ...).
    pub tag: String,
    /// The elements, indexed from 0.
    pub elements: Vec<Element>,
    /// The byte offset of the segment in the source document, for error reporting; 0 when the
    /// segment was built by hand. Not part of a segment's identity.
    #[serde(skip)]
    pub offset: usize,
}

impl Segment {
    /// A segment with a tag and no elements.
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            elements: Vec::new(),
            offset: 0,
        }
    }

    /// A segment of simple elements.
    pub fn simple(tag: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            tag: tag.into(),
            elements: values.into_iter().map(Element::Simple).collect(),
            offset: 0,
        }
    }

    /// A segment of the given elements.
    pub fn with(tag: impl Into<String>, elements: Vec<Element>) -> Self {
        Self {
            tag: tag.into(),
            elements,
            offset: 0,
        }
    }

    /// The element at `index` (0-based; EDI element `index + 1`).
    pub fn element(&self, index: usize) -> Option<&Element> {
        self.elements.get(index)
    }

    /// The text of the element at `index`, or `""` when it is absent.
    pub fn text(&self, index: usize) -> &str {
        self.elements
            .get(index)
            .and_then(Element::first)
            .unwrap_or("")
    }

    /// Pushes a simple element.
    pub fn push(&mut self, value: impl Into<String>) {
        self.elements.push(Element::Simple(value.into()));
    }

    /// Sets the element at `index` to a simple value, padding with empty elements as needed.
    pub fn set(&mut self, index: usize, value: impl Into<String>) {
        while self.elements.len() <= index {
            self.elements.push(Element::Simple(String::new()));
        }
        self.elements[index] = Element::Simple(value.into());
    }
}

impl PartialEq for Segment {
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag && self.elements == other.elements
    }
}

impl Eq for Segment {}

/// A transaction set (X12 `ST`..`SE`) or message (EDIFACT `UNH`..`UNT`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// The `ST`/`UNH` header segment.
    pub header: Segment,
    /// The `SE`/`UNT` trailer segment, when the message is closed.
    pub trailer: Option<Segment>,
    /// The segments between the header and the trailer.
    pub segments: Vec<Segment>,
}

impl Message {
    /// The message's type: X12 `ST01` (for example `850`) or EDIFACT `UNH02`'s first component
    /// (for example `ORDERS`).
    pub fn id(&self) -> &str {
        match &self.header.tag[..] {
            "UNH" => self.header.text(1).split(':').next().unwrap_or_default(),
            _ => self.header.text(0),
        }
    }

    /// The message's control number: X12 `ST02` or EDIFACT `UNH01`.
    pub fn control(&self) -> &str {
        match &self.header.tag[..] {
            "UNH" => self.header.text(0),
            _ => self.header.text(1),
        }
    }

    /// Every segment of the message, header and trailer included, in order.
    pub fn all_segments(&self) -> Vec<&Segment> {
        let mut out = vec![&self.header];
        out.extend(self.segments.iter());
        if let Some(trailer) = &self.trailer {
            out.push(trailer);
        }
        out
    }
}

/// A functional group (X12 `GS`..`GE`) or group (EDIFACT `UNG`..`UNE`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// The `GS`/`UNG` header segment.
    pub header: Segment,
    /// The `GE`/`UNE` trailer segment, when the group is closed.
    pub trailer: Option<Segment>,
    /// The group's messages.
    pub messages: Vec<Message>,
}

/// A whole interchange.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interchange {
    /// The syntax the document is written in.
    pub syntax: Syntax,
    /// The separators the document uses.
    pub delimiters: Delimiters,
    /// The `ISA`/`UNB` header segment, verbatim.
    pub header: Segment,
    /// The `IEA`/`UNZ` trailer segment, verbatim.
    pub trailer: Segment,
    /// The sender, read from the header.
    pub sender: Party,
    /// The receiver, read from the header.
    pub receiver: Party,
    /// The interchange control number, read from the header.
    pub control: String,
    /// Every segment between the header and the trailer, in order.
    pub segments: Vec<Segment>,
}

impl Interchange {
    /// Builds an interchange from its header and trailer, reading the envelope's identity out
    /// of the header and taking `segments` as the body.
    pub fn new(
        syntax: Syntax,
        delimiters: Delimiters,
        header: Segment,
        trailer: Segment,
        segments: Vec<Segment>,
    ) -> Self {
        let (sender, receiver, control) = envelope_identity(syntax, &header);
        Self {
            syntax,
            delimiters,
            header,
            trailer,
            sender,
            receiver,
            control,
            segments,
        }
    }

    /// Every segment of the interchange, header and trailer included, in order.
    pub fn all_segments(&self) -> Vec<&Segment> {
        let mut out = vec![&self.header];
        out.extend(self.segments.iter());
        out.push(&self.trailer);
        out
    }

    /// The messages in the interchange, whether or not they sit inside a functional group.
    pub fn messages(&self) -> Vec<Message> {
        let header_tag = self.syntax.message_header();
        let trailer_tag = self.syntax.message_trailer();
        let mut out = Vec::new();
        let mut current: Option<(Segment, Vec<Segment>)> = None;
        for segment in &self.segments {
            if segment.tag == header_tag {
                if let Some((header, body)) = current.take() {
                    out.push(Message {
                        header,
                        trailer: None,
                        segments: body,
                    });
                }
                current = Some((segment.clone(), Vec::new()));
            } else if segment.tag == trailer_tag {
                if let Some((header, body)) = current.take() {
                    out.push(Message {
                        header,
                        trailer: Some(segment.clone()),
                        segments: body,
                    });
                }
            } else if let Some((_, body)) = current.as_mut() {
                body.push(segment.clone());
            }
        }
        if let Some((header, body)) = current.take() {
            out.push(Message {
                header,
                trailer: None,
                segments: body,
            });
        }
        out
    }

    /// The functional groups in the interchange. Messages that sit directly under the
    /// interchange (legal in EDIFACT) appear in a synthetic group with an empty header.
    pub fn groups(&self) -> Vec<Group> {
        let group_header = self.syntax.group_header();
        let group_trailer = self.syntax.group_trailer();
        let mut out: Vec<Group> = Vec::new();
        let mut current: Option<(Segment, Vec<Segment>)> = None;
        let mut loose: Vec<Segment> = Vec::new();
        for segment in &self.segments {
            if segment.tag == group_header {
                if let Some((header, body)) = current.take() {
                    out.push(group_from(header, None, body, self.syntax));
                }
                current = Some((segment.clone(), Vec::new()));
            } else if segment.tag == group_trailer {
                if let Some((header, body)) = current.take() {
                    out.push(group_from(header, Some(segment.clone()), body, self.syntax));
                }
            } else if let Some((_, body)) = current.as_mut() {
                body.push(segment.clone());
            } else if segment.tag == self.syntax.message_header()
                || segment.tag == self.syntax.message_trailer()
            {
                loose.push(segment.clone());
            }
        }
        if let Some((header, body)) = current.take() {
            out.push(group_from(header, None, body, self.syntax));
        }
        if !loose.is_empty() {
            let group = Group {
                header: Segment::new(""),
                trailer: None,
                messages: split_messages(self.syntax, &loose),
            };
            out.push(group);
        }
        out
    }

    /// The interchange control number of the trailer (`IEA02`/`UNZ02`).
    pub fn trailer_control(&self) -> &str {
        match self.syntax {
            Syntax::X12 => self.trailer.text(1),
            Syntax::Edifact => self.trailer.text(1),
        }
    }

    /// The count the trailer declares (`IEA01`/`UNZ01`).
    pub fn trailer_count(&self) -> &str {
        self.trailer.text(0)
    }
}

/// Splits a run of segments into messages by the syntax's message header/trailer tags.
fn split_messages(syntax: Syntax, segments: &[Segment]) -> Vec<Message> {
    let header_tag = syntax.message_header();
    let trailer_tag = syntax.message_trailer();
    let mut out = Vec::new();
    let mut current: Option<(Segment, Vec<Segment>)> = None;
    for segment in segments {
        if segment.tag == header_tag {
            if let Some((header, body)) = current.take() {
                out.push(Message {
                    header,
                    trailer: None,
                    segments: body,
                });
            }
            current = Some((segment.clone(), Vec::new()));
        } else if segment.tag == trailer_tag {
            if let Some((header, body)) = current.take() {
                out.push(Message {
                    header,
                    trailer: Some(segment.clone()),
                    segments: body,
                });
            }
        } else if let Some((_, body)) = current.as_mut() {
            body.push(segment.clone());
        }
    }
    if let Some((header, body)) = current.take() {
        out.push(Message {
            header,
            trailer: None,
            segments: body,
        });
    }
    out
}

fn group_from(
    header: Segment,
    trailer: Option<Segment>,
    body: Vec<Segment>,
    syntax: Syntax,
) -> Group {
    Group {
        header,
        trailer,
        messages: split_messages(syntax, &body),
    }
}

/// Reads the sender, receiver and control number out of an interchange header.
pub fn envelope_identity(syntax: Syntax, header: &Segment) -> (Party, Party, String) {
    match syntax {
        Syntax::X12 => {
            let sender = Party {
                id: header.text(5).trim().to_owned(),
                qualifier: nonempty(header.text(4)),
            };
            let receiver = Party {
                id: header.text(7).trim().to_owned(),
                qualifier: nonempty(header.text(6)),
            };
            (sender, receiver, header.text(12).trim().to_owned())
        }
        Syntax::Edifact => {
            let sender = party(&full_text(header, 1));
            let receiver = party(&full_text(header, 2));
            (sender, receiver, header.text(4).trim().to_owned())
        }
    }
}

/// The full text of an element: a simple value, or a composite's components joined by `:`, so
/// an EDIFACT party (`id:qualifier`) can be split apart.
fn full_text(segment: &Segment, index: usize) -> String {
    match segment.element(index) {
        Some(Element::Simple(value)) => value.clone(),
        Some(Element::Composite(parts)) => parts.join(":"),
        Some(Element::Repeated(items)) => items
            .iter()
            .filter_map(Element::first)
            .collect::<Vec<_>>()
            .join(":"),
        None => String::new(),
    }
}

/// Parses an EDIFACT party (`id:qualifier`).
fn party(text: &str) -> Party {
    let mut parts = text.splitn(2, ':');
    let id = parts.next().unwrap_or_default().trim().to_owned();
    let qualifier = parts.next().and_then(nonempty);
    Party { id, qualifier }
}

fn nonempty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}
