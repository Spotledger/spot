//! The lookup operation's domain and wire types (ADR-068 §3, contract v0.2.0): the request
//! the endpoint decodes, the page a [`LookupService`](crate::api::service::LookupService)
//! returns, and the lookup spec the meta endpoint encodes.
//!
//! The types mirror `spot_app_api::lookup` (the seam contract's in-memory form) without
//! depending on it: the crate's main dependencies stay as the brief fixes them, and values
//! cross the boundary as JSON per ADR-055. `LookupSpec` here is the contract's meta shape —
//! the definition's spec, for information; a client never sends it and `when` rules are the
//! server's (ADR-068 §3.2), so they are not part of the type.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_storage_contract::RecordId;

/// The lookup spec a field's definition carries, as the contract's `LookupSpec` meta schema:
/// the named seam (absent means the platform default), the protocol major and the fixed
/// parameters. Conditional (`when`) choices are the server's and are not exposed (WP-UI-11).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LookupSpec {
    /// The named seam, or `None` for the platform default `platform:link_search`.
    pub seam: Option<String>,
    /// The lookup protocol major; `0` means the definition does not state one and the
    /// property is omitted in meta (the contract schema requires a value of at least 1).
    pub version: u64,
    /// Fixed parameters passed to every call.
    pub params: BTreeMap<String, String>,
}

impl LookupSpec {
    /// The contract's JSON form (`components.schemas.LookupSpec`): `seam` absent means the
    /// platform default, and `version`/`params` are omitted when unset or empty.
    pub fn to_json(&self) -> Json {
        let mut map = serde_json::Map::new();
        if let Some(seam) = &self.seam {
            map.insert("seam".to_owned(), Json::String(seam.clone()));
        }
        if self.version > 0 {
            map.insert("version".to_owned(), Json::from(self.version));
        }
        if !self.params.is_empty() {
            let mut params = serde_json::Map::new();
            for (key, value) in &self.params {
                params.insert(key.clone(), Json::String(value.clone()));
            }
            map.insert("params".to_owned(), Json::Object(params));
        }
        Json::Object(map)
    }
}

/// A lookup request (ADR-068 §3.3), as the endpoint decoded it: the text typed so far, the
/// opaque page cursor, the current document snapshot and the parent document for a field on
/// a child row. The seam's fixed parameters are the service's business (it resolves the
/// seam, ADR-068 §3.4): the endpoint passes the client's four parts only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LookupRequest {
    /// The `text` typed so far; the contract allows an empty string.
    pub text: String,
    /// The `cursor` from a previous page's response, as an opaque string; `None` starts at
    /// the top. The endpoint does not decode it.
    pub cursor: Option<String>,
    /// The `doc` snapshot, an object in the contract's value encoding; the seam reads it.
    pub doc: Option<Json>,
    /// The `parent` document, when the field is on a child row.
    pub parent: Option<Json>,
}

/// One candidate row (the contract's `LookupRow`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupRow {
    /// The document to link, as a record id; the encoder renders the canonical
    /// lower-case hyphenated UUIDv7 the contract's `RecordId` schema fixes.
    pub id: RecordId,
    /// What the user picks from.
    pub label: String,
    /// A second line, when the seam offers one.
    pub description: Option<String>,
    /// Values the form may copy into other fields when the row is picked (the feeding side
    /// of `fetch_from`, WP-MET-02), in the value encoding. The field types the extras carry
    /// are the target fields'; the service encodes them.
    pub extras: BTreeMap<String, Json>,
    /// The text of each display column of the page, by column name (contract 0.5.0); a row
    /// may omit a column. Display only: a cell is never a value to copy. At most
    /// [`MAX_CELL_CHARS`] characters each; empty when the seam gives a single column.
    pub cells: BTreeMap<String, String>,
}

/// The contract's `maxLength` of a cell.
pub const MAX_CELL_CHARS: usize = 1024;
/// The contract's `maxItems` of a page's `columns`.
pub const MAX_COLUMNS: usize = 8;

/// One display column of a picker page (contract 0.5.0 `LookupPage.columns`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupColumn {
    /// The column's identifier; the key of a row's `cells`.
    pub name: String,
    /// The column heading.
    pub label: String,
}

/// A page of candidates (the contract's `LookupPage`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LookupPage {
    /// The rows of this page.
    pub rows: Vec<LookupRow>,
    /// Whether another page follows.
    pub has_more: bool,
    /// The cursor to pass for the next page; `Some` exactly when `has_more` is true.
    pub cursor: Option<String>,
    /// The display columns, in order (contract 0.5.0); at most [`MAX_COLUMNS`]. Empty means
    /// the single-column picker of `label` and `description`.
    pub columns: Vec<LookupColumn>,
}
