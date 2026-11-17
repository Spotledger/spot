//! The list path through the field guard (WP-SEC-04.T5, ADR-070 section 12 rule 6).
//!
//! A column of a list view is a channel of its own ([`Channel::ListColumn`]). It needs the
//! `report` right, which is **not** the `read` right the document load path checks: a caller who
//! may read a protected field of one document may still not see it as a column of a list.
//! [`ListFieldGuard`] makes the list path ask the guard twice:
//!
//! 1. **Before the read**, on every field the filter and the order keys touch
//!    ([`FieldGuard::check_query_fields_for`], the no-inference rule). A refused field answers
//!    exactly as an unknown field does (`document.unknown_field`), so the answer says nothing
//!    about whether the field exists, is protected, or what it holds.
//! 2. **After the load**, on every row ([`FieldGuard::redact`] with `Channel::ListColumn`): a
//!    hidden column is removed, a masked column is masked, and a guard error empties the row's
//!    columns (the id stays), so a failure can never widen what leaves.
//!
//! Derived and link-title columns inherit the strictest classification of their inputs because
//! the protection facts of a derived field carry every group of the fields it derives from
//! (`spot_field_guard::lint`); a link column holds the target's id only, its title is the lookup
//! endpoint's channel (`Channel::LinkTitle`).
//!
//! The guard is built where the services are assembled and handed to
//! [`EngineResources::with_field_guard`](crate::EngineResources::with_field_guard); there is no
//! global.

use std::sync::Arc;

use spot_app_api::Document;
use spot_app_api::context::{Actor, Invocation};
use spot_field_guard::{Channel, FieldGuard, QueryField, QueryUse};
use spot_http::api::filter::FilterAst;
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, QuerySpec};
use spot_permissions::Principal;

use crate::access::{PrincipalSource, doc_facts};

/// The field guard and the source of principals the list path consults.
#[derive(Clone)]
pub struct ListFieldGuard {
    guard: Arc<FieldGuard>,
    principals: Arc<dyn PrincipalSource>,
}

impl std::fmt::Debug for ListFieldGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListFieldGuard").finish_non_exhaustive()
    }
}

impl ListFieldGuard {
    /// A list guard over `guard`, turning actors into principals through `principals`.
    #[must_use]
    pub fn new(guard: Arc<FieldGuard>, principals: Arc<dyn PrincipalSource>) -> Self {
        Self { guard, principals }
    }

    /// The principal acting on `inv` (no error: an unknown user has no rights).
    pub(crate) async fn principal(&self, inv: &Invocation) -> Principal {
        match &inv.actor {
            Actor::System { .. } => Principal::system(),
            Actor::User { id, .. } => self.principals.resolve(id).await,
            Actor::Consumer { .. } => Principal::user(&inv.actor.principal_text(), Vec::new()),
        }
    }

    /// The no-inference check, for `channel` (`ListColumn` or `LinkTitle`), on the filter and the order keys of `spec`. A refused field is
    /// answered as an unknown field; any other guard failure is an internal error (nothing is
    /// read either way).
    pub(crate) fn check_query(
        &self,
        channel: Channel,
        principal: &Principal,
        doctype: &str,
        spec: &QuerySpec,
    ) -> Result<(), ApiError> {
        let used = used_fields(spec);
        if used.is_empty() {
            return Ok(());
        }
        match self
            .guard
            .check_query_fields_for(channel, principal, doctype, &used)
        {
            Ok(()) => Ok(()),
            Err(error) if error.code() == "permission.field_denied" => {
                Err(ApiError::from_code(Code::DocumentUnknownField))
            }
            Err(_) => Err(ApiError::from_code(Code::HttpInternal)),
        }
    }

    /// `doc` as `channel` lets `principal` see it: hidden columns removed, masked
    /// columns masked. On a guard error the guard has already emptied the document's columns.
    pub(crate) fn redact(
        &self,
        channel: Channel,
        principal: &Principal,
        doctype: &str,
        mut doc: Document,
    ) -> Document {
        let facts = doc_facts(&doc, Some(&doc.id().to_string()));
        // The error is deliberately not surfaced: `redact` leaves an emptied document on
        // failure, which is the fail-closed result, and the row still counts in the page.
        let _outcome = self
            .guard
            .redact(channel, principal, doctype, &facts, &mut doc);
        doc
    }
}

/// Every field the filter and the order keys of `spec` touch (`id` is never protected).
fn used_fields(spec: &QuerySpec) -> Vec<QueryField> {
    let mut out = Vec::new();
    if let Some(filter) = &spec.filter {
        collect(filter, &mut out);
    }
    for key in &spec.order {
        if key.field.as_str() != "id" {
            out.push(QueryField::new(key.field.as_str(), QueryUse::Sort));
        }
    }
    out
}

fn collect(ast: &FilterAst, out: &mut Vec<QueryField>) {
    match ast {
        FilterAst::And(nodes) | FilterAst::Or(nodes) => {
            for node in nodes {
                collect(node, out);
            }
        }
        FilterAst::Not(node) => collect(node, out),
        FilterAst::Cmp(cmp) => {
            if cmp.field.as_str() != "id" {
                out.push(QueryField::new(cmp.field.as_str(), QueryUse::Filter));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_http::api::filter::{Comparison, FilterValue, Op, OrderKey};
    use spot_storage_contract::{Name, OrderDir};

    fn cmp(field: &str) -> FilterAst {
        FilterAst::Cmp(Comparison {
            field: Name::new(field).unwrap(),
            op: Op::Eq,
            values: vec![FilterValue::Text("x".into())],
            case_insensitive: false,
        })
    }

    #[test]
    fn used_fields_walks_the_tree_and_skips_id() {
        let spec = QuerySpec {
            fields: Vec::new(),
            filter: Some(FilterAst::And(vec![
                cmp("a"),
                FilterAst::Not(Box::new(cmp("id"))),
                FilterAst::Or(vec![cmp("b"), cmp("c")]),
            ])),
            order: vec![
                OrderKey {
                    field: Name::new("d").unwrap(),
                    dir: OrderDir::Asc,
                    nulls: None,
                },
                OrderKey {
                    field: Name::new("id").unwrap(),
                    dir: OrderDir::Asc,
                    nulls: None,
                },
            ],
            limit: 20,
            cursor: None,
            with_total: false,
        };
        let paths: Vec<String> = used_fields(&spec).into_iter().map(|f| f.path).collect();
        assert_eq!(paths, ["a", "b", "c", "d"]);
    }
}
