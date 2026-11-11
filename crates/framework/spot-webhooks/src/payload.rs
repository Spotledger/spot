//! Payload building: the stable JSON a delivery carries, per mode and per the owner's rights.
//!
//! The envelope always carries `event_id`, `event_type`, `doctype`, `document_id`,
//! `document_version` and `timestamp`. The document data follows the mode: `ids` carries none,
//! `fields` the allow-listed fields, `full` every field. A field the owner may not read is never
//! included ([`FieldVisibility`]). When the result exceeds the size cap it falls back to the
//! `ids` shape with `"truncated": true`.

use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::MAX_PAYLOAD_SIZE_BYTES;
use crate::model::{EventType, PayloadMode};
use crate::permissions::FieldVisibility;

/// Builds delivery payloads.
#[derive(Debug, Clone)]
pub struct PayloadBuilder {
    mode: PayloadMode,
    field_allowlist: std::collections::BTreeSet<String>,
    max_size: usize,
}

impl PayloadBuilder {
    /// A builder for `mode`; `field_allowlist` is used by `fields` mode.
    pub fn new(mode: PayloadMode, field_allowlist: Vec<String>, max_size: usize) -> Self {
        Self {
            mode,
            field_allowlist: field_allowlist.into_iter().collect(),
            max_size,
        }
    }

    /// Builds the payload for one event.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &self,
        event_id: &str,
        event_type: EventType,
        doctype: &str,
        document_id: &str,
        document_version: u64,
        occurred_at: OffsetDateTime,
        after_image: &Value,
        before_image: Option<&Value>,
        visibility: &dyn FieldVisibility,
    ) -> Value {
        let timestamp = occurred_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| occurred_at.unix_timestamp().to_string());

        let mut payload = Map::new();
        payload.insert("event_id".into(), Value::String(event_id.to_owned()));
        payload.insert(
            "event_type".into(),
            Value::String(event_type.as_str().to_owned()),
        );
        payload.insert("doctype".into(), Value::String(doctype.to_owned()));
        payload.insert("document_id".into(), Value::String(document_id.to_owned()));
        payload.insert(
            "document_version".into(),
            Value::Number(document_version.into()),
        );
        payload.insert("timestamp".into(), Value::String(timestamp.clone()));

        match self.mode {
            PayloadMode::Ids => {}
            PayloadMode::Fields => {
                payload.insert(
                    "data".into(),
                    self.project(after_image, doctype, visibility),
                );
            }
            PayloadMode::Full => {
                payload.insert(
                    "data".into(),
                    self.project(after_image, doctype, visibility),
                );
            }
        }
        if self.mode != PayloadMode::Ids
            && let Some(before) = before_image
        {
            payload.insert("before".into(), self.project(before, doctype, visibility));
        }

        let value = Value::Object(payload);
        let size = serde_json::to_vec(&value).map_or(0, |bytes| bytes.len());
        if size > self.max_size {
            return self.truncated(
                event_id,
                event_type,
                doctype,
                document_id,
                document_version,
                &timestamp,
            );
        }
        value
    }

    /// The allow-listed, readable fields of one image, keeping the value encoding (decimals are
    /// already strings on the wire, so nothing is converted here).
    fn project(&self, image: &Value, doctype: &str, visibility: &dyn FieldVisibility) -> Value {
        let mut out = Map::new();
        if let Value::Object(map) = image {
            for (key, value) in map {
                if self.mode == PayloadMode::Fields && !self.field_allowlist.contains(key) {
                    continue;
                }
                if !visibility.may_read(doctype, key) {
                    continue;
                }
                out.insert(key.clone(), value.clone());
            }
        }
        Value::Object(out)
    }

    /// The `ids`-mode payload with `"truncated": true`.
    fn truncated(
        &self,
        event_id: &str,
        event_type: EventType,
        doctype: &str,
        document_id: &str,
        document_version: u64,
        timestamp: &str,
    ) -> Value {
        serde_json::json!({
            "event_id": event_id,
            "event_type": event_type.as_str(),
            "doctype": doctype,
            "document_id": document_id,
            "document_version": document_version,
            "timestamp": timestamp,
            "truncated": true,
        })
    }
}

/// Builds a payload with the crate's default size cap.
#[allow(clippy::too_many_arguments)]
pub fn build_payload(
    mode: PayloadMode,
    field_allowlist: Vec<String>,
    event_id: &str,
    event_type: EventType,
    doctype: &str,
    document_id: &str,
    document_version: u64,
    occurred_at: OffsetDateTime,
    after_image: &Value,
    before_image: Option<&Value>,
    visibility: &dyn FieldVisibility,
) -> Value {
    PayloadBuilder::new(mode, field_allowlist, MAX_PAYLOAD_SIZE_BYTES).build(
        event_id,
        event_type,
        doctype,
        document_id,
        document_version,
        occurred_at,
        after_image,
        before_image,
        visibility,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::permissions::AllFieldsReadable;
    use serde_json::json;
    use std::collections::BTreeSet;

    struct DenySalary;

    impl FieldVisibility for DenySalary {
        fn may_read(&self, _doctype: &str, field: &str) -> bool {
            field != "salary"
        }
    }

    fn at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn build(
        mode: PayloadMode,
        allow: Vec<&str>,
        after: &Value,
        visibility: &dyn FieldVisibility,
    ) -> Value {
        PayloadBuilder::new(
            mode,
            allow.into_iter().map(str::to_owned).collect(),
            MAX_PAYLOAD_SIZE_BYTES,
        )
        .build(
            "e1",
            EventType::Update,
            "task",
            "d1",
            7,
            at(),
            after,
            None,
            visibility,
        )
    }

    #[test]
    fn ids_mode_carries_no_data() {
        let payload = build(
            PayloadMode::Ids,
            vec![],
            &json!({"status": "Done"}),
            &AllFieldsReadable,
        );
        assert_eq!(payload["event_id"], "e1");
        assert_eq!(payload["document_version"], 7);
        assert!(payload.get("data").is_none());
        assert!(
            payload["timestamp"]
                .as_str()
                .unwrap()
                .starts_with("2023-11-14")
        );
    }

    #[test]
    fn fields_mode_keeps_only_the_allowlist() {
        let after = json!({"status": "Done", "salary": "10.00", "note": "x"});
        let payload = build(
            PayloadMode::Fields,
            vec!["status", "salary"],
            &after,
            &AllFieldsReadable,
        );
        assert_eq!(payload["data"]["status"], "Done");
        assert_eq!(payload["data"]["salary"], "10.00");
        assert!(payload["data"].get("note").is_none());
    }

    #[test]
    fn full_mode_omits_unreadable_fields() {
        let after = json!({"status": "Done", "salary": "10.00"});
        let payload = build(PayloadMode::Full, vec![], &after, &DenySalary);
        assert_eq!(payload["data"]["status"], "Done");
        assert!(payload["data"].get("salary").is_none());
    }

    #[test]
    fn decimals_stay_strings_and_integers_stay_numbers() {
        let after = json!({"amount": "1234.50", "qty": 3});
        let payload = build(PayloadMode::Full, vec![], &after, &AllFieldsReadable);
        assert_eq!(payload["data"]["amount"], "1234.50");
        assert_eq!(payload["data"]["qty"], 3);
    }

    #[test]
    fn oversize_payload_falls_back_to_ids_with_truncated() {
        let mut big = Map::new();
        for i in 0..5000 {
            big.insert(format!("field_{i}"), Value::String("x".repeat(50)));
        }
        let after = Value::Object(big);
        let payload = PayloadBuilder::new(PayloadMode::Full, vec![], 1000).build(
            "e1",
            EventType::Update,
            "task",
            "d1",
            1,
            at(),
            &after,
            None,
            &AllFieldsReadable,
        );
        assert_eq!(payload["truncated"], true);
        assert!(payload.get("data").is_none());
    }

    #[test]
    fn before_image_is_projected_too() {
        let after = json!({"status": "Done", "salary": "10.00"});
        let before = json!({"status": "Open", "salary": "9.00"});
        let payload = PayloadBuilder::new(PayloadMode::Full, vec![], MAX_PAYLOAD_SIZE_BYTES).build(
            "e1",
            EventType::Update,
            "task",
            "d1",
            2,
            at(),
            &after,
            Some(&before),
            &DenySalary,
        );
        assert_eq!(payload["data"]["status"], "Done");
        assert_eq!(payload["before"]["status"], "Open");
        assert!(payload["before"].get("salary").is_none());
    }

    #[test]
    fn the_allowlist_set_is_case_sensitive() {
        let after = json!({"status": "Done"});
        let payload = build(
            PayloadMode::Fields,
            vec!["Status"],
            &after,
            &AllFieldsReadable,
        );
        let empty: BTreeSet<String> = payload["data"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert!(empty.is_empty());
    }
}
