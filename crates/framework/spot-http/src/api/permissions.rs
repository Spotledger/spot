//! The permission summary of `GET /api/v1/auth/permissions` (contract 0.5.0, ADR-070 §12)
//! and its JSON encoding. The types only carry what a
//! [`PermissionService`](crate::api::service::PermissionService) returns: no right, mask,
//! limit or currency is computed here. The summary is a UI hint, never authority.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_storage_contract::Name;

use super::registry::Code;
use super::service::ApiError;

/// The contract's `maxItems` of an attach field's `accept` list.
pub const MAX_ACCEPT: usize = 64;

/// The eight document rights of one DocType for the signed-in user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rights {
    /// May read.
    pub read: bool,
    /// May create.
    pub create: bool,
    /// May write.
    pub write: bool,
    /// May delete.
    pub delete: bool,
    /// May post (the contract calls it `submit`).
    pub submit: bool,
    /// May cancel.
    pub cancel: bool,
    /// May correct.
    pub correct: bool,
    /// May rename.
    pub rename: bool,
}

/// The per-user field summary. Lists are in definition order and name field paths only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldSummary {
    /// Fields the user cannot read at all.
    pub hidden: Vec<String>,
    /// Fields visible but not writable.
    pub read_only: Vec<String>,
    /// Fields read only in masked form.
    pub masked: Vec<String>,
    /// The masked fields the user may reveal; a subset of `masked`.
    pub revealable: Vec<String>,
}

/// What this user may upload to one `attach` field, already resolved by the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachLimit {
    /// The largest file, in bytes; at least 1.
    pub max_bytes: u64,
    /// Media types allowed (`*` allowed as the subtype); `None` means any type.
    pub accept: Option<Vec<String>>,
}

/// One DocType's entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctypePermissions {
    /// The DocType.
    pub doctype: Name,
    /// The document rights.
    pub rights: Rights,
    /// The field summary.
    pub fields: FieldSummary,
    /// Attachment limits by `attach` field path; omitted from the response when empty.
    pub attach_limits: BTreeMap<String, AttachLimit>,
}

/// The answer of the permission service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionSummary {
    /// One entry per requested (or visible) DocType.
    pub doctypes: Vec<DoctypePermissions>,
    /// The permission snapshot version; the `ETag` is its quoted decimal form.
    pub version: u64,
}

impl PermissionSummary {
    /// The strong `ETag` value, `"<version>"`.
    pub fn etag(&self) -> String {
        format!("\"{}\"", self.version)
    }

    /// The contract's `PermissionSummary` body. Defensive in what it sends: a hidden field
    /// is removed from every other list, `revealable` keeps only fields that are `masked`,
    /// and an attach limit that breaks the contract's bounds is a service bug
    /// (`http.internal`).
    pub fn to_json(&self) -> Result<Json, ApiError> {
        let mut doctypes = Vec::with_capacity(self.doctypes.len());
        for entry in &self.doctypes {
            doctypes.push(doctype_json(entry)?);
        }
        let mut data = serde_json::Map::new();
        data.insert("doctypes".to_owned(), Json::Array(doctypes));
        let mut body = serde_json::Map::new();
        body.insert("data".to_owned(), Json::Object(data));
        body.insert("etag".to_owned(), Json::String(self.etag()));
        Ok(Json::Object(body))
    }
}

fn strings(items: impl Iterator<Item = String>) -> Json {
    Json::Array(items.map(Json::String).collect())
}

fn doctype_json(entry: &DoctypePermissions) -> Result<Json, ApiError> {
    let r = &entry.rights;
    let mut rights = serde_json::Map::new();
    for (key, value) in [
        ("read", r.read),
        ("create", r.create),
        ("write", r.write),
        ("delete", r.delete),
        ("submit", r.submit),
        ("cancel", r.cancel),
        ("correct", r.correct),
        ("rename", r.rename),
    ] {
        rights.insert(key.to_owned(), Json::from(value));
    }
    let f = &entry.fields;
    let hidden = |name: &&String| !f.hidden.contains(name);
    let read_only = f.read_only.iter().filter(hidden);
    let masked: Vec<&String> = f.masked.iter().filter(hidden).collect();
    let mut fields = serde_json::Map::new();
    fields.insert("hidden".to_owned(), strings(f.hidden.iter().cloned()));
    fields.insert("read_only".to_owned(), strings(read_only.cloned()));
    fields.insert(
        "masked".to_owned(),
        strings(masked.iter().map(|s| (*s).clone())),
    );
    let revealable: Vec<String> = f
        .revealable
        .iter()
        .filter(|name| masked.contains(name))
        .cloned()
        .collect();
    if !revealable.is_empty() {
        fields.insert("revealable".to_owned(), strings(revealable.into_iter()));
    }
    let mut map = serde_json::Map::new();
    map.insert(
        "doctype".to_owned(),
        Json::String(entry.doctype.as_str().to_owned()),
    );
    map.insert("rights".to_owned(), Json::Object(rights));
    map.insert("fields".to_owned(), Json::Object(fields));
    if !entry.attach_limits.is_empty() {
        let mut limits = serde_json::Map::new();
        for (path, limit) in &entry.attach_limits {
            if f.hidden.contains(path) {
                continue;
            }
            limits.insert(path.clone(), limit_json(limit)?);
        }
        if !limits.is_empty() {
            map.insert("attach_limits".to_owned(), Json::Object(limits));
        }
    }
    Ok(Json::Object(map))
}

fn limit_json(limit: &AttachLimit) -> Result<Json, ApiError> {
    let bad = |what: &str| ApiError::new(Code::HttpInternal, format!("an attach limit {what}."));
    if limit.max_bytes < 1 {
        return Err(bad("allows no bytes"));
    }
    let mut map = serde_json::Map::new();
    map.insert("max_bytes".to_owned(), Json::from(limit.max_bytes));
    if let Some(accept) = &limit.accept {
        if accept.len() > MAX_ACCEPT || !accept.iter().all(|m| is_media_range(m)) {
            return Err(bad("has an invalid accept list"));
        }
        map.insert("accept".to_owned(), strings(accept.iter().cloned()));
    }
    Ok(Json::Object(map))
}

/// The contract's `^[a-z0-9.+-]+/[a-z0-9.+*-]+$`.
fn is_media_range(s: &str) -> bool {
    let Some((main, sub)) = s.split_once('/') else {
        return false;
    };
    let ok = |part: &str, star: bool| {
        !part.is_empty()
            && part.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'.' | b'+' | b'-')
                    || (star && b == b'*')
            })
    };
    ok(main, false) && ok(sub, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Must;

    fn entry() -> DoctypePermissions {
        DoctypePermissions {
            doctype: Name::new("customer").must(),
            rights: Rights::default(),
            fields: FieldSummary::default(),
            attach_limits: BTreeMap::new(),
        }
    }

    #[test]
    fn revealable_is_a_subset_of_masked_and_hidden_appears_nowhere_else() {
        let mut e = entry();
        e.fields.hidden = vec!["secret".into()];
        e.fields.masked = vec!["iban".into(), "secret".into()];
        e.fields.revealable = vec!["iban".into(), "other".into(), "secret".into()];
        e.fields.read_only = vec!["secret".into(), "total".into()];
        let json = doctype_json(&e).must();
        assert_eq!(json["fields"]["masked"], serde_json::json!(["iban"]));
        assert_eq!(json["fields"]["revealable"], serde_json::json!(["iban"]));
        assert_eq!(json["fields"]["read_only"], serde_json::json!(["total"]));
    }

    #[test]
    fn empty_revealable_and_limits_are_omitted() {
        let json = doctype_json(&entry()).must();
        assert!(json["fields"].get("revealable").is_none());
        assert!(json.get("attach_limits").is_none());
    }

    #[test]
    fn attach_limits_are_bounded() {
        let mut e = entry();
        e.attach_limits.insert(
            "scan".into(),
            AttachLimit {
                max_bytes: 0,
                accept: None,
            },
        );
        assert!(doctype_json(&e).is_err());
        e.attach_limits.insert(
            "scan".into(),
            AttachLimit {
                max_bytes: 10,
                accept: Some(vec!["application/pdf".into(), "image/*".into()]),
            },
        );
        let json = doctype_json(&e).must();
        assert_eq!(json["attach_limits"]["scan"]["max_bytes"], 10);
        e.attach_limits.insert(
            "scan".into(),
            AttachLimit {
                max_bytes: 10,
                accept: Some(vec!["PDF".into()]),
            },
        );
        assert!(doctype_json(&e).is_err());
    }
}
