//! Field names on the notification channel (WP-SEC-04.T8, ADR-070 section 12 rule 6).
//!
//! `Channel::Notification` **never carries a value**: field names only, and a name the
//! recipient may not know exists is not a name it is told. A room event is one payload for every
//! member of the room, so it cannot be tailored to one recipient. Two rules follow:
//!
//! - [`broadcast_names`]: the names a *shared* payload may carry are those of fields that no
//!   protection group, classification tag or mask covers. A protected field's name goes to no
//!   room; a DocType or table the source does not know contributes nothing (fail closed). A
//!   client that wants to know more fetches the document through the normal read path
//!   (`GET /api/v1/resource/{doctype}/{id}`), where the field's own permissions apply.
//! - [`recipient_names`]: for a payload built for one recipient (mail, push, a per-user room)
//!   the names are asked of [`FieldGuard::visible_names`], so a field the recipient may not see
//!   is not named. A guard error names nothing.
//!
//! A user notice's `data` is app-supplied JSON; with the guard installed it is held to the same
//! rule by [`notice_names_only`]: no string value may be anything but a record id, a DocType
//! the source knows, or the name of an unprotected field, so a text template that interpolated a
//! protected field's value is refused rather than sent.

use serde_json::Value as Json;
use spot_field_guard::{DocProtection, FieldGuard, ProtectionSource};
use spot_permissions::{DocFacts, Principal};

/// The keys under which a notice may list field names.
const NAME_KEYS: [&str; 3] = ["field", "fields", "changed"];

/// Whether `path` (`field`, or `table.column`) names a field nothing protects.
fn unprotected(prot: &DocProtection, source: &dyn ProtectionSource, path: &str) -> bool {
    let (facts, plain) = match path.split_once('.') {
        None => (prot.field(path), true),
        Some((table, column)) => {
            let Some(child) = prot.child_doctype(table) else {
                return false;
            };
            let Some(child_prot) = source.protection(child) else {
                return false;
            };
            // The table itself must be unprotected too.
            let table_facts = prot.field(table);
            if !table_facts.groups.is_empty()
                || !table_facts.tags.is_empty()
                || table_facts.mask.is_some()
            {
                return false;
            }
            (child_prot.field(column), true)
        }
    };
    plain && facts.groups.is_empty() && facts.tags.is_empty() && facts.mask.is_none()
}

/// The names of `names` a payload shared by a whole room may carry.
#[must_use]
pub fn broadcast_names(
    source: &dyn ProtectionSource,
    doctype: &str,
    names: &[String],
) -> Vec<String> {
    let Some(prot) = source.protection(doctype) else {
        return Vec::new();
    };
    names
        .iter()
        .filter(|name| unprotected(&prot, source, name))
        .cloned()
        .collect()
}

/// The names of `names` a payload built for `principal` may carry. A guard error (an unknown
/// DocType) names nothing.
#[must_use]
pub fn recipient_names(
    guard: &FieldGuard,
    principal: &Principal,
    doctype: &str,
    facts: &DocFacts,
    names: &[String],
) -> Vec<String> {
    guard
        .visible_names(principal, doctype, facts, names)
        .unwrap_or_default()
}

/// Whether a notice's `data` holds no value: every string is a record id, a DocType the source
/// knows (under `doctype`), or the name of an unprotected field of that DocType (under `field`,
/// `fields` or `changed`). Numbers, booleans and nulls carry no field value of a protected
/// field the pump can tell apart and are kept.
#[must_use]
pub(crate) fn notice_names_only(
    body: &Json,
    source: &dyn ProtectionSource,
    is_record_id: fn(&str) -> bool,
) -> bool {
    let Some(map) = body.as_object() else {
        return false;
    };
    let doctype = map.get("doctype").and_then(Json::as_str);
    let known = doctype.and_then(|d| source.protection(d));
    let string_ok = |key: &str, text: &str| -> bool {
        if is_record_id(text) {
            return true;
        }
        if key == "doctype" {
            return known.is_some();
        }
        NAME_KEYS.contains(&key)
            && known
                .as_ref()
                .is_some_and(|prot| unprotected(prot, source, text))
    };
    map.iter().all(|(key, value)| match value {
        Json::String(text) => string_ok(key, text),
        Json::Array(items) => items.iter().all(|item| match item {
            Json::String(text) => string_ok(key, text),
            Json::Array(_) | Json::Object(_) => false,
            _ => true,
        }),
        Json::Object(_) => false,
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spot_field_guard::StaticProtection;
    use spot_permissions::{FieldFacts, GroupName};

    fn source() -> StaticProtection {
        let mut salary = FieldFacts::plain("salary");
        salary.groups = vec![GroupName("hr".to_owned())];
        StaticProtection::new(vec![DocProtection {
            name: "employee".to_owned(),
            fields: vec![salary],
            tables: Default::default(),
            trace_groups: Vec::new(),
        }])
    }

    fn is_id(text: &str) -> bool {
        text.len() == 36
    }

    #[test]
    fn a_protected_name_is_never_broadcast() {
        let names = vec!["title".to_owned(), "salary".to_owned()];
        assert_eq!(broadcast_names(&source(), "employee", &names), ["title"]);
        assert!(broadcast_names(&source(), "unknown_doctype", &names).is_empty());
    }

    #[test]
    fn a_notice_with_a_value_is_refused() {
        let s = source();
        let ok = serde_json::json!({"doctype": "employee", "fields": ["title"], "n": 3});
        assert!(notice_names_only(&ok, &s, is_id));
        let bad = serde_json::json!({"doctype": "employee", "text": "Salary is 5000"});
        assert!(!notice_names_only(&bad, &s, is_id));
        let protected = serde_json::json!({"doctype": "employee", "fields": ["salary"]});
        assert!(!notice_names_only(&protected, &s, is_id));
    }
}
