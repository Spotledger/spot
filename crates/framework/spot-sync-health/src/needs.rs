//! Which open conditions need a person, and how they collapse into rows.

use std::collections::BTreeMap;

use spot_conditions::{ConditionParams, HealthThresholds, Millis, Open, Severity};
use spot_sync_conditions::CatalogueEntry;

use crate::model::{MAX_ROWS, NeedsYou, Route, SAMPLE_SIZE};

/// The message key of the remedy when a definition names no action at all.
pub const REVIEW_REMEDY: &str = concat!("sync", ".remedy", ".review");

/// Whether `open` is on the `needs you` list at `now`.
///
/// A condition is listed only when its definition says a person must respond *and* the
/// automation is spent or never applied:
///
/// * `info` never needs anyone.
/// * `red` needs a person at once; the detector has already decided.
/// * `amber` needs a person when the definition names an operator command (or is not in the
///   catalogue, which cannot be judged and is not hidden) and either it has no automatic
///   response, or it has stayed open past the threshold, which is when the automatic steps
///   count as exhausted.
pub fn needs_a_person(
    open: &Open,
    entry: Option<&CatalogueEntry>,
    now: Millis,
    thresholds: &HealthThresholds,
) -> bool {
    match open.severity {
        Severity::Info => false,
        Severity::Red => true,
        Severity::Amber => {
            let human_required = entry.is_none_or(|entry| {
                !entry.def.actions.is_empty()
                    || entry
                        .def
                        .escalation
                        .iter()
                        .any(|step| step.to == Severity::Red)
            });
            if !human_required {
                return false;
            }
            let inapplicable = entry.is_some_and(|entry| entry.def.response.is_empty());
            inapplicable
                || now.elapsed_at_least(open.raised_at_millis, thresholds.amber_needs_person_after)
        }
    }
}

/// The message key of what to do about `entry`: its primary remedy when the catalogue names
/// one, else its first operator command, else its last automatic step (the one that ran out),
/// else a generic review.
fn remedy_key(entry: Option<&CatalogueEntry>) -> String {
    entry
        .and_then(CatalogueEntry::remedy_message_key)
        .unwrap_or_else(|| REVIEW_REMEDY.to_owned())
}

/// Whether `text` looks like an id: short, no whitespace, only id characters. Anything else
/// is never shown, because an id is the only thing a subject or parameter may carry.
fn is_id_like(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/' | '#' | '+')
        })
}

/// The text shown instead of a value that is not id-like.
pub const REDACTED: &str = "(redacted)";

fn id_only(text: &str) -> String {
    if is_id_like(text) {
        text.to_owned()
    } else {
        REDACTED.to_owned()
    }
}

fn safe_params(params: &ConditionParams) -> ConditionParams {
    params
        .iter()
        .map(|(name, value)| (name.to_owned(), id_only(value)))
        .collect()
}

/// One item on its way into a row.
struct Item<'a> {
    open: &'a Open,
}

/// Collapse the listed conditions into rows: same remedy and route, one row, a count and a
/// sample. Returns every row, sorted worst first, then oldest first, then by key.
pub fn group(listed: &[(&Open, Option<&CatalogueEntry>)]) -> Vec<NeedsYou> {
    let mut groups: BTreeMap<(String, Route), Vec<Item<'_>>> = BTreeMap::new();
    for (open, entry) in listed {
        let route = entry.map_or(Route::Condition, |entry| {
            Route::for_subject_kind(&entry.def.subject_kind)
        });
        groups
            .entry((remedy_key(*entry), route))
            .or_default()
            .push(Item { open });
    }
    let mut rows: Vec<NeedsYou> = groups
        .into_iter()
        .map(|((remedy, route), mut items)| {
            // Worst first, then oldest, then by name: the first item is the headline one.
            items.sort_by(|left, right| {
                right
                    .open
                    .severity
                    .cmp(&left.open.severity)
                    .then_with(|| left.open.raised_at_millis.cmp(&right.open.raised_at_millis))
                    .then_with(|| left.open.id.cmp(&right.open.id))
                    .then_with(|| left.open.subject.cmp(&right.open.subject))
            });
            let count = items.len();
            let head = items.first().map(|item| item.open);
            let mut by_age: Vec<&Item<'_>> = items.iter().collect();
            by_age.sort_by(|left, right| {
                left.open
                    .raised_at_millis
                    .cmp(&right.open.raised_at_millis)
                    .then_with(|| left.open.subject.cmp(&right.open.subject))
                    .then_with(|| left.open.id.cmp(&right.open.id))
            });
            let mut conditions: Vec<String> = items
                .iter()
                .map(|item| item.open.id.as_str().to_owned())
                .collect();
            conditions.sort();
            conditions.dedup();
            let sample: Vec<String> = by_age
                .iter()
                .take(SAMPLE_SIZE)
                .map(|item| id_only(item.open.subject.as_str()))
                .collect();
            NeedsYou {
                conditions,
                severity: head.map_or(Severity::Amber, |open| open.severity),
                remedy_key: remedy,
                params: head.map_or_else(ConditionParams::new, |open| safe_params(&open.params)),
                since: by_age
                    .first()
                    .map_or(Millis::ZERO, |item| item.open.raised_at_millis),
                count,
                more: count.saturating_sub(sample.len()),
                subjects_sample: sample,
                link: route,
            }
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .severity
            .cmp(&left.severity)
            .then_with(|| left.since.cmp(&right.since))
            .then_with(|| left.remedy_key.cmp(&right.remedy_key))
            .then_with(|| left.link.cmp(&right.link))
    });
    rows
}

/// Cap `rows` at [`MAX_ROWS`]; true when rows were dropped.
pub fn cap(rows: &mut Vec<NeedsYou>) -> bool {
    if rows.len() > MAX_ROWS {
        rows.truncate(MAX_ROWS);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_drops_rows_past_the_limit_and_says_so() {
        let row = NeedsYou {
            conditions: vec!["X".to_owned()],
            severity: Severity::Red,
            remedy_key: "k".to_owned(),
            params: ConditionParams::new(),
            since: Millis::ZERO,
            count: 1,
            subjects_sample: vec![],
            more: 1,
            link: Route::Node,
        };
        let mut rows = vec![row.clone(); MAX_ROWS + 1];
        assert!(cap(&mut rows));
        assert_eq!(rows.len(), MAX_ROWS);
        let mut exact = vec![row; MAX_ROWS];
        assert!(!cap(&mut exact));
    }

    #[test]
    fn only_id_like_text_is_shown() {
        assert_eq!(id_only("node:12"), "node:12");
        assert_eq!(id_only("Invoice 4411 for Acme"), REDACTED);
        assert_eq!(id_only(""), REDACTED);
        assert_eq!(id_only(&"a".repeat(129)), REDACTED);
    }
}
