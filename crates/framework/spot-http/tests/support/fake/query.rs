//! The fake's own list/query engine over `StubHost`: filtering, sorting, cursors and
//! projection in memory. `StubHost` answers `platform.unsupported` for queries, so the fake
//! scans: ids newest first, an opaque cursor that is the last start offset seen.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_app_api::context::DocumentService;
use spot_app_api::{Actor, Document, Invocation, prelude::*};
use spot_http::api::codec::{ApiDocument, FieldValue};
use spot_http::api::filter::{Comparison, FilterAst, FilterValue, Op, OrderKey};
use spot_http::api::lookup::{LookupPage, LookupRequest, LookupRow};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, Page, QuerySpec};
use spot_storage_contract::{Name, RecordId, Value};

use crate::support::fake::LookupOverride;
use crate::support::harness;

use super::{FakeApp, fn_name, ok, services};

impl FakeApp {
    /// The newest-first id scan with the filter applied, sorted, then sliced by limit and
    /// cursor. `with_total` decides whether the page carries `total`.
    pub async fn fake_list(
        &self,
        doctype: &str,
        spec: &QuerySpec,
        inv: &Invocation,
    ) -> Result<Page, ApiError> {
        let ids = self.ids_of(doctype);
        let start = match &spec.cursor {
            Some(cursor) => cursor
                .strip_prefix("p:")
                .and_then(|offset| offset.parse::<usize>().ok())
                .ok_or_else(|| {
                    ApiError::new(Code::ApiInvalidRequest, "the cursor is not valid here.")
                })?
                .min(ids.len()),
            None => 0,
        };
        let mut matched: Vec<RecordId> = Vec::new();
        for id in &ids[start..] {
            let outcome = DocumentService::load(&self.host, inv, doctype, *id)
                .await
                .map_err(services::api_error)?;
            let api_doc = self.to_api(&outcome.value);
            if let Some(ast) = &spec.filter
                && !matches(ast, &api_doc)?
            {
                continue;
            }
            matched.push(*id);
        }
        if !spec.order.is_empty() {
            sort_ids(self, doctype, inv, &mut matched, &spec.order).await?;
        }
        let total = spec
            .with_total
            .then(|| u64::try_from(matched.len()).unwrap_or(u64::MAX));
        let take = usize::try_from(spec.limit).unwrap_or(0).min(matched.len());
        let mut documents = Vec::with_capacity(take);
        for id in matched.iter().take(take) {
            let outcome = DocumentService::load(&self.host, inv, doctype, *id)
                .await
                .map_err(services::api_error)?;
            let mut api_doc = self.to_api(&outcome.value);
            project(&mut api_doc, &spec.fields);
            documents.push(api_doc);
        }
        let next_cursor = (matched.len() > take).then(|| format!("p:{}", start + take));
        Ok(Page {
            documents,
            next_cursor,
            total,
        })
    }
}

/// Keeps only the requested fields; empty means every field (id and version stay).
fn project(api: &mut ApiDocument, wanted: &[Name]) {
    if !wanted.is_empty() {
        api.fields.retain(|name, _| wanted.contains(name));
    }
}

fn matches(ast: &FilterAst, doc: &ApiDocument) -> Result<bool, ApiError> {
    Ok(match ast {
        FilterAst::Cmp(cmp) => matches_cmp(cmp, doc)?,
        FilterAst::And(parts) => {
            for part in parts {
                if !matches(part, doc)? {
                    return Ok(false);
                }
            }
            true
        }
        FilterAst::Or(parts) => {
            for part in parts {
                if matches(part, doc)? {
                    return Ok(true);
                }
            }
            false
        }
        FilterAst::Not(part) => !matches(part, doc)?,
    })
}

fn matches_cmp(cmp: &Comparison, doc: &ApiDocument) -> Result<bool, ApiError> {
    let scalar = doc.fields.get(&cmp.field).and_then(|value| match value {
        FieldValue::Scalar(value) => Some(value),
        FieldValue::Rows(_) => None,
    });
    let present = scalar.is_some_and(|value| !value.is_null());
    match cmp.op {
        Op::IsNull => Ok(!present),
        Op::IsNotNull => Ok(present),
        Op::Eq | Op::In => Ok(present
            && cmp.values.iter().any(|wanted| {
                cmp_value(scalar.unwrap_or(&Value::Null), wanted, cmp.case_insensitive)
            })),
        Op::Ne => Ok(present
            && !cmp.values.iter().any(|wanted| {
                cmp_value(scalar.unwrap_or(&Value::Null), wanted, cmp.case_insensitive)
            })),
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            if !present {
                return Ok(false);
            }
            let current = scalar.unwrap_or(&Value::Null);
            Ok(cmp
                .values
                .iter()
                .any(|wanted| match cmp_order(current, wanted) {
                    Some(ordering) => match cmp.op {
                        Op::Lt => ordering == std::cmp::Ordering::Less,
                        Op::Le => ordering < std::cmp::Ordering::Greater,
                        Op::Gt => ordering == std::cmp::Ordering::Greater,
                        _ => ordering > std::cmp::Ordering::Less,
                    },
                    None => false,
                }))
        }
        Op::Prefix | Op::Contains | Op::Suffix => {
            let Some(Value::Text(current)) = scalar else {
                return Ok(false);
            };
            let current = if cmp.case_insensitive {
                current.to_ascii_lowercase()
            } else {
                current.clone()
            };
            Ok(cmp.values.iter().any(|wanted| match wanted {
                FilterValue::Text(wanted) => {
                    let wanted = if cmp.case_insensitive {
                        wanted.to_ascii_lowercase()
                    } else {
                        wanted.clone()
                    };
                    match cmp.op {
                        Op::Prefix => current.starts_with(wanted.as_str()),
                        Op::Contains => current.contains(wanted.as_str()),
                        _ => current.ends_with(wanted.as_str()),
                    }
                }
                _ => false,
            }))
        }
    }
}

fn cmp_value(current: &Value, wanted: &FilterValue, case_insensitive: bool) -> bool {
    match (current, wanted) {
        (Value::Text(left), FilterValue::Text(right)) => {
            if case_insensitive {
                left.eq_ignore_ascii_case(right.as_str())
            } else {
                left == right
            }
        }
        (Value::Int(left), FilterValue::Int(right)) => left == right,
        (Value::Bool(left), FilterValue::Bool(right)) => left == right,
        _ => false,
    }
}

fn cmp_order(current: &Value, wanted: &FilterValue) -> Option<std::cmp::Ordering> {
    match (current, wanted) {
        (Value::Text(left), FilterValue::Text(right)) => Some(left.cmp(right)),
        (Value::Int(left), FilterValue::Int(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

/// Sorts the matched ids by the order keys' stored scalars. The suites order on text and on
/// int fields whose stored text forms order like their values do.
async fn sort_ids(
    fake: &FakeApp,
    doctype: &str,
    inv: &Invocation,
    ids: &mut Vec<RecordId>,
    order: &[OrderKey],
) -> Result<(), ApiError> {
    let mut keyed: Vec<(Vec<String>, RecordId)> = Vec::with_capacity(ids.len());
    for id in ids.iter() {
        let outcome = DocumentService::load(&fake.host, inv, doctype, *id)
            .await
            .map_err(super::services::api_error)?;
        let document: &Document = &outcome.value;
        let mut keys = Vec::with_capacity(order.len());
        for key in order {
            keys.push(scalar_text(document.get(key.field.as_str())));
        }
        keyed.push((keys, *id));
    }
    keyed.sort_by(|left, right| {
        for (index, key) in order.iter().enumerate() {
            let ordering = match key.dir {
                OrderDir::Asc => left.0[index].cmp(&right.0[index]),
                _ => right.0[index].cmp(&left.0[index]),
            };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
    *ids = keyed.into_iter().map(|(_, id)| id).collect();
    Ok(())
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        _ => String::new(),
    }
}

/// The `MethodOutcome.result` payload for the whitelisted `totals` method: the row count and
/// the parent total, encoded by the Values table.
pub fn totals_json(fake: &FakeApp, document: &Document) -> Json {
    let mut map = serde_json::Map::new();
    map.insert(
        "count".to_owned(),
        Json::from(u64::try_from(document.rows("items").len()).unwrap_or(u64::MAX)),
    );
    map.insert("total".to_owned(), fake.value_json(document.get("total")));
    Json::Object(map)
}

/// The fake's platform-default link search and the named-override dispatcher
/// (ADR-068 §3.4). Labels come from the target DocType's title field; matching is
/// case-insensitive on the label, prefix first and contains as the fallback, so a search
/// stays predictable while still finding "Acme Karachi" for "karachi".
impl FakeApp {
    /// The title field of a DocType, from the DocTypeDef the fake seeded (the fake keeps
    /// `title_field` per DocType next to its metas); an empty string means no title field
    /// and every row label falls back to the document id.
    pub fn title_field_of(&self, doctype: &str) -> String {
        self.title_fields.get(doctype).cloned().unwrap_or_default()
    }

    /// The invocation the link search loads documents under: the system actor (the stub
    /// host polices no permissions; the real host's search runs as the caller, ADR-068 §3).
    fn link_search_invocation(&self) -> Invocation {
        Invocation {
            site: ok(SiteId::new(harness::SITE)),
            actor: Actor::System {
                component: fn_name("http"),
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        }
    }

    /// The page the platform default (link search over the target DocType) answers.
    pub async fn platform_page(
        &self,
        target: &Name,
        title_field: &str,
        request: &LookupRequest,
    ) -> Result<LookupPage, ApiError> {
        let needle = request.text.to_lowercase();
        let ids = self.ids_of(target.as_str());
        let mut labels: Vec<Option<String>> = Vec::with_capacity(ids.len());
        for id in &ids {
            let invocation = self.link_search_invocation();
            let outcome = DocumentService::load(&self.host, &invocation, target.as_str(), *id)
                .await
                .map_err(super::services::api_error)?;
            labels.push(title_label(&outcome.value, title_field));
        }
        // Prefix matches first, then contains matches, both case-insensitive, in the ids'
        // newest-first order; an empty text lists everything, so an opened form gets
        // candidates before the user types.
        let mut matched: Vec<RecordId> = Vec::new();
        for pass in [MatchPass::Prefix, MatchPass::Contains] {
            for (index, id) in ids.iter().enumerate() {
                if matched.contains(id) {
                    continue;
                }
                let fits = match (&labels[index], pass) {
                    (None, _) => needle.is_empty(),
                    (Some(label), MatchPass::Prefix) => label.starts_with(&needle),
                    (Some(label), MatchPass::Contains) => label.contains(&needle),
                };
                if fits {
                    matched.push(*id);
                }
            }
        }
        // Paging like the list engine's: the opaque `p:<offset>` cursor.
        const PAGE_SIZE: usize = 3;
        let start = match &request.cursor {
            Some(cursor) => cursor
                .strip_prefix("p:")
                .and_then(|offset| offset.parse::<usize>().ok())
                .ok_or_else(|| {
                    ApiError::new(Code::ApiInvalidRequest, "the cursor is not valid here.")
                })?
                .min(matched.len()),
            None => 0,
        };
        let take = PAGE_SIZE.min(matched.len().saturating_sub(start));
        let mut rows = Vec::with_capacity(take);
        for id in matched.iter().skip(start).take(take) {
            let index = ids
                .iter()
                .position(|candidate| candidate == id)
                .unwrap_or_default();
            let label = labels[index].clone().unwrap_or_else(|| id.to_string());
            rows.push(LookupRow {
                id: *id,
                label,
                description: None,
                extras: BTreeMap::new(),
                cells: BTreeMap::new(),
            });
        }
        let has_more = start + take < matched.len();
        let cursor = (has_more).then(|| format!("p:{}", start + take));
        Ok(LookupPage {
            rows,
            has_more,
            cursor,
            columns: Vec::new(),
        })
    }

    /// Dispatches to a named override seam: the behaviour a test registered, or
    /// `lookup.seam_failed` when nothing is registered for the name (a missing seam fails
    /// the operation; it never falls back to the default list, ADR-068 §3.8).
    pub async fn override_page(
        &self,
        seam: &str,
        doctype: &str,
        field: &str,
        request: &LookupRequest,
    ) -> Result<LookupPage, ApiError> {
        let request_json = serde_json::json!({
            "doctype": doctype,
            "field": field,
            "text": request.text,
            "cursor": request.cursor,
            "doc": request.doc,
            "parent": request.parent,
        });
        let entry = {
            let entries = super::lock(&self.lookup_overrides);
            match entries.get(seam) {
                Some(entry) => entry.clone(),
                None => {
                    return Err(ApiError::new(
                        Code::LookupSeamFailed,
                        format!("The seam `{seam}` is not registered."),
                    )
                    .explained(format!(
                        "the lookup of `{doctype}.{field}` chose a seam no provider answers."
                    )));
                }
            }
        };
        *super::lock(&entry.last) = Some(request_json);
        match &entry.behaviour {
            LookupOverride::Failing(explanation) => Err(ApiError::new(
                Code::LookupSeamFailed,
                "The lookup seam failed.",
            )
            .explained(explanation.clone())),
            LookupOverride::Static(page) => Ok(page.clone()),
            LookupOverride::Spy => Ok(LookupPage {
                rows: vec![LookupRow {
                    id: RecordId::new_v7(),
                    label: seam.to_owned(),
                    description: None,
                    extras: BTreeMap::new(),
                    cells: BTreeMap::new(),
                }],
                has_more: false,
                cursor: None,
                columns: Vec::new(),
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum MatchPass {
    Prefix,
    Contains,
}

/// The lower-case label of a stored document from its title field, or `None` when the
/// DocType has no title field (the search then lists plain ids).
fn title_label(document: &Document, title_field: &str) -> Option<String> {
    if title_field.is_empty() {
        return None;
    }
    match document.get(title_field) {
        Value::Text(label) => Some(label.to_lowercase()),
        _ => None,
    }
}
