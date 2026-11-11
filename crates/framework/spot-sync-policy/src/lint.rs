//! The policy lints: every problem at once, before runtime (ADR-062 rule 3).
//!
//! Inconsistent sync policy is rejected when the metadata is saved or the app installed, never
//! discovered in production. [`lint`] checks a resolved [`PolicyTable`] against the effective
//! metadata and returns **all** problems, not the first. [`lint_change`] adds the one check that
//! needs history: a policy change that would strand records already queued for delivery.
//!
//! The checks:
//!
//! | Code | Rejects |
//! | --- | --- |
//! | `sync_policy.closed_link` | a synced DocType that links — directly or through a child table — to a `local` DocType |
//! | `sync_policy.direction_conflict` | a direction that contradicts the class |
//! | `sync_policy.owner_scope_missing` | a policy that flows upward and names no `owner_scope` |
//! | `sync_policy.excluded_field` | an excluded field that is required, is a link, or is not a field |
//! | `sync_policy.orphan_queued` | a change that would leave queued records undeliverable |

use std::collections::BTreeSet;

use spot_app_api::{AppError, ErrorClass};
use spot_meta::{Definition, EffectiveMeta, FieldType};

use crate::error::codes;
use crate::policy::{Class, Direction, SyncPolicy};
use crate::table::PolicyTable;

/// One policy problem: the code, the DocType it is about, and a plain-language detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyProblem {
    code: &'static str,
    doctype: String,
    detail: String,
}

impl PolicyProblem {
    /// A problem with the given code, DocType and detail.
    #[must_use]
    pub fn new(code: &'static str, doctype: &str, detail: impl Into<String>) -> Self {
        Self {
            code,
            doctype: doctype.to_owned(),
            detail: detail.into(),
        }
    }

    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The DocType the problem is about.
    #[must_use]
    pub fn doctype(&self) -> &str {
        &self.doctype
    }

    /// The plain-language detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The full message: the code, the DocType and the detail.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{}: `{}`: {}", self.code, self.doctype, self.detail)
    }

    /// The platform error. A change that orphans queued records is [`ErrorClass::InvalidState`];
    /// every other problem is [`ErrorClass::Validation`].
    #[must_use]
    pub fn to_error(&self) -> AppError {
        let class = if self.code == codes::ORPHAN_QUEUED {
            ErrorClass::InvalidState
        } else {
            ErrorClass::Validation
        };
        AppError::new(class, self.code, self.message())
    }
}

/// Records already queued for delivery, by DocType. Time and volume are the caller's; this crate
/// reads no clock and counts nothing itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueuedRecords {
    counts: std::collections::BTreeMap<String, u64>,
}

impl QueuedRecords {
    /// No queued records.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the count queued for one DocType (replacing any earlier count).
    pub fn record(&mut self, doctype: &str, count: u64) {
        self.counts.insert(doctype.to_owned(), count);
    }

    /// A set with one DocType's count, for tests and one-line callers.
    #[must_use]
    pub fn with(doctype: &str, count: u64) -> Self {
        let mut records = Self::new();
        records.record(doctype, count);
        records
    }

    /// How many records are queued for a DocType.
    #[must_use]
    pub fn count(&self, doctype: &str) -> u64 {
        self.counts.get(doctype).copied().unwrap_or(0)
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.values().all(|&c| c == 0)
    }

    /// Every DocType with a queued count, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.counts.iter().map(|(k, &v)| (k.as_str(), v))
    }
}

/// Every policy problem in `table`, checked against `meta`, all at once.
#[must_use]
pub fn lint(table: &PolicyTable, meta: &EffectiveMeta) -> Vec<PolicyProblem> {
    let mut problems = Vec::new();
    for definition in meta.definitions() {
        let policy = table.policy(&definition.name);

        if let Some(reason) = direction_conflict(policy) {
            problems.push(PolicyProblem::new(
                codes::DIRECTION_CONFLICT,
                &definition.name,
                format!(
                    "class `{}` with direction `{}` {reason}",
                    policy.class, policy.direction
                ),
            ));
        }

        if policy.direction.has_up() && policy.owner_scope.is_none() {
            problems.push(PolicyProblem::new(
                codes::OWNER_SCOPE_MISSING,
                &definition.name,
                format!(
                    "direction `{}` flows upward, so an `owner_scope` key type is required to \
                     check ownership",
                    policy.direction
                ),
            ));
        }

        lint_excluded_fields(&definition.name, definition, policy, &mut problems);

        if policy.is_synced() {
            let mut visited = BTreeSet::new();
            lint_closed_links(
                &definition.name,
                definition,
                table,
                meta,
                &mut visited,
                &mut problems,
            );
        }
    }
    problems
}

/// The change-only lint: a policy change that would leave records already queued for delivery
/// with nowhere to go.
#[must_use]
pub fn lint_change(
    previous: &PolicyTable,
    current: &PolicyTable,
    queued: &QueuedRecords,
) -> Vec<PolicyProblem> {
    let mut problems = Vec::new();
    for (doctype, count) in queued.iter() {
        if count == 0 {
            continue;
        }
        let before = previous.policy(doctype);
        let after = current.policy(doctype);
        let lost_up = before.direction.has_up() && !after.direction.has_up();
        let lost_down = before.direction.has_down() && !after.direction.has_down();
        let stopped = before.is_synced() && !after.is_synced();
        if stopped || lost_up || lost_down {
            problems.push(PolicyProblem::new(
                codes::ORPHAN_QUEUED,
                doctype,
                format!(
                    "{count} record(s) are queued but the change moves direction from `{}` to \
                     `{}` (class `{}` to `{}`), so they can no longer be delivered",
                    before.direction, after.direction, before.class, after.class
                ),
            ));
        }
    }
    problems
}

/// Whether the direction contradicts the class, with a plain reason.
fn direction_conflict(policy: &SyncPolicy) -> Option<&'static str> {
    match policy.class {
        Class::Local => (policy.direction != Direction::None)
            .then_some("sends records, but a `local` DocType syncs nothing"),
        Class::Master => policy
            .direction
            .has_up()
            .then_some("flows upward, but master data is hub-owned and flows down"),
        Class::Transactional => policy
            .direction
            .has_down()
            .then_some("flows downward, but a transaction happens at a node and flows up"),
        Class::Shared => None,
    }
    .or_else(|| {
        (policy.class.is_synced() && policy.direction == Direction::None)
            .then_some("syncs nothing, but only `local` may carry direction `none`")
    })
}

/// An excluded field must not be required, must not be a link, and must exist.
fn lint_excluded_fields(
    doctype: &str,
    definition: &Definition,
    policy: &SyncPolicy,
    problems: &mut Vec<PolicyProblem>,
) {
    for field in policy.excluded() {
        let name = field.as_str();
        match definition.fields.iter().find(|f| f.name == name) {
            None => problems.push(PolicyProblem::new(
                codes::EXCLUDED_FIELD,
                doctype,
                format!("excluded field `{name}` is not a field of this DocType"),
            )),
            Some(spec) if spec.required => problems.push(PolicyProblem::new(
                codes::EXCLUDED_FIELD,
                doctype,
                format!(
                    "excluded field `{name}` is required; the destination stores full documents \
                     and needs a value for every declared field"
                ),
            )),
            Some(spec) if is_link(spec.ty) => problems.push(PolicyProblem::new(
                codes::EXCLUDED_FIELD,
                doctype,
                format!("excluded field `{name}` is a link; a destination could not resolve it"),
            )),
            Some(_) => {}
        }
    }
}

/// Walks a synced DocType's links, recursing through child tables, and reports a link to a
/// `local` DocType. `visited` guards against a cycle through child tables.
fn lint_closed_links(
    root: &str,
    definition: &Definition,
    table: &PolicyTable,
    meta: &EffectiveMeta,
    visited: &mut BTreeSet<String>,
    problems: &mut Vec<PolicyProblem>,
) {
    for field in &definition.fields {
        let Some(target) = field.options.as_deref() else {
            continue;
        };
        match field.ty {
            FieldType::Link => {
                if meta.definition(target).is_some() && table.policy(target).class == Class::Local {
                    problems.push(PolicyProblem::new(
                        codes::CLOSED_LINK,
                        root,
                        format!(
                            "field `{}` links to `{target}`, which is `local`; the destination \
                             could never resolve it",
                            field.name
                        ),
                    ));
                }
            }
            FieldType::Table | FieldType::TableMultiselect => {
                if table.policy(target).class == Class::Local {
                    problems.push(PolicyProblem::new(
                        codes::CLOSED_LINK,
                        root,
                        format!(
                            "child table `{}` holds rows of `{target}`, which is `local`; the \
                             destination could never resolve it",
                            field.name
                        ),
                    ));
                }
                if visited.insert(target.to_owned())
                    && let Some(child) = meta.definition(target)
                {
                    lint_closed_links(root, child, table, meta, visited, problems);
                }
            }
            _ => {}
        }
    }
}

/// Whether a field type is a link (a reference to another DocType, direct or per record, or a
/// child table of one).
fn is_link(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::Link | FieldType::DynamicLink | FieldType::Table | FieldType::TableMultiselect
    )
}
