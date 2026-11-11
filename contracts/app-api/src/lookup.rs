//! Configurable lookups (ADR-068 §3): how a field gets its candidate values.
//!
//! A [`LookupSpec`] is a **property** (the behaviour layer) on `link`, `dynamic_link`, `select`,
//! `table_multiselect` and `text` (suggestions only). It names a **seam** (ADR-08) that returns
//! candidate rows. The default is [`PLATFORM_LINK_SEARCH`], which searches the target DocType,
//! so a field with no `lookup` behaves like any other link.
//!
//! # The seam contract
//!
//! The seam speaks JSON at the boundary (ADR-055), so native, WASM and script providers are
//! interchangeable. It receives a [`LookupRequest`] and returns a [`LookupPage`]. Resolution is
//! layered, most specific first: the form's view, the site override, the field's definition
//! (its `when` rules, in order, first match wins), then the platform default. The server picks
//! the seam; a client never names one, so desktop and browser behave identically and a client
//! cannot reach an operation it is not allowed to call.
//!
//! # Rules (ADR-068 §3, restated for every provider)
//!
//! - **A lookup suggests; it does not validate.** What is stored is checked as always: a `link`
//!   must name a document of the target DocType that exists, and the caller needs permission to
//!   link to it (`document.invalid_link`). A seam returning an id the engine will not accept
//!   makes a form that cannot save — a configuration error to surface, not a security hole.
//! - **It runs under the caller's permissions.** The lookup operation runs as the user who
//!   opened the form; the seam may only show what that user may see.
//! - **Failure is a typed error, with no silent fallback.** A missing or failing seam returns
//!   an [`AppError`](crate::AppError); it never falls back to the platform default, which would
//!   show the user the wrong list.
//! - **`when` rules are kept, not evaluated.** Conditions need the shared expression language
//!   (ADR-068 §3.9, WP-UI-11); [`lookup_seam`] therefore ignores them and answers with the
//!   unconditional seam.
//!
//! # The wire encoding
//!
//! [`LookupRequest`], [`LookupRow`] and [`LookupPage`] cross the seam as JSON in the ADR-055
//! value encoding. `Name` and `RecordId` have no serde forms of their own (the storage contract
//! is not serde-aware), so the wire structs [`LookupRequestWire`] and [`LookupPageWire`] carry
//! them as their canonical text forms — a storage name and a lower-case hyphenated UUIDv7 —
//! through [`request_wire`], [`page_wire`] and their inverses. That encoding is also the
//! in-memory form; a provider converts once at the boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use spot_storage_contract::{Name, RecordId};

/// The platform's default lookup seam: it searches the target DocType.
pub const PLATFORM_LINK_SEARCH: &str = "platform:link_search";

/// The lookup protocol major version.
pub const LOOKUP_VERSION: u32 = 1;

/// How one field gets its candidate values (ADR-068 §3.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LookupSpec {
    /// The seam to call; `None` means the platform default ([`PLATFORM_LINK_SEARCH`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seam: Option<String>,
    /// The lookup protocol major the field expects ([`LOOKUP_VERSION`]).
    #[serde(default)]
    pub version: u32,
    /// Fixed parameters passed to every call.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// Conditional choices, evaluated in order, first match wins once the shared expression
    /// language exists (WP-UI-11). Until then they are parsed and kept, never evaluated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when: Vec<LookupRule>,
}

impl LookupSpec {
    /// The spec with `seam` set and nothing else.
    pub fn seam(seam: impl Into<String>) -> Self {
        Self {
            seam: Some(seam.into()),
            version: LOOKUP_VERSION,
            params: BTreeMap::new(),
            when: Vec::new(),
        }
    }
}

/// One conditional choice of a lookup (ADR-068 §3.2): the seam to use while `if_expr` holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LookupRule {
    /// The condition, in the shared expression language (not evaluated until WP-UI-11).
    #[serde(rename = "if")]
    pub if_expr: String,
    /// The seam to call when the condition holds.
    pub seam: String,
    /// Parameters added to the spec's own for this choice.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

/// The named seam of a lookup, or the platform default. `when` rules are **not** evaluated:
/// conditions need the expression language (ADR-068 §3.9), so for now the unconditional seam
/// is the only one that can answer.
pub fn lookup_seam(spec: &LookupSpec) -> &str {
    match &spec.seam {
        Some(s) => s,
        None => PLATFORM_LINK_SEARCH,
    }
}

/// A lookup request (ADR-068 §3.3): what the lookup operation sends to the seam.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LookupRequest {
    /// The DocType the form is working on.
    pub doctype: Name,
    /// The field being filled.
    pub field: Name,
    /// The text typed so far.
    pub text: String,
    /// Where to continue a previous page; `None` starts at the top.
    pub cursor: Option<String>,
    /// The current document snapshot.
    pub doc: Option<Json>,
    /// The parent document, when the field is on a child row.
    pub parent: Option<Json>,
    /// The lookup's fixed parameters.
    pub params: BTreeMap<String, String>,
}

/// One candidate row.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LookupRow {
    /// The document to link.
    pub id: RecordId,
    /// What the user picks from.
    pub label: String,
    /// A second line, when the seam offers one.
    pub description: Option<String>,
    /// Values the form may copy into other fields when the row is picked (the feeding side of
    /// `fetch_from`, WP-MET-02).
    pub extras: BTreeMap<String, Json>,
}

/// A page of candidates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LookupPage {
    /// The rows of this page.
    pub rows: Vec<LookupRow>,
    /// Whether another page follows.
    pub has_more: bool,
    /// The cursor to pass for it; `None` when `has_more` is false.
    pub cursor: Option<String>,
}

/// The wire form of a lookup request: names as storage-name strings, as the seam receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupRequestWire {
    /// The DocType the form is working on.
    pub doctype: String,
    /// The field being filled.
    pub field: String,
    /// The text typed so far.
    pub text: String,
    /// Where to continue a previous page; `None` starts at the top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The current document snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<Json>,
    /// The parent document, when the field is on a child row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Json>,
    /// The lookup's fixed parameters.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

/// The wire form of a page: ids as canonical hyphenated UUIDv7 strings, as the seam returns it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupPageWire {
    /// The rows of this page.
    pub rows: Vec<LookupRowWire>,
    /// Whether another page follows.
    pub has_more: bool,
    /// The cursor to pass for it; `None` when `has_more` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// The wire form of one candidate row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupRowWire {
    /// The document to link, as a canonical hyphenated UUIDv7.
    pub id: String,
    /// What the user picks from.
    pub label: String,
    /// A second line, when the seam offers one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Values the form may copy into other fields when the row is picked.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, Json>,
}

impl From<&LookupRequest> for LookupRequestWire {
    fn from(r: &LookupRequest) -> Self {
        Self {
            doctype: r.doctype.as_str().to_owned(),
            field: r.field.as_str().to_owned(),
            text: r.text.clone(),
            cursor: r.cursor.clone(),
            doc: r.doc.clone(),
            parent: r.parent.clone(),
            params: r.params.clone(),
        }
    }
}

impl TryFrom<&LookupRequestWire> for LookupRequest {
    type Error = String;

    fn try_from(w: &LookupRequestWire) -> Result<Self, Self::Error> {
        let invalid = |what: &str, s: &str| format!("{what} `{s}` is not a storage name");
        Ok(Self {
            doctype: Name::new(&w.doctype).map_err(|_| invalid("doctype", &w.doctype))?,
            field: Name::new(&w.field).map_err(|_| invalid("field", &w.field))?,
            text: w.text.clone(),
            cursor: w.cursor.clone(),
            doc: w.doc.clone(),
            parent: w.parent.clone(),
            params: w.params.clone(),
        })
    }
}

impl TryFrom<&LookupPage> for LookupPageWire {
    type Error = String;

    fn try_from(p: &LookupPage) -> Result<Self, Self::Error> {
        Ok(Self {
            rows: p
                .rows
                .iter()
                .map(|r| LookupRowWire {
                    id: r.id.to_string(),
                    label: r.label.clone(),
                    description: r.description.clone(),
                    extras: r.extras.clone(),
                })
                .collect(),
            has_more: p.has_more,
            cursor: p.cursor.clone(),
        })
    }
}

impl TryFrom<&LookupPageWire> for LookupPage {
    type Error = String;

    fn try_from(w: &LookupPageWire) -> Result<Self, Self::Error> {
        let bad = |s: &str| format!("row id `{s}` is not a canonical UUIDv7");
        Ok(Self {
            rows: w
                .rows
                .iter()
                .map(|r| {
                    Ok(LookupRow {
                        id: parse_record_id(&r.id).ok_or_else(|| bad(&r.id))?,
                        label: r.label.clone(),
                        description: r.description.clone(),
                        extras: r.extras.clone(),
                    })
                })
                .collect::<Result<Vec<_>, Self::Error>>()?,
            has_more: w.has_more,
            cursor: w.cursor.clone(),
        })
    }
}

/// A canonical lower-case hyphenated UUIDv7 id, the wire form of a [`RecordId`]. Parsing by
/// position and nibble, validating shape, version and variant, then rebuilding the bytes: the
/// storage contract owns ids and this crate adds no dependency to reach its parser.
fn parse_record_id(s: &str) -> Option<RecordId> {
    let b = s.as_bytes();
    let hyphen = |i: usize| b.get(i) == Some(&b'-');
    let hex = |c: u8| matches!(c, b'0'..=b'9' | b'a'..=b'f');
    if b.len() != 36
        || !(hyphen(8) && hyphen(13) && hyphen(18) && hyphen(23))
        || !b
            .iter()
            .copied()
            .enumerate()
            .all(|(i, c)| matches!(i, 8 | 13 | 18 | 23) || hex(c))
    {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        // `hex` above proved every character is one of these before `nib` runs.
        _ => 0,
    };
    let mut bytes = [0u8; 16];
    let positions = [
        0usize, 2, 4, 6, 9, 11, 14, 16, 19, 21, 24, 26, 28, 30, 32, 34,
    ];
    for (out, pos) in positions.iter().enumerate() {
        bytes[out] = nib(b[*pos]) << 4 | nib(b[*pos + 1]);
    }
    // Version 7 in the high nibble of byte 6, RFC 4122 variant in the top bits of byte 8.
    if bytes[6] >> 4 != 0x7 || bytes[8] >> 6 != 0b10 {
        return None;
    }
    RecordId::from_bytes(bytes).ok()
}

/// Serialises a request in the ADR-055 value encoding: what a provider receives on the wire.
/// The wire struct is strings, maps and embedded JSON throughout, so this is infallible; a
/// failure (a programming bug) degrades to an error message instead of a panic.
pub fn request_wire(r: &LookupRequest) -> Json {
    serde_json::to_value(LookupRequestWire::from(r))
        .unwrap_or_else(|e| Json::String(format!("lookup request did not encode: {e}")))
}

/// Serialises a page in the ADR-055 value encoding: what a provider returns.
pub fn page_wire(p: &LookupPage) -> Result<Json, String> {
    serde_json::to_value(LookupPageWire::try_from(p)?)
        .map_err(|e| format!("lookup page did not encode: {e}"))
}

/// Parses a page back from the wire.
pub fn page_from_wire(j: &Json) -> Result<LookupPage, String> {
    let wire: LookupPageWire =
        serde_json::from_value(j.clone()).map_err(|e| format!("lookup page malformed: {e}"))?;
    LookupPage::try_from(&wire)
}

/// Parses a request back from the wire.
pub fn request_from_wire(j: &Json) -> Result<LookupRequest, String> {
    let wire: LookupRequestWire =
        serde_json::from_value(j.clone()).map_err(|e| format!("lookup request malformed: {e}"))?;
    LookupRequest::try_from(&wire)
}
