//! The record body and its digest: the ADR-055 value encoding, applied to what a row holds.
//!
//! `spot-journal-audit-run`'s in-memory sides digest a record as the canonical JSON of its image
//! body (`spot_journal_audit_run::memory::body_digest`: `serde_json`'s sorted-key text of
//! `Value::Object(body)`). A database side has no image to hand and must build the same body from
//! the rows, so this module is the one place that decides how a stored value is written as JSON,
//! and every rule below matches the native API's encoding (`spot-http`'s `codec`, `spot-activity`'s
//! `wire`) so a record and an API response never disagree about how a value is written (ADR-056):
//!
//! | declared type | body JSON |
//! | --- | --- |
//! | `Bool` | `true` / `false` |
//! | `Int` | a JSON number |
//! | `Decimal` | a JSON string with its scale kept (`"1234.50"`) |
//! | `Text` | a JSON string |
//! | `Date` | `"YYYY-MM-DD"` |
//! | `Time` | `"HH:MM:SS[.f]"`, fraction trimmed of trailing zeros |
//! | `DateTime` | RFC 3339 in UTC ending in `Z`, microseconds trimmed of trailing zeros |
//! | `Link` | the record id, canonical lower-case hyphenated |
//! | `Json` | the embedded document, never stringified |
//! | `Bytes` | padded standard base64 |
//! | `Geo` | `POINT(x y)` (`ST_AsText`) |
//! | `Range` | PostgreSQL's range text (`[1,5)`) |
//!
//! A null column is `null` for every type, and a document body carries **every** declared field
//! (`null` when unset) and **every** declared child table (an empty array when it has no rows),
//! exactly as `spot-document`'s `to_storage` writes a document. A child row is an object holding
//! its `id`, its 1-based `idx` and its declared fields.

use serde_json::{Map, Value as Json};
use spot_journal_audit::content_digest;
use spot_storage_contract::FieldType;

use crate::error::{PgSideError, image_invalid};

/// The digest of a record body: the audit digest of its canonical JSON text, which is what the
/// in-memory side computes for the same image (`memory::body_digest`).
///
/// # Errors
///
/// [`PgSideError`] with [`IMAGE_INVALID`](crate::error::IMAGE_INVALID) when the body cannot be
/// serialised, which a body of JSON values cannot do.
pub fn body_digest(body: &Map<String, Json>) -> Result<[u8; 32], PgSideError> {
    let text = serde_json::to_string(body)
        .map_err(|_| image_invalid("a record body could not be written as canonical JSON"))?;
    Ok(content_digest(text.as_bytes()))
}

/// One stored column as the body JSON of its declared type. `text` is the column's PostgreSQL
/// text form (`NULL` stays `None`), which the select list of [`crate::sql`] produces.
pub(crate) fn scalar(ty: &FieldType, text: Option<&str>) -> Result<Json, PgSideError> {
    let Some(text) = text else {
        return Ok(Json::Null);
    };
    Ok(match ty {
        // PostgreSQL spells a boolean's text form `true`/`false` (`t`/`f` is psql's display);
        // both are accepted, and the write side binds `t`/`f`.
        FieldType::Bool => match text {
            "t" | "true" => Json::Bool(true),
            "f" | "false" => Json::Bool(false),
            _ => return Err(image_invalid("a boolean column is neither true nor false")),
        },
        FieldType::Int => Json::from(
            text.parse::<i64>()
                .map_err(|_| image_invalid("an integer column is not an integer"))?,
        ),
        FieldType::Json => {
            serde_json::from_str(text).map_err(|_| image_invalid("a json column is not JSON"))?
        }
        // Everything else is its text form, which the select list already wrote in the ADR-055
        // shape: decimals with their scale, dates, times, instants in UTC, ids, base64 bytes,
        // points and ranges.
        FieldType::Decimal { .. }
        | FieldType::Text { .. }
        | FieldType::Date
        | FieldType::Time
        | FieldType::DateTime
        | FieldType::Link { .. }
        | FieldType::Bytes
        | FieldType::Geo { .. }
        | FieldType::Range { .. } => Json::String(text.to_owned()),
    })
}

/// A child row as the body holds it: its id, its 1-based position in the table and its declared
/// fields.
pub(crate) fn child_row(
    id: &str,
    idx: i64,
    fields: impl IntoIterator<Item = (String, Json)>,
) -> Json {
    let mut row = Map::new();
    row.insert("id".to_owned(), Json::String(id.to_owned()));
    row.insert("idx".to_owned(), Json::from(idx));
    for (name, value) in fields {
        row.insert(name, value);
    }
    Json::Object(row)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spot_storage_contract::FieldType;

    use super::*;

    #[test]
    fn a_body_digests_as_the_in_memory_side_digests_it() {
        // The rule of `spot_journal_audit_run::memory::body_digest`: the audit digest of the
        // sorted-key text of the body object.
        let body = json!({"b": 1, "a": "x"});
        let object = body.as_object().cloned().unwrap_or_default();
        let want = content_digest(br#"{"a":"x","b":1}"#);
        assert_eq!(body_digest(&object), Ok(want));
    }

    #[test]
    fn scalars_are_written_in_the_adr_055_shape() {
        let text = FieldType::Text { max_chars: None };
        assert_eq!(scalar(&text, None), Ok(Json::Null));
        assert_eq!(scalar(&text, Some("hi")), Ok(json!("hi")));
        assert_eq!(scalar(&FieldType::Bool, Some("t")), Ok(json!(true)));
        assert_eq!(scalar(&FieldType::Bool, Some("false")), Ok(json!(false)));
        assert_eq!(scalar(&FieldType::Int, Some("42")), Ok(json!(42)));
        assert_eq!(
            scalar(
                &FieldType::Decimal {
                    precision: 18,
                    scale: 2
                },
                Some("1234.50")
            ),
            Ok(json!("1234.50"))
        );
        assert_eq!(
            scalar(&FieldType::Json, Some(r#"{"k":[1]}"#)),
            Ok(json!({"k": [1]}))
        );
        assert!(scalar(&FieldType::Bool, Some("yes")).is_err());
        assert!(scalar(&FieldType::Int, Some("4x")).is_err());
    }
}
