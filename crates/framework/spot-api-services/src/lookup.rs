//! The platform link lookup behind the field guard (WP-SEC-04.T6, ADR-070 section 12 rule 6).
//!
//! [`EngineLookup`] implements `spot-http`'s `LookupService` for the platform default seam
//! (`platform:link_search`) over the document engine: the candidates of a link field are the
//! documents of its target DocType, labelled with the target's title field. A title is a value
//! that leaves the platform, so it is a channel ([`Channel::LinkTitle`], needs `read` or
//! `read_masked`):
//!
//! - the **label** is built only from what the guard lets the caller see of the target
//!   document: a masked title is masked, and a title the caller may not read falls back to the
//!   record id, never to a value of a protected field;
//! - the **text typed** in the link box filters on the title field only if the caller may use
//!   that field unmasked (`check_query_fields_for`); otherwise it answers as if the DocType had
//!   no title field, so the page carries no hint of the field's value, length or presence;
//! - a field that names another seam is not served here (`lookup.seam_failed`, never a silent
//!   fallback to the default, ADR-068 section 3.8).

use std::sync::Arc;

use spot_app_api::Document;
use spot_app_api::context::DocumentService;
use spot_app_api::lookup::PLATFORM_LINK_SEARCH;
use spot_app_api::meta::FieldKind;
use spot_app_api::prelude::{Clock, Value};
use spot_field_guard::Channel;
use spot_http::api::filter::{Comparison, FilterAst, FilterValue, Op, OrderKey};
use spot_http::api::lookup::{LookupPage, LookupRequest, LookupRow};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, LookupService, QuerySpec, RequestCtx};
use spot_meta::EffectiveMeta;
use spot_storage_contract::OrderDir;

use crate::engine_resources::ActorResolver;
use crate::list_guard::ListFieldGuard;
use crate::query::{QueryService, list_documents};
use crate::util::build_invocation;

/// Candidates per page.
const PAGE: u32 = 20;

/// The platform link lookup over the engine, with the guard on the title.
pub struct EngineLookup<S: DocumentService> {
    engine: Arc<S>,
    meta: Arc<EffectiveMeta>,
    clock: Arc<dyn Clock>,
    actor: Arc<ActorResolver>,
    guard: ListFieldGuard,
}

impl<S: DocumentService> EngineLookup<S> {
    /// A lookup over `engine`. The guard is required: there is no unguarded form of this
    /// service, because a title leaves the platform through it.
    pub fn new(
        engine: Arc<S>,
        meta: Arc<EffectiveMeta>,
        clock: Arc<dyn Clock>,
        actor: Arc<ActorResolver>,
        guard: ListFieldGuard,
    ) -> Self {
        Self {
            engine,
            meta,
            clock,
            actor,
            guard,
        }
    }
}

fn title_of(doc: &Document, title_field: Option<&str>) -> String {
    if let Some(field) = title_field
        && let Value::Text(text) = doc.get(field)
        && !text.is_empty()
    {
        return text.clone();
    }
    // The id label: the record's name, never a value of any field.
    doc.id().to_string()
}

impl<S: DocumentService + QueryService + 'static> LookupService for EngineLookup<S> {
    fn search<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        field: &'a str,
        request: &'a LookupRequest,
    ) -> BoxFuture<'a, Result<LookupPage, ApiError>> {
        Box::pin(async move {
            let inv = build_invocation(ctx, self.clock.as_ref(), (self.actor)(ctx)?)?;
            let def = self.meta.doctype(doctype).ok_or_else(|| {
                ApiError::new(
                    Code::MetaUnknownDoctype,
                    format!("The DocType `{doctype}` does not exist."),
                )
            })?;
            let link = def
                .fields
                .iter()
                .find(|f| f.name.as_str() == field)
                .ok_or_else(|| ApiError::from_code(Code::DocumentUnknownField))?;
            let FieldKind::Link { doctype: target } = &link.kind else {
                return Err(ApiError::from_code(Code::ApiInvalidRequest));
            };
            if let Some(spec) = &link.lookup
                && spec
                    .seam
                    .as_deref()
                    .is_some_and(|s| s != PLATFORM_LINK_SEARCH)
            {
                return Err(ApiError::from_code(Code::LookupSeamFailed));
            }
            let target = target.as_str();
            let target_def = self.meta.doctype(target).ok_or_else(|| {
                ApiError::new(
                    Code::MetaUnknownDoctype,
                    format!("The DocType `{target}` does not exist."),
                )
            })?;
            let principal = self.guard.principal(&inv).await;

            // The title field is used to filter and order only when the caller may use it.
            let mut spec = QuerySpec {
                fields: Vec::new(),
                filter: None,
                order: Vec::new(),
                limit: PAGE,
                cursor: request.cursor.clone(),
                with_total: false,
            };
            let mut title_usable = false;
            if let Some(title) = &target_def.title_field {
                let mut probe = spec.clone();
                probe.order = vec![OrderKey {
                    field: title.clone(),
                    dir: OrderDir::Asc,
                    nulls: None,
                }];
                title_usable = self
                    .guard
                    .check_query(Channel::LinkTitle, &principal, target, &probe)
                    .is_ok();
                if title_usable {
                    spec.order = probe.order;
                    if !request.text.is_empty() {
                        spec.filter = Some(FilterAst::Cmp(Comparison {
                            field: title.clone(),
                            op: Op::Contains,
                            values: vec![FilterValue::Text(request.text.clone())],
                            case_insensitive: true,
                        }));
                    }
                }
            }
            if !title_usable && !request.text.is_empty() {
                // As if the DocType had no title to match: nothing matches, and the empty page
                // does not say why.
                return Ok(LookupPage::default());
            }

            let raw = list_documents(
                self.engine.as_ref(),
                &self.meta,
                &inv,
                target,
                &spec,
                Some((&self.guard, Channel::LinkTitle)),
            )
            .await?;
            let title_name = target_def.title_field.as_ref().map(|n| n.as_str());
            let mut rows = Vec::with_capacity(raw.documents.len());
            for loaded in raw.documents {
                let id = loaded.id();
                let seen = self
                    .guard
                    .redact(Channel::LinkTitle, &principal, target, loaded);
                rows.push(LookupRow {
                    id,
                    label: title_of(&seen, title_name),
                    description: None,
                    extras: Default::default(),
                    cells: Default::default(),
                });
            }
            Ok(LookupPage {
                rows,
                has_more: raw.next_cursor.is_some(),
                cursor: raw.next_cursor,
                columns: Vec::new(),
            })
        })
    }
}
