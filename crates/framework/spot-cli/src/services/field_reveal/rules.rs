//! The pure rules of a reveal: what can be revealed at all, and how a stored value is encoded
//! on the wire. No storage, no permission engine, no clock: everything here is testable alone.

use serde_json::Value as Json;
use spot_permissions::FieldFacts;
use spot_storage_contract::Value;
use time::macros::format_description;

/// The stored prefix of a sealed field (a box to the hub's key). The node cannot open it.
pub(super) const SEALED_PREFIX: &str = "$sealed:v1:";
/// The stored prefix of an encrypted field (group-key ciphertext).
pub(super) const ENCRYPTED_PREFIX: &str = "$enc:v1:";

/// The group word recorded for a field that is in no protection group.
pub(super) const NO_GROUP: &str = "none";

/// Whether `field` names one top-level field: a storage name, no path into a child table.
pub(super) fn is_simple_field(field: &str) -> bool {
    let mut chars = field.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && field.len() <= 63
}

/// Whether a field has anything to reveal: it is in a protection group or it has a mask. A plain
/// field has nothing to reveal and is refused like any other field that cannot be revealed.
pub(super) fn is_revealable(facts: &FieldFacts) -> bool {
    !facts.groups.is_empty() || facts.mask.is_some()
}

/// The group recorded for `facts`: its first group, or [`NO_GROUP`].
pub(super) fn group_of(facts: Option<&FieldFacts>) -> String {
    facts
        .and_then(|f| f.groups.first())
        .map_or_else(|| NO_GROUP.to_owned(), |g| g.as_str().to_owned())
}

/// Whether a stored text is a sealed or encrypted envelope (not a value).
pub(super) fn is_envelope(text: &str) -> bool {
    text.starts_with(SEALED_PREFIX) || text.starts_with(ENCRYPTED_PREFIX)
}

/// A stored value in the native API encoding (the Values table of the contract: decimals as
/// strings, instants in UTC). `None` for a value with no wire form here: the reveal answers
/// `server.unavailable` rather than guess.
pub(super) fn encode(value: &Value) -> Option<Json> {
    Some(match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Date(d) => Json::String(
            d.format(&format_description!("[year]-[month]-[day]"))
                .ok()?,
        ),
        Value::DateTime(at) => Json::String(spot_http::api::codec::encode_datetime(at)),
        Value::Time(t) => Json::String(spot_http::api::codec::encode_time(t).ok()?),
        Value::Json(text) => serde_json::from_str(text).ok()?,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_permissions::MaskFormat;

    use super::*;

    #[test]
    fn only_a_top_level_storage_name_is_a_simple_field() {
        for good in ["details", "phone_2", "a"] {
            assert!(is_simple_field(good), "{good}");
        }
        for bad in [
            "",
            "Details",
            "2fa",
            "rows[0].x",
            "items.qty",
            "a-b",
            &"a".repeat(64),
        ] {
            assert!(!is_simple_field(bad), "{bad}");
        }
    }

    #[test]
    fn a_plain_field_has_nothing_to_reveal() {
        assert!(!is_revealable(&FieldFacts::plain("subject")));
        let masked = FieldFacts {
            mask: Some(MaskFormat::Placeholder),
            ..FieldFacts::plain("phone")
        };
        assert!(is_revealable(&masked));
        assert_eq!(group_of(Some(&FieldFacts::plain("subject"))), NO_GROUP);
        assert_eq!(group_of(None), NO_GROUP);
    }

    #[test]
    fn a_sealed_or_encrypted_text_is_an_envelope_not_a_value() {
        assert!(is_envelope("$sealed:v1:AAAA"));
        assert!(is_envelope("$enc:v1:AAAA"));
        assert!(!is_envelope("0300-1234567"));
        assert!(!is_envelope("sealed:v1:AAAA"));
    }

    #[test]
    fn values_are_encoded_as_the_contract_says() {
        assert_eq!(
            encode(&Value::Text("0300-1234567".into())),
            Some(Json::String("0300-1234567".into()))
        );
        assert_eq!(encode(&Value::Int(7)), Some(Json::from(7)));
        assert_eq!(encode(&Value::Bool(true)), Some(Json::Bool(true)));
        assert_eq!(encode(&Value::Null), Some(Json::Null));
        assert_eq!(
            encode(&Value::Json("{\"a\":1}".into())),
            Some(serde_json::json!({"a": 1}))
        );
        assert_eq!(encode(&Value::Json("not json".into())), None);
    }
}
