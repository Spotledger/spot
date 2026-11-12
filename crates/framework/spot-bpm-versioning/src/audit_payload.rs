//! The `sys_bpm_case_migrated` payload (WP-CON-04.T8; events contract `system_platform.rs`).
//!
//! [`MigrationAudit::to_payload`] is the one place the audit becomes the contract's event data.
//! It is written from the contract's field list (`case`, `from`, `to`, `plan_digest`, `actor`
//! optional, `store_version`, `nodes_changed`, `variables_defaulted`, `at`), not from the struct's
//! field names:
//!
//! - `case` is the canonical text of the Case id;
//! - `from` and `to` are `{name, version, hash}` objects;
//! - the counts and `store_version` are integers; there is no float anywhere (ADR-12);
//! - `at` is **Unix microseconds** as an integer, as the contract states (the task brief said
//!   RFC 3339; the contract and its example file win, see the report);
//! - `actor` is absent when the migration has none (the schema makes it optional).
//!
//! [`MigrationAudit::to_outbox_json`] is the canonical text: keys sorted at every depth, no
//! whitespace, so the same audit always gives the same bytes whatever features `serde_json` was
//! built with.

use serde_json::{Map, Value as Json};
use spot_app_api::prelude::OffsetDateTime;

use crate::migration::MigrationAudit;
use crate::version::VersionRef;

fn pin_json(pin: &VersionRef) -> Json {
    let mut map = Map::new();
    map.insert("name".to_owned(), Json::from(pin.name.clone()));
    map.insert("version".to_owned(), Json::from(u64::from(pin.version)));
    map.insert("hash".to_owned(), Json::from(pin.hash.clone()));
    Json::Object(map)
}

/// Unix microseconds, rounded towards negative infinity. Every `OffsetDateTime` fits an `i64`
/// of microseconds, so the saturation is unreachable.
fn unix_micros(at: OffsetDateTime) -> i64 {
    let micros = at.unix_timestamp_nanos().div_euclid(1_000);
    i64::try_from(micros).unwrap_or(if micros < 0 { i64::MIN } else { i64::MAX })
}

fn count(n: usize) -> Json {
    Json::from(u64::try_from(n).unwrap_or(u64::MAX))
}

impl MigrationAudit {
    /// The `sys_bpm_case_migrated` event data the contract accepts.
    #[must_use]
    pub fn to_payload(&self) -> Json {
        let mut map = Map::new();
        map.insert("case".to_owned(), Json::from(self.case.to_string()));
        map.insert("from".to_owned(), pin_json(&self.from));
        map.insert("to".to_owned(), pin_json(&self.to));
        map.insert(
            "plan_digest".to_owned(),
            Json::from(self.plan_digest.clone()),
        );
        if let Some(actor) = &self.actor {
            map.insert("actor".to_owned(), Json::from(actor.clone()));
        }
        map.insert("store_version".to_owned(), Json::from(self.store_version));
        map.insert("nodes_changed".to_owned(), count(self.nodes_changed));
        map.insert(
            "variables_defaulted".to_owned(),
            count(self.variables_defaulted),
        );
        map.insert("at".to_owned(), Json::from(unix_micros(self.at)));
        Json::Object(map)
    }

    /// The canonical outbox text of [`to_payload`](Self::to_payload): sorted keys, compact.
    #[must_use]
    pub fn to_outbox_json(&self) -> String {
        let mut out = String::new();
        write_canonical(&self.to_payload(), &mut out);
        out
    }
}

fn write_canonical(value: &Json, out: &mut String) {
    match value {
        Json::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Json::String(key.clone()).to_string());
                out.push(':');
                if let Some(inner) = map.get(key) {
                    write_canonical(inner, out);
                }
            }
            out.push('}');
        }
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}
