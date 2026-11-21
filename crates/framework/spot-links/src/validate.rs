//! Link validation on every write: the pre-checks the engine runs before storage is asked.
//!
//! Every `Link`, `DynamicLink` and table-multiselect value is checked: the target exists in the
//! same site; its DocType is the one the field expects (or, for a dynamic link, the one its type
//! field names, and that type may be linked at all); a cancelled target is refused for a
//! submittable target; and a posted source may not point at a draft. Existence checks are
//! batched: **one query per target DocType**, never one per value.
//!
//! An `attach` field is a link to a platform file record owned by the file service, not a
//! DocType link, so it is out of scope here (see `## Open questions`). A cancelled target is
//! refused by default for every submittable target; the per-field opt-in
//! (`allow_cancelled_link`) arrives with a metadata slot and is not yet expressible.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::error::{AppError, FieldPath};
use spot_app_api::meta::DocTypeDef;
use spot_app_api::prelude::{Expr, Query, QueryPath, RecordId, Selection, Value};
use spot_app_api::{AppResult, DocState, Document};

use crate::LinkChecks;
use crate::codes;
use crate::field::{LinkTarget, occurrences};
use crate::name;

/// One invalid link, reported as a field error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldIssue {
    /// The exact field path (`customer`, or `lines[2].item`).
    pub path: FieldPath,
    /// The stable dotted code (`links.target_not_found`, ...).
    pub code: &'static str,
    /// A plain-language message naming no secret and no raw SQL.
    pub message: String,
}

/// Checks every link value of `doc` against the metadata and the store, returning one
/// [`FieldIssue`] per invalid value, ordered by field path.
///
/// `checks` is the engine's scoped view: its `query` is permission-filtered by the caller, so a
/// target the caller may not read reports as missing, exactly as a delete would refuse it. The
/// check is a **pre-check**: storage still owns referential integrity (ADR-053); this explains
/// and catches what metadata can decide before the write, with the field path.
///
/// # Errors
///
/// The mapped storage error if a batched existence query fails.
pub async fn validate_links(
    def: &DocTypeDef,
    doc: &Document,
    checks: &mut dyn LinkChecks,
) -> AppResult<Vec<FieldIssue>> {
    let meta = checks.effective_meta();
    let source_posted = doc.state() == Some(DocState::Posted);
    let found = occurrences(&meta, def, doc);

    let mut issues: Vec<FieldIssue> = Vec::new();
    // Valid occurrences, grouped by the DocType their target must exist in: one query per group.
    let mut pending: BTreeMap<String, Vec<(usize, RecordId)>> = BTreeMap::new();
    for (index, occ) in found.iter().enumerate() {
        match &occ.target {
            LinkTarget::Fixed(target) => match meta.doctype(target) {
                Some(t) if !t.is_child => {
                    pending
                        .entry(target.clone())
                        .or_default()
                        .push((index, occ.id));
                }
                _ => issues.push(issue(
                    &occ.path,
                    codes::WRONG_DOCTYPE,
                    "the linked document is of the wrong kind",
                )),
            },
            LinkTarget::Dynamic(type_name) => match dynamic_target(&meta, type_name) {
                Ok(target) => {
                    pending.entry(target).or_default().push((index, occ.id));
                }
                Err(code) => issues.push(issue(
                    &occ.path,
                    code,
                    if code == codes::DYNAMIC_TYPE_UNKNOWN {
                        "the dynamic link names no DocType in its type field"
                    } else {
                        "the dynamic link names a kind of document that cannot be linked"
                    },
                )),
            },
        }
    }

    for (target_doctype, refs) in &pending {
        let Some(target_def) = meta.doctype(target_doctype) else {
            continue;
        };
        let ids: Vec<RecordId> = dedup(refs.iter().map(|(_, id)| *id));
        let states = fetch_states(checks, &target_def, &ids).await?;
        for (index, id) in refs {
            let occ = &found[*index];
            match states.get(id).copied() {
                None => issues.push(issue(
                    &occ.path,
                    codes::TARGET_NOT_FOUND,
                    "the linked document does not exist",
                )),
                Some(state) => {
                    if target_def.is_posted && state == Some(DocState::Cancelled) {
                        issues.push(issue(
                            &occ.path,
                            codes::CANCELLED_TARGET,
                            "the linked document is cancelled",
                        ));
                    }
                    if source_posted && state == Some(DocState::Draft) {
                        issues.push(issue(
                            &occ.path,
                            codes::DRAFT_TARGET,
                            "a posted document may not point at a draft",
                        ));
                    }
                }
            }
        }
    }

    issues.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(issues)
}

/// Wraps issues into one `document.validation_failed` error with a field error per issue, or
/// `None` when there is nothing to report. The engine adds it to the operation's other problems.
pub fn validate_links_error(issues: Vec<FieldIssue>) -> Option<AppError> {
    let mut iter = issues.into_iter();
    let first = iter.next()?;
    let mut error = AppError::field(first.path, first.code, first.message);
    let mut count = 1;
    for issue in iter {
        count += 1;
        error = error.with_field(issue.path, issue.code, issue.message);
    }
    if count > 1 {
        error.message = format!("{count} link fields are invalid.");
    }
    Some(error)
}

/// The DocType a dynamic link's type field names, when it may be linked at all. `Err` is the
/// `links.*` code to report on the field.
pub(crate) fn dynamic_target(
    meta: &spot_meta::EffectiveMeta,
    type_name: &str,
) -> Result<String, &'static str> {
    if type_name.is_empty() {
        return Err(codes::DYNAMIC_TYPE_UNKNOWN);
    }
    match meta.doctype(type_name) {
        None => Err(codes::DYNAMIC_TYPE_UNKNOWN),
        Some(def) if def.is_child => Err(codes::DYNAMIC_TYPE_NOT_ALLOWED),
        Some(_) if reserved_type(type_name) => Err(codes::DYNAMIC_TYPE_NOT_ALLOWED),
        Some(_) => Ok(type_name.to_owned()),
    }
}

/// Whether `name` is a platform-reserved DocType that a dynamic link may not point at. A
/// reserved DocType is one the platform owns and a site never links at by hand; the current
/// metadata marks none, so the check is by the reserved `_` prefix and is ready for a registry
/// (see `## Open questions`).
pub(crate) fn reserved_type(name: &str) -> bool {
    name.starts_with('_')
}

/// The state of every `id` of `target`, from one query. A missing id is absent from the map.
/// The query selects the id, and `doc_state` only when the target is submittable.
async fn fetch_states(
    checks: &mut dyn LinkChecks,
    target: &DocTypeDef,
    ids: &[RecordId],
) -> AppResult<BTreeMap<RecordId, Option<DocState>>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let alias = name("candidate")?;
    let id_field = name("id")?;
    let mut query = Query::from(&target.name, &alias, QueryPath::Transactional);
    query.select.push(Selection {
        expr: Expr::field(&alias, &id_field),
        alias: name("id")?,
    });
    let state_field = name("doc_state")?;
    if target.is_posted {
        query.select.push(Selection {
            expr: Expr::field(&alias, &state_field),
            alias: name("state")?,
        });
    }
    query.filter = Some(Expr::In(
        Box::new(Expr::field(&alias, &id_field)),
        ids.iter().map(|id| Value::Id(*id)).collect(),
    ));
    let rows = checks.query(&query).await?;

    let mut out = BTreeMap::new();
    for row in &rows.rows {
        let Some(Value::Id(id)) = row.first() else {
            continue;
        };
        let state = if target.is_posted {
            row.get(1).and_then(parse_state)
        } else {
            None
        };
        out.insert(*id, state);
    }
    Ok(out)
}

/// The lifecycle state a stored `doc_state` value stands for.
fn parse_state(value: &Value) -> Option<DocState> {
    match value {
        Value::Text(text) => [DocState::Draft, DocState::Posted, DocState::Cancelled]
            .into_iter()
            .find(|s| s.as_str() == text),
        _ => None,
    }
}

/// The ids in `ids`, sorted and without repeats (a query's `IN` list).
fn dedup(ids: impl Iterator<Item = RecordId>) -> Vec<RecordId> {
    let set: BTreeSet<RecordId> = ids.collect();
    set.into_iter().collect()
}

/// One field issue with a stable code and a plain message.
fn issue(path: &FieldPath, code: &'static str, why: &str) -> FieldIssue {
    FieldIssue {
        path: path.clone(),
        code,
        message: format!("{why} ({path})."),
    }
}
