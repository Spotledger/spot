//! The SCIM client as a data channel (WP-SEC-04.T10, ADR-070 section 12 rule 6, `Channel::ApiScim`).
//!
//! A SCIM client is an external system that mirrors users out of the platform, and a user attribute
//! (e-mail, employee number, a manager) can be a protected field. [`handle_guarded`] is
//! [`crate::handle`] with the field guard in front of and behind it, as **the SCIM client's
//! principal** (the service user that owns the credential, supplied by the host):
//!
//! - **Before.** The attributes a `filter` or `sortBy` names are asked of
//!   [`FieldGuard::check_query_fields_for`]. A filter on an attribute the client may not read
//!   **unmasked** is refused with the same sentence an attribute the user does not have gets, so a
//!   filter cannot be used as an oracle for a value or for the attribute's existence. The refusal
//!   comes after the credential check and before any record is read.
//! - **After.** Every user resource in the answer (a single resource, a `POST`/`PUT`/`PATCH`
//!   answer, each entry of a list) is rendered through [`FieldGuard::redact`]: a hidden
//!   attribute is removed, a masked one carries the masked form, a guard error removes the
//!   attribute (fail closed).
//!
//! # Attribute to field
//!
//! Each attribute is judged as the `user` DocType field it comes from ([`field_of`]): `userName` is
//! `user_name`, `displayName` and every `name.*` part are `full_name`, `emails` is `email`,
//! `externalId` is `external_id`, the enterprise `employeeNumber`, `department` and `manager` carry
//! their own names. A field the DocType does not declare is unprotected (the guard's own rule), so
//! a tenant protects `employee_number` by declaring it with a protection group. `id`, `meta`,
//! `schemas` and `active` are structural and never judged.
//!
//! # Not guarded here
//!
//! A group has no DocType (its name and member list are SCIM-only records), so group resources pass
//! through unchanged; see the task report. Nothing here reads a clock or a random source.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};
use spot_app_api::Document;
use spot_app_api::prelude::{Name as FieldName, RecordId, Value};
use spot_field_guard::{Channel, FieldGuard, QueryField, QueryUse};
use spot_permissions::{DocFacts, Principal};

use crate::error::{ScimError, ScimResult};
use crate::filter::{AttrPath, Filter, path_parts};
use crate::request::{Method, ScimContext, ScimRequest, ScimResponse, handle};
use crate::resource::{ENTERPRISE_USER_SCHEMA, SCIM_BASE, USER_SCHEMA};

/// The DocType a user resource is judged as.
pub const USER_DOCTYPE: &str = "user";

/// The id every judged document carries: the guard needs one and no value depends on it.
const PROBE_ID: [u8; 16] = [0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1];

/// The `user` DocType field a base attribute (lower-case, enterprise prefix stripped) comes from,
/// or `None` for a structural attribute that is never judged.
#[must_use]
pub fn field_of(base: &str) -> Option<&'static str> {
    match base {
        "username" => Some("user_name"),
        "displayname" | "name" => Some("full_name"),
        "emails" => Some("email"),
        "externalid" => Some("external_id"),
        "employeenumber" => Some("employee_number"),
        "department" => Some("department"),
        "manager" => Some("manager"),
        "active" => Some("state"),
        _ => None,
    }
}

/// The field guard and the principal a SCIM client's answers are built for.
#[derive(Clone)]
pub struct ScimGuard {
    guard: FieldGuard,
    principal: Principal,
    doctype: String,
    fields: BTreeMap<String, String>,
}

impl std::fmt::Debug for ScimGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScimGuard")
    }
}

impl ScimGuard {
    /// A guard judging users as the [`USER_DOCTYPE`] DocType for `principal`.
    #[must_use]
    pub fn new(guard: FieldGuard, principal: Principal) -> Self {
        Self {
            guard,
            principal,
            doctype: USER_DOCTYPE.to_owned(),
            fields: BTreeMap::new(),
        }
    }

    /// Judges the base attribute `attr` (lower-case, as [`field_of`] takes it) as `field` instead
    /// of the default: a tenant whose user DocType names its fields differently.
    #[must_use]
    pub fn with_field(mut self, attr: &str, field: &str) -> Self {
        self.fields.insert(attr.to_owned(), field.to_owned());
        self
    }

    /// The field a base attribute is judged as, or `None` for a structural attribute.
    fn field(&self, attr: &str) -> Option<String> {
        self.fields
            .get(attr)
            .cloned()
            .or_else(|| field_of(attr).map(str::to_owned))
    }

    /// Judges users as another DocType (a tenant's renamed user DocType).
    #[must_use]
    pub fn with_doctype(mut self, doctype: &str) -> Self {
        doctype.clone_into(&mut self.doctype);
        self
    }

    /// The text of one attribute leaf as the client may see it: the value, its masked form, or
    /// `None` when the attribute is hidden (or the guard failed: fail closed).
    #[must_use]
    pub fn leaf(&self, field: &str, text: &str) -> Option<String> {
        let id = RecordId::from_bytes(PROBE_ID).ok()?;
        let doctype = FieldName::new(&self.doctype).ok()?;
        let name = FieldName::new(field).ok()?;
        let mut doc = Document::new(doctype, id);
        doc.set(name.clone(), Value::Text(text.to_owned()));
        let facts = DocFacts {
            doctype: self.doctype.clone(),
            ..DocFacts::default()
        };
        self.guard
            .redact(
                Channel::ApiScim,
                &self.principal,
                &self.doctype,
                &facts,
                &mut doc,
            )
            .ok()?;
        match doc.fields().get(&name) {
            Some(Value::Text(shown)) => Some(shown.clone()),
            _ => None,
        }
    }

    /// Refuses a `GET /Users` whose `filter` or `sortBy` names an attribute the client may not
    /// read unmasked. `max_filter_chars` bounds the parse (a longer filter is left for
    /// [`handle`] to refuse as it always does).
    ///
    /// # Errors
    ///
    /// `iam_scim.invalid_filter` with the sentence an attribute a user does not have gets.
    pub fn check_request(&self, request: &ScimRequest, max_filter_chars: usize) -> ScimResult<()> {
        let path = request
            .path
            .strip_prefix(SCIM_BASE)
            .unwrap_or(&request.path)
            .trim_matches('/');
        if path != "Users" || request.method != Method::Get {
            return Ok(());
        }
        let mut used: Vec<(String, QueryUse)> = Vec::new();
        if let Some(text) = request.query.get("filter")
            && text.chars().count() <= max_filter_chars
            && let Ok(filter) = Filter::parse(text)
        {
            collect(&filter, &mut used);
        }
        if let Some(text) = request.query.get("sortBy")
            && text.chars().count() <= max_filter_chars
            && let Ok(path) = Filter::parse_path(text)
        {
            used.push((path.attr, QueryUse::Sort));
        }
        for (attr, usage) in used {
            let attr_path = AttrPath {
                attr: attr.clone(),
                sub: None,
                value_filter: None,
            };
            let (base, _) = path_parts(&attr_path);
            let Some(field) = self.field(base) else {
                continue;
            };
            let query = [QueryField::new(&field, usage)];
            let allowed = self
                .guard
                .check_query_fields_for(Channel::ApiScim, &self.principal, &self.doctype, &query)
                .is_ok();
            if !allowed {
                return Err(ScimError::invalid_filter(format!(
                    "the filter names an attribute a user does not have: `{attr}`"
                )));
            }
        }
        Ok(())
    }

    /// Renders every user resource inside `body` through the guard: a single resource, or each
    /// entry of a list. Anything else (a group, an error, a schema) is left alone.
    pub fn redact_json(&self, body: &mut Json) {
        match body {
            Json::Object(map) if is_user(map) => self.redact_user(map),
            Json::Object(map) => {
                if let Some(Json::Array(resources)) = map.get_mut("Resources") {
                    for resource in resources {
                        self.redact_json(resource);
                    }
                }
            }
            _ => {}
        }
    }

    /// Renders the body of `response` through the guard.
    pub fn redact_response(&self, response: &mut ScimResponse) {
        if let Some(body) = response.body.as_mut() {
            self.redact_json(body);
        }
    }

    /// The text of an attribute leaf, judged as the field the base attribute `attr` maps to.
    fn leaf_of(&self, attr: &str, text: &str) -> Option<String> {
        match self.field(attr) {
            Some(field) => self.leaf(&field, text),
            None => Some(text.to_owned()),
        }
    }

    /// Replaces or removes the string at `key`, judged as the base attribute `attr`.
    fn scalar(&self, map: &mut Map<String, Json>, key: &str, attr: &str) {
        let Some(Json::String(text)) = map.get(key) else {
            return;
        };
        match self.leaf_of(attr, text) {
            Some(shown) => {
                map.insert(key.to_owned(), Json::String(shown));
            }
            None => {
                map.remove(key);
            }
        }
    }

    fn redact_user(&self, map: &mut Map<String, Json>) {
        self.scalar(map, "userName", "username");
        self.scalar(map, "displayName", "displayname");
        self.scalar(map, "externalId", "externalid");
        if let Some(Json::Object(name)) = map.get_mut("name") {
            let keys: Vec<String> = name.keys().cloned().collect();
            for key in keys {
                self.scalar(name, &key, "name");
            }
        }
        if matches!(map.get("name"), Some(Json::Object(name)) if name.is_empty()) {
            map.remove("name");
        }
        if let Some(Json::Array(emails)) = map.get_mut("emails") {
            let kept: Vec<Json> = emails
                .drain(..)
                .filter_map(|mut email| {
                    let Json::Object(entry) = &mut email else {
                        return None;
                    };
                    let Some(Json::String(value)) = entry.get("value") else {
                        return None;
                    };
                    let shown = self.leaf_of("emails", value)?;
                    entry.insert("value".to_owned(), Json::String(shown));
                    Some(email)
                })
                .collect();
            *emails = kept;
        }
        if matches!(map.get("emails"), Some(Json::Array(emails)) if emails.is_empty()) {
            map.remove("emails");
        }
        if let Some(Json::Object(enterprise)) = map.get_mut(ENTERPRISE_USER_SCHEMA) {
            self.scalar(enterprise, "employeeNumber", "employeenumber");
            self.scalar(enterprise, "department", "department");
            if let Some(Json::Object(manager)) = enterprise.get_mut("manager") {
                self.scalar(manager, "value", "manager");
                self.scalar(manager, "displayName", "manager");
            }
            if matches!(enterprise.get("manager"), Some(Json::Object(m)) if m.is_empty()) {
                enterprise.remove("manager");
            }
        }
        if matches!(map.get(ENTERPRISE_USER_SCHEMA), Some(Json::Object(e)) if e.is_empty()) {
            map.remove(ENTERPRISE_USER_SCHEMA);
        }
    }
}

fn is_user(map: &Map<String, Json>) -> bool {
    matches!(map.get("schemas"), Some(Json::Array(schemas))
        if schemas.iter().any(|s| s.as_str() == Some(USER_SCHEMA)))
}

fn collect(filter: &Filter, out: &mut Vec<(String, QueryUse)>) {
    match filter {
        Filter::Compare { path, .. } | Filter::Present { path } => {
            // A value filter's inner terms name the sub-attributes of this same attribute.
            out.push((path.attr.clone(), QueryUse::Filter));
        }
        Filter::And(terms) | Filter::Or(terms) => {
            for term in terms {
                collect(term, out);
            }
        }
        Filter::Not(term) => collect(term, out),
    }
}

/// [`handle`] with the field guard before and after it.
///
/// The credential is checked first, so an unauthenticated caller learns nothing about which
/// attributes exist.
pub fn handle_guarded(
    request: ScimRequest,
    ctx: &mut ScimContext<'_>,
    guard: &ScimGuard,
) -> ScimResponse {
    if let Err(error) = ctx.credential.authorise(ctx.now) {
        return ScimResponse::error(&error);
    }
    if let Err(error) = guard.check_request(&request, ctx.limits.max_filter_chars) {
        return ScimResponse::error(&error);
    }
    let mut response = handle(request, ctx);
    guard.redact_response(&mut response);
    response
}
