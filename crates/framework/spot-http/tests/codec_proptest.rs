//! The codec's round-trip property (brief WP-PLT-05.T3, suite "Codec"): decode after encode
//! is the identity for every field type, and the Values-table rules hold over HTTP — a JSON
//! number for a decimal, an exponent, a `+`, offset datetimes, unknown fields listing every
//! offender.

#[allow(dead_code)]
mod support;

use serde_json::Value as Json;
use spot_http::api::codec::{MetaLookup, WriteMode, decode_body, encode_document};
use spot_http::api::meta::DocTypeMeta;
use support::Must;
use support::fake::TOKEN_ALICE;
use support::harness::{self, App};

use proptest::prelude::*;

/// The metas and lookup shared by the in-process properties.
fn metas() -> (
    DocTypeMeta,
    DocTypeMeta,
    std::collections::BTreeMap<String, DocTypeMeta>,
) {
    let app = support::harness::App::new();
    let note = app.fake.meta_of("doc_note");
    let invoice = app.fake.meta_of("sales_invoice");
    let lookup = std::collections::BTreeMap::from([
        ("doc_note".to_owned(), note.clone()),
        ("sales_invoice".to_owned(), invoice.clone()),
        (
            "sales_invoice_item".to_owned(),
            app.fake.meta_of("sales_invoice_item"),
        ),
    ]);
    (note, invoice, lookup)
}

/// The core property: `decode(encode(decode(body))) == decode(body)`, value for value.
fn round_trip(
    body: Json,
    meta: &DocTypeMeta,
    lookup: &std::collections::BTreeMap<String, DocTypeMeta>,
) {
    let decoded = decode_body(
        &body,
        meta,
        WriteMode::Create,
        None,
        MetaLookup::over(lookup),
    )
    .unwrap_or_else(|error| panic!("decode failed for {body}: {error:?}"));
    // A create request's id-less child rows get ids when the engine saves them; every wire
    // representation of a document carries those ids. Model that save step (the engines do
    // exactly this): assign each id-less row a server id, then encode the saved shape.
    let mut saved = decoded.clone();
    for value in saved.fields.values_mut() {
        if let spot_http::api::codec::FieldValue::Rows(rows) = value {
            for (row_index, row) in rows.iter_mut().enumerate() {
                if row.id.is_none() {
                    row.id = Some(
                        spot_http::api::codec::parse_record_id(UUIDS[row_index % UUIDS.len()])
                            .must(),
                    );
                }
            }
        }
    }
    let encoded = encode_document(&saved, meta, None, MetaLookup::over(lookup))
        .unwrap_or_else(|error| panic!("encode failed for {saved:?}: {error:?}"));
    // The response shape carries the system keys `id`/`version`/`_state` and each row's
    // `id`/`idx`; those are the response envelope, not values, and the request codec rightly
    // rejects them (ADR-055: `_`-keys and `version` are server metadata, `idx` is positional).
    // Strip them before re-decoding: the property is about the field-value encoding.
    let again_input = strip_system_keys(encoded, meta);
    let again = decode_body(
        &again_input,
        meta,
        WriteMode::Create,
        None,
        MetaLookup::over(lookup),
    )
    .unwrap_or_else(|error| panic!("re-decode failed for {again_input}: {error:?}"));
    assert_eq!(
        again, decoded,
        "decode after encode must be the identity\n input {body}\n  wire {again_input}"
    );
}

/// Removes the response-side system keys: `id`/`version`/`_state` at the top, and each child
/// row's `id`/`idx`.
fn strip_system_keys(encoded: Json, meta: &DocTypeMeta) -> Json {
    let mut object = match encoded {
        Json::Object(map) => map,
        other => return other,
    };
    for key in ["id", "version", "_state"] {
        object.remove(key);
    }
    for field in &meta.fields {
        if field.child_doctype.is_some()
            && let Some(Json::Array(rows)) = object.get_mut(field.name.as_str())
        {
            for row in rows.iter_mut() {
                if let Json::Object(row_map) = row {
                    for key in ["id", "idx"] {
                        row_map.remove(key);
                    }
                }
            }
        }
    }
    Json::Object(object)
}

/// A decimal string within scale 2 (the invoice row fields).
fn decimal2_strategy() -> impl Strategy<Value = String> {
    decimal_string_strategy().prop_filter("scale within 2", |text| match text.split_once('.') {
        Some((_, frac)) => frac.len() <= 2,
        None => true,
    })
}

/// A decimal string within scale 4, with a fractional part: mantissa × 10^-scale digits.
fn decimal_string_strategy() -> impl Strategy<Value = String> {
    (any::<i64>(), 1usize..=4)
        .prop_map(|(mantissa, frac_len)| {
            let negative = mantissa < 0;
            let digits = mantissa.unsigned_abs().to_string();
            let (int_part, frac_part) = if digits.len() > frac_len {
                let split = digits.len() - frac_len;
                (digits[..split].to_owned(), digits[split..].to_owned())
            } else {
                ("0".to_owned(), format!("{digits:0>frac_len$}"))
            };
            if negative {
                format!("-{int_part}.{frac_part}")
            } else {
                format!("{int_part}.{frac_part}")
            }
        })
        .prop_filter("decimal strings keep their scale", |text| {
            // `Decimal` drops trailing zeros in its own string form, so a generated value
            // whose frac is all zeros would not round-trip byte for byte; the identity
            // property still holds by value. Keep generated values with a nonzero tail.
            text.split_once('.')
                .is_none_or(|(_, frac)| frac.bytes().any(|b| b != b'0'))
        })
}

const DATES: [&str; 5] = [
    "2026-09-29",
    "1970-01-01",
    "2000-02-29",
    "2100-12-31",
    "1999-06-15",
];

const TIMES: [&str; 5] = [
    "00:00:00",
    "13:45:06",
    "23:59:59",
    "01:02:03.5",
    "09:07:05.123456",
];

const INSTANTS: [&str; 4] = [
    "2026-09-29T10:00:00",
    "2026-01-01T00:30:00.5",
    "1970-01-01T00:00:00",
    "2100-06-15T23:59:59.999999",
];

const OFFSETS: [&str; 6] = ["Z", "+05:00", "-08:00", "+00:30", "-11:30", "+13:45"];

const UUIDS: [&str; 3] = [
    "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f",
    "018f11ab-2cd3-7e5f-9a7b-8c9d0e1f2a3b",
    "0199aa11-bb22-7c99-8e00-00000000ffff",
];

const BASE64S: [&str; 6] = ["", "aGk=", "dGVzdA==", "/w==", "AAECAwQFBgcICQ==", "7+/v"];

/// Embedded JSON: a leaf or one level of array/object.
fn embedded_json() -> impl Strategy<Value = Json> {
    prop_oneof![
        Just(Json::Null),
        any::<bool>().prop_map(Json::Bool),
        (0i64..=9).prop_map(Json::from),
        "[a-z ]{0,10}".prop_map(Json::String),
        prop::collection::vec((0i64..=9).prop_map(Json::from), 0..=3).prop_map(Json::Array),
        prop::collection::vec("[a-z]{1,3}".prop_map(|key| (key, true)), 1..=2).prop_map(|pairs| {
            Json::Object(
                pairs
                    .into_iter()
                    .map(|(key, flag)| (key, Json::Bool(flag)))
                    .collect(),
            )
        }),
    ]
}

/// Every doc_note field's value, generated within its encoding's rules.
fn note_values() -> impl Strategy<Value = Vec<(&'static str, Json)>> {
    (
        any::<String>(),
        any::<bool>(),
        any::<i64>(),
        decimal_string_strategy(),
        0usize..DATES.len(),
        0usize..TIMES.len(),
        (0usize..INSTANTS.len(), 0usize..OFFSETS.len()),
        embedded_json(),
        0usize..BASE64S.len(),
    )
        .prop_map(
            |(name, flag, count, price, due, logged, stamped, extra, content)| {
                vec![
                    ("name", Json::String(name)),
                    ("flag", Json::Bool(flag)),
                    ("count", Json::from(count)),
                    ("price", Json::String(price)),
                    ("due", Json::String(DATES[due].to_owned())),
                    ("logged", Json::String(TIMES[logged].to_owned())),
                    (
                        "stamped",
                        Json::String(format!("{}{}", INSTANTS[stamped.0], OFFSETS[stamped.1])),
                    ),
                    ("extra", extra),
                    ("content", Json::String(BASE64S[content].to_owned())),
                ]
            },
        )
}

/// The document body for a `doc_note` create with the generated values.
fn note_body(values: &[(&'static str, Json)]) -> Json {
    values.iter().fold(
        Json::Object(serde_json::Map::new()),
        |mut body: Json, (name, value)| {
            body.as_object_mut()
                .must()
                .insert((*name).to_owned(), value.clone());
            body
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The core property over every scalar kind: decode after encode is the identity.
    #[test]
    fn decode_encode_is_the_identity_for_every_scalar_field(values in note_values()) {
        let (note, _invoice, lookup) = metas();
        round_trip(note_body(&values), &note, &lookup);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The same identity for a full invoice document: child rows included.
    #[test]
    fn decode_encode_is_the_identity_for_a_full_invoice_document(
        rows in prop::collection::vec(
            (0usize..UUIDS.len(), decimal2_strategy(), decimal2_strategy()),
            0..=4,
        ),
        name in "[A-Z]{1,6}",
        notes in prop::option::of("[a-z ]{0,20}"),
    ) {
        let (_note, invoice, lookup) = metas();
        let items: Vec<Json> = rows
            .into_iter()
            .map(|(item, qty, rate)| {
                serde_json::json!({ "item": UUIDS[item], "qty": qty, "rate": rate })
            })
            .collect();
        let mut body = serde_json::json!({ "name": name, "customer": UUIDS[0], "items": items });
        if let Some(notes) = notes {
            body["notes"] = Json::String(notes);
        }
        round_trip(body, &invoice, &lookup);
    }
}

/// The `i64` boundaries as JSON integers, stored and returned exactly.
#[test]
fn int_boundaries_round_trip_in_process() {
    let (note, _invoice, lookup) = metas();
    for count in [i64::MAX, i64::MIN, 0, -1] {
        let body = serde_json::json!({ "name": "bound", "count": count });
        let decoded = decode_body(
            &body,
            &note,
            WriteMode::Create,
            None,
            MetaLookup::over(&lookup),
        )
        .must();
        let encoded = encode_document(&decoded, &note, None, MetaLookup::over(&lookup)).must();
        assert_eq!(
            encoded["count"],
            Json::from(count),
            "the 64-bit boundary is exact"
        );
        round_trip(body, &note, &lookup);
    }
}

#[tokio::test]
async fn values_through_http_follow_the_table() {
    let app = App::new();
    let alice = TOKEN_ALICE;

    // A create with every field kind; the response carries the Values-table encodings.
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({
            "name": "all kinds",
            "flag": true,
            "count": -9_007_199_254_740_993_i64,
            "price": "12345.6789",
            "due": "2026-09-29",
            "logged": "01:02:03.5",
            "stamped": "2026-09-29T10:00:00+05:00",
            "extra": { "a": [1, "two", false] },
            "content": "aGk="
        }),
    )
    .await;
    call.expect_status(201, "create with every kind");
    let data = call.body["data"].clone();
    assert!(data["flag"].as_bool().must(), "bool true stays true");
    assert_eq!(data["count"].as_i64().must(), -9_007_199_254_740_993);
    assert_eq!(data["price"].as_str().must(), "12345.6789", "scale kept");
    assert_eq!(data["due"].as_str().must(), "2026-09-29");
    assert_eq!(data["logged"].as_str().must(), "01:02:03.5");
    assert_eq!(
        data["stamped"].as_str().must(),
        "2026-09-29T05:00:00Z",
        "the offset instant is normalised to UTC"
    );
    assert_eq!(
        data["extra"],
        serde_json::json!({ "a": [1, "two", false] }),
        "embedded as JSON"
    );
    assert_eq!(
        data["content"].as_str().must(),
        "aGk=",
        "bytes are base64 with padding"
    );

    // The same document reads back the same way.
    let probe = harness::get_doc(&app, alice, "doc_note", &call.doc_id()).await;
    probe.expect_status(200, "read back");
    assert_eq!(probe.body["data"]["extra"], data["extra"]);

    // A JSON number for a decimal field never passes (ADR-12): 422 document.invalid_value.
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({ "name": "x", "price": 10.5 }),
    )
    .await;
    call.expect_status(422, "a number for a decimal");
    assert_eq!(call.error_code(), "document.invalid_value");

    // Exponent and `+` syntax are outside the decimal pattern.
    for price in ["1e5", "+5.0", ".5", "1."] {
        let call = harness::create_doc(
            &app,
            alice,
            "doc_note",
            serde_json::json!({ "name": "x", "price": price }),
        )
        .await;
        call.expect_status(422, "bad decimal syntax");
        assert_eq!(call.error_code(), "document.invalid_value", "{price}");
    }

    // The scale of the field is a hard cap: values with more digits are rejected, never
    // rounded.
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({ "name": "x", "price": "1.23456" }),
    )
    .await;
    call.expect_status(422, "scale overflow is rejected, never rounded");
    assert_eq!(call.error_code(), "document.invalid_value");

    // Values that do not fit their types.
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({ "name": "x", "flag": "yes" }),
    )
    .await;
    call.expect_status(422, "string for a bool");
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({ "name": "x", "due": "29.09.2026" }),
    )
    .await;
    call.expect_status(422, "bad date");
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({ "name": "x", "logged": "25:00:00" }),
    )
    .await;
    call.expect_status(422, "hour 25");
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({ "name": "x", "stamped": "2026-09-29 10:00" }),
    )
    .await;
    call.expect_status(422, "not RFC 3339");
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({ "name": "x", "content": "!!!" }),
    )
    .await;
    call.expect_status(422, "not base64");

    // Unknown fields: every offender is listed in one error.
    let call = harness::create_doc(
        &app,
        alice,
        "doc_note",
        serde_json::json!({ "name": "x", "typo_a": 1, "typo_b": 2, "typo_c": 3 }),
    )
    .await;
    call.expect_status(422, "unknown fields");
    assert_eq!(call.error_code(), "document.unknown_field");
    let mut paths = call.field_paths();
    paths.sort();
    assert_eq!(
        paths,
        vec!["typo_a", "typo_b", "typo_c"],
        "every offender listed"
    );
}
