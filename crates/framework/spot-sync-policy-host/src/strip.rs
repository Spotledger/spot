//! Stripping excluded fields from an event payload on the way **up** (node to hub), by name at
//! every level (ADR-063 section 1, `excluded_fields`; WP-RTB-02.T10).
//!
//! `spot_sync_hub_adapters::PolicyRedactor` strips an image on the way **down** (hub to node); the
//! node's own outbox push needs the same rule on the way up, or an excluded field would leave the
//! node in the payload of an event. The rule is the policy's own
//! ([`SyncPolicy::excludes`](spot_sync_policy::SyncPolicy::excludes), by name, at every level):
//! the node and the hub agree on what is excluded because both ask the same function.
//!
//! The strip is **deep**: an excluded name is removed wherever it appears in the JSON value
//! (parent fields, child tables as a whole, fields inside child rows, nested objects). That
//! is stricter than the one-level image redactor and cannot leak a field the redactor would keep.

use serde_json::Value;
use spot_sync_policy::SyncPolicy;

/// Removes every excluded name from `value`, at every depth. Returns how many entries were
/// removed (never their values).
pub fn strip_value(value: &mut Value, policy: &SyncPolicy) -> usize {
    match value {
        Value::Object(map) => {
            let before = map.len();
            map.retain(|name, _| !policy.excludes(name));
            let mut removed = before - map.len();
            for inner in map.values_mut() {
                removed += strip_value(inner, policy);
            }
            removed
        }
        Value::Array(items) => items.iter_mut().map(|item| strip_value(item, policy)).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeSet;

    use serde_json::json;
    use spot_sync_policy::{Class, Direction, FieldName, ScopeKeyType, SyncPolicy, Trigger};

    use super::strip_value;

    fn policy(excluded: &[&str]) -> SyncPolicy {
        let exclude: BTreeSet<FieldName> = excluded
            .iter()
            .map(|name| FieldName::new(name).unwrap())
            .collect();
        SyncPolicy::new(
            Class::Transactional,
            Some(ScopeKeyType::new("warehouse").unwrap()),
            Direction::Up,
            Trigger::OnSubmit,
            exclude,
        )
    }

    #[test]
    fn an_excluded_name_is_removed_at_every_depth() {
        let mut value = json!({
            "title": "t",
            "secret_note": "never",
            "items": [{"id": "1", "secret_note": "never", "qty": 2}],
            "nested": {"deep": {"secret_note": "never", "keep": 1}},
        });
        let removed = strip_value(&mut value, &policy(&["secret_note"]));
        assert_eq!(removed, 3);
        assert_eq!(
            value,
            json!({
                "title": "t",
                "items": [{"id": "1", "qty": 2}],
                "nested": {"deep": {"keep": 1}},
            })
        );
    }

    #[test]
    fn an_excluded_child_table_goes_as_a_whole_and_nothing_else_changes() {
        let mut value = json!({"title": "t", "lines": [{"id": "1"}]});
        assert_eq!(strip_value(&mut value, &policy(&["lines"])), 1);
        assert_eq!(value, json!({"title": "t"}));
        let mut same = json!({"title": "t"});
        assert_eq!(strip_value(&mut same, &policy(&[])), 0);
        assert_eq!(same, json!({"title": "t"}));
    }
}
