//! Which DocTypes the core knows to be free of `secret` fields (feature `storage`).
//!
//! A write under `/api/v1/resource/{doctype}` carries field values in its body, and the path
//! cannot tell whether one of them is a secret. The core therefore learns the answer from the
//! `GET /api/v1/meta/{doctype}` replies that pass through it (no request of its own) and writes
//! such a call to storage only when the DocType is *safe*:
//!
//! * its latest meta reply was a 200 that parsed, named the same DocType as the path, and had no
//!   field of type `secret` (or `bytes`, see `classify`);
//! * for every `table` or `table_multiselect` field, the child DocType (`child_doctype`) is
//!   itself safe by the same rule. This is computed at decision time, so the order of the
//!   replies does not matter; a table field without a `child_doctype` makes the DocType unsafe.
//!
//! Everything else (never seen, did not parse, a `secret` field, a child not yet seen, a path
//! that cannot be read strictly) is not persisted: the call still runs from memory. A newer
//! reply replaces the older one, and a reply that does not parse forgets the DocType. The set is
//! cleared where the read cache is cleared (a different user, sign-out): metadata is per site
//! and per principal.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::persist_policy::{Target, has_query, may_persist, meta_doctype, target_of};

/// The most DocTypes remembered; a further new DocType is simply not learned (fail closed).
const MAX_DOCTYPES: usize = 2048;

#[derive(Debug, Clone)]
enum Seen {
    /// Has a `secret` field, or a table field that names no child DocType.
    Unsafe,
    /// No secret field of its own; these child DocTypes must be safe too.
    Plain { children: Vec<String> },
}

/// The DocTypes whose metadata the core has seen.
#[derive(Debug, Default)]
pub(crate) struct MetaSeen {
    seen: BTreeMap<String, Seen>,
}

fn classify(doctype: &str, body: &str) -> Option<Seen> {
    let value: Value = serde_json::from_str(body).ok()?;
    let data = value.get("data")?;
    if data.get("doctype")?.as_str()? != doctype {
        return None;
    }
    let mut children = Vec::new();
    let mut bad = false;
    for field in data.get("fields")?.as_array()? {
        match field.get("type")?.as_str()? {
            // The contract names the type `secret`, but the server's meta_get still sends a
            // secret field as `bytes` (spot-api-services meta_resources.rs maps
            // FieldKind::Secret to FieldType::Bytes). A bytes field is therefore treated as a
            // possible secret: fail closed until the server sends `secret`.
            "secret" | "bytes" => bad = true,
            "table" | "table_multiselect" => {
                match field.get("child_doctype").and_then(Value::as_str) {
                    Some(child) if !child.is_empty() => children.push(child.to_owned()),
                    _ => bad = true,
                }
            }
            _ => {}
        }
    }
    Some(if bad {
        Seen::Unsafe
    } else {
        Seen::Plain { children }
    })
}

impl MetaSeen {
    /// Forgets everything (a different user, sign-out).
    pub(crate) fn clear(&mut self) {
        self.seen.clear();
    }

    /// Learns from a finished `GET`. Only a 200 reply to an exact `/api/v1/meta/{doctype}`
    /// counts; anything else is ignored. A 200 that does not parse forgets the DocType.
    pub(crate) fn learn(&mut self, method: &str, path: &str, status: u16, body: &str) {
        if method != "GET" || status != 200 {
            return;
        }
        let Some(doctype) = meta_doctype(path) else {
            return;
        };
        match classify(&doctype, body) {
            Some(seen) => {
                if self.seen.len() < MAX_DOCTYPES || self.seen.contains_key(&doctype) {
                    self.seen.insert(doctype, seen);
                }
            }
            None => {
                self.seen.remove(&doctype);
            }
        }
    }

    /// True when the DocType and every child table reachable beneath it are known to hold no
    /// secret. Each DocType is visited once (a cycle or a diamond of child tables adds no new
    /// fact and costs nothing more), so the work is bounded by the number of DocTypes learned.
    pub(crate) fn is_safe(&self, doctype: &str) -> bool {
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut todo: Vec<&str> = vec![doctype];
        while let Some(name) = todo.pop() {
            if !visited.insert(name) {
                continue;
            }
            match self.seen.get(name) {
                Some(Seen::Plain { children }) => todo.extend(children.iter().map(String::as_str)),
                _ => return false,
            }
        }
        true
    }

    /// True when metadata seen so far shows a `secret` (or `bytes`) field in the DocType or in
    /// a child table reachable beneath it. A DocType not seen (or a child not seen) is not
    /// known to be unsafe.
    fn known_unsafe(&self, doctype: &str) -> bool {
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut todo: Vec<&str> = vec![doctype];
        while let Some(name) = todo.pop() {
            if !visited.insert(name) {
                continue;
            }
            match self.seen.get(name) {
                Some(Seen::Unsafe) => return true,
                Some(Seen::Plain { children }) => todo.extend(children.iter().map(String::as_str)),
                None => {}
            }
        }
        false
    }

    /// True when a call read back from storage may be written again: the path class still
    /// allows it, the path reads strictly, and no metadata seen shows a secret for its DocType.
    /// Only calls that came out of the host's storage are judged by this looser rule; the
    /// stored file is host-controlled, so it is one barrier among several (the owner check and
    /// `may_persist` are the others), never the only one.
    pub(crate) fn may_keep(&self, method: &str, path: &str) -> bool {
        if !may_persist(method, path) {
            return false;
        }
        match target_of(path) {
            Target::Other => true,
            Target::Refused => false,
            Target::Resource(doctype) => !self.known_unsafe(&doctype),
        }
    }

    /// True when a call may be written to storage: the path class allows it and, under
    /// `/api/v1/resource/`, it has no body and no query, or its DocType is safe.
    pub(crate) fn may_store(&self, method: &str, path: &str, has_body: bool) -> bool {
        if !may_persist(method, path) {
            return false;
        }
        match target_of(path) {
            Target::Other => true,
            Target::Refused => false,
            Target::Resource(doctype) => (!has_body && !has_query(path)) || self.is_safe(&doctype),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(doctype: &str, fields: &[(&str, Option<&str>)]) -> String {
        let fields: Vec<Value> = fields
            .iter()
            .map(|(ty, child)| match child {
                Some(c) => serde_json::json!({"name": "f", "type": ty, "child_doctype": c}),
                None => serde_json::json!({"name": "f", "type": ty}),
            })
            .collect();
        serde_json::json!({"data": {"doctype": doctype, "version": 1, "fields": fields}})
            .to_string()
    }

    /// `layers` layers of `width` DocTypes, each naming every DocType of the next layer: a
    /// walk that retraced every path would never finish.
    fn layered(last_field: &str) -> MetaSeen {
        let (layers, width) = (40, 8);
        let mut meta = MetaSeen::default();
        for layer in 0..layers {
            for i in 0..width {
                let name = format!("L{layer}x{i}");
                let fields: Vec<(&str, Option<String>)> = if layer + 1 < layers {
                    (0..width)
                        .map(|j| ("table", Some(format!("L{}x{j}", layer + 1))))
                        .collect()
                } else {
                    vec![(last_field, None)]
                };
                let refs: Vec<(&str, Option<&str>)> =
                    fields.iter().map(|(t, c)| (*t, c.as_deref())).collect();
                meta.learn(
                    "GET",
                    &format!("/api/v1/meta/{name}"),
                    200,
                    &reply(&name, &refs),
                );
            }
        }
        meta
    }

    #[test]
    fn a_wide_deep_child_graph_is_decided_at_once() {
        assert!(layered("text").is_safe("L0x0"));
        assert!(!layered("secret").is_safe("L0x0"));
    }

    #[test]
    fn a_bytes_field_is_treated_as_a_possible_secret() {
        let mut meta = MetaSeen::default();
        meta.learn(
            "GET",
            "/api/v1/meta/K",
            200,
            &reply("K", &[("text", None), ("bytes", None)]),
        );
        assert!(!meta.is_safe("K"));
    }

    #[test]
    fn a_cycle_of_plain_tables_is_safe_and_a_secret_inside_it_is_not() {
        let mut meta = MetaSeen::default();
        let t = |m: &mut MetaSeen, a: &str, b: &str, extra: &str| {
            m.learn(
                "GET",
                &format!("/api/v1/meta/{a}"),
                200,
                &reply(a, &[("table", Some(b)), (extra, None)]),
            );
        };
        t(&mut meta, "A", "B", "text");
        t(&mut meta, "B", "A", "text");
        assert!(meta.is_safe("A") && meta.is_safe("B"));
        t(&mut meta, "B", "A", "secret");
        assert!(!meta.is_safe("A") && !meta.is_safe("B"));
    }
}
