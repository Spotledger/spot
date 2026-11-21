//! Draft ownership (ADR-080 section 2.2): while a draft is owned by a running request it can be
//! changed only through that request's steps.
//!
//! The draft carries the field [`OWNER_FIELD`], holding the id of the owning request. The
//! [`OwnershipController`] runs at the `before_*` hook of every operation that changes the
//! record, so the rule holds in every channel (the desk, the API, an import, a script), and it
//! reads the **stored** flag, never the one the caller sent, so a caller cannot free a draft by
//! sending an empty flag or take one by sending somebody else's request id.
//!
//! * A change from another channel fails with [`EntryError::OwnedByRequest`], which names the
//!   owning request so the caller can open it.
//! * A document step of the owning request that submits the draft clears the flag as part of
//!   the same operation.
//! * When the request is cancelled or undone, [`abort_drafts`] discards the draft or releases it
//!   as an ordinary draft, per the step's `on_abort`.

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::lifecycle::{HookPoint, Operation, Phase, Target};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, DocumentPatch, HookContext, StepDecl, StepName,
};
use spot_bpm_governance::actor_text;

use crate::guard::RequestAuthority;

use crate::error::{EntryError, EntryResult};

/// The reserved field of a draft that holds the id of the request that owns it. Empty or null
/// means no request owns the record.
pub const OWNER_FIELD: &str = "owned_by_request";

/// What happens to an owned draft when its request is cancelled or undone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortPolicy {
    /// Delete the draft: it never existed as far as other channels are concerned.
    #[default]
    Discard,
    /// Keep the draft as an ordinary draft anyone with the right may continue.
    KeepAsDraft,
}

impl AbortPolicy {
    /// The token a step's `on_abort` setting uses.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Discard => "discard",
            Self::KeepAsDraft => "keep_as_draft",
        }
    }

    /// Parses the token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "discard" => Some(Self::Discard),
            "keep_as_draft" => Some(Self::KeepAsDraft),
            _ => None,
        }
    }
}

const OPERATIONS: [(Operation, &str); 5] = [
    (Operation::Create, "bpm:ownership_create"),
    (Operation::Update, "bpm:ownership_update"),
    (Operation::Post, "bpm:ownership_post"),
    (Operation::Cancel, "bpm:ownership_cancel"),
    (Operation::Delete, "bpm:ownership_delete"),
];

/// The hook that keeps an owned draft inside its request.
pub struct OwnershipController {
    doctype: String,
    authority: Arc<dyn RequestAuthority>,
}

impl OwnershipController {
    /// A controller for `doctype`. The request a change comes from is whatever the authority
    /// verifies for the acting person and the current attempt; the command id by itself proves
    /// nothing, so a copied request id or a replayed old grant is another channel.
    #[must_use]
    pub fn new(doctype: &str, authority: Arc<dyn RequestAuthority>) -> Self {
        Self {
            doctype: doctype.to_owned(),
            authority,
        }
    }

    async fn decide(&self, ctx: &mut dyn HookContext, op: Operation) -> EntryResult<()> {
        let request = {
            let inv = ctx.invocation();
            inv.command_id
                .as_ref()
                .and_then(|key| {
                    self.authority
                        .authenticate(key.as_str(), &actor_text(&inv.actor))
                })
                .map(|step| step.request)
        };
        let incoming = flag_of(ctx.doc());
        let stored = if op == Operation::Create {
            None
        } else {
            let id = ctx.doc().id();
            match ctx.load(&self.doctype, id).await {
                Ok(doc) => flag_of(&doc),
                // A record that cannot be loaded here is the lifecycle's to report.
                Err(_) => None,
            }
        };
        // The stored flag decides. For a create there is nothing stored, so the flag the caller
        // sends is a claim, and only the claimed request itself may make it.
        let claimed = stored.clone().or(incoming);
        let Some(owner) = claimed else {
            return Ok(());
        };
        if request.as_deref() != Some(owner.as_str()) {
            return Err(EntryError::OwnedByRequest {
                doctype: self.doctype.clone(),
                request: owner,
            });
        }
        // The owning request submits the draft: the flag clears with the same operation.
        if op == Operation::Post && stored.is_some() {
            let doc = ctx.doc_mut().map_err(|error| EntryError::DraftFailed {
                detail: error.message.clone(),
            })?;
            let field = owner_field()?;
            doc.set(field, Value::Text(String::new()));
        }
        Ok(())
    }
}

fn owner_field() -> EntryResult<Name> {
    Name::new(OWNER_FIELD).map_err(|_| EntryError::ConfigInvalid {
        node: OWNER_FIELD.to_owned(),
        detail: "not a valid field name".to_owned(),
    })
}

fn flag_of(doc: &spot_app_api::Document) -> Option<String> {
    doc.text(OWNER_FIELD)
        .ok()
        .flatten()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

impl Controller for OwnershipController {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        OPERATIONS
            .iter()
            .filter_map(|(op, name)| {
                Some(StepDecl {
                    name: StepName::new(name).ok()?,
                    point: HookPoint::of(Phase::Before, Target::Op(*op)),
                    priority: 90,
                    budget: Duration::from_secs(5),
                })
            })
            .collect()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let Some((op, _)) = OPERATIONS
                .iter()
                .find(|(op, name)| {
                    *name == step.as_str()
                        && ctx.point() == &HookPoint::of(Phase::Before, Target::Op(*op))
                })
                .copied()
            else {
                return Ok(());
            };
            self.decide(ctx, op)
                .await
                .map_err(|error| error.to_app_error())
        })
    }
}

/// A draft owned by a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftRef {
    /// The record type.
    pub doctype: String,
    /// The record.
    pub id: RecordId,
    /// The version the caller last saw.
    pub version: Version,
}

/// The request an abort acts for: it supplies the context the ownership rule recognises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestContext {
    /// The request id.
    pub request: String,
    /// The node that is aborting.
    pub node: String,
    /// The attempt number.
    pub attempt: u32,
    /// The grant the request authority issued for this step, actor and attempt. It is the
    /// command id the lifecycle sees.
    pub token: String,
}

impl RequestContext {
    /// The command id that carries this context into the lifecycle: the authority's grant.
    #[must_use]
    pub fn command_id(&self) -> String {
        self.token.clone()
    }
}

/// Changes drafts through their lifecycle on behalf of a request that is ending.
pub trait DraftLifecycle: Send + Sync {
    /// Deletes the draft.
    fn discard<'a>(
        &'a self,
        draft: &'a DraftRef,
        ctx: &'a RequestContext,
        now: OffsetDateTime,
    ) -> BoxFuture<'a, EntryResult<()>>;

    /// Clears the owner flag so the draft is an ordinary draft.
    fn release<'a>(
        &'a self,
        draft: &'a DraftRef,
        ctx: &'a RequestContext,
        now: OffsetDateTime,
    ) -> BoxFuture<'a, EntryResult<()>>;
}

/// [`DraftLifecycle`] over the document engine's public service: every change runs the record's
/// own lifecycle, as the person the request acts for.
pub struct DocumentServiceDrafts {
    service: Arc<dyn DocumentService>,
    site: SiteId,
    actor: spot_app_api::context::Actor,
}

impl DocumentServiceDrafts {
    /// A lifecycle over `service`, acting as `actor` on `site`.
    #[must_use]
    pub fn new(
        service: Arc<dyn DocumentService>,
        site: SiteId,
        actor: spot_app_api::context::Actor,
    ) -> Self {
        Self {
            service,
            site,
            actor,
        }
    }

    fn invocation(&self, ctx: &RequestContext, now: OffsetDateTime) -> EntryResult<Invocation> {
        let command_id =
            CommandId::new(&ctx.command_id()).map_err(|_| EntryError::ConfigInvalid {
                node: ctx.node.clone(),
                detail: "the request context is not a valid command id".to_owned(),
            })?;
        Ok(Invocation {
            site: self.site.clone(),
            actor: self.actor.clone(),
            command_id: Some(command_id),
            language: None,
            now,
        })
    }
}

fn draft_failed(error: &AppError) -> EntryError {
    EntryError::DraftFailed {
        detail: error.message.clone(),
    }
}

impl DraftLifecycle for DocumentServiceDrafts {
    fn discard<'a>(
        &'a self,
        draft: &'a DraftRef,
        ctx: &'a RequestContext,
        now: OffsetDateTime,
    ) -> BoxFuture<'a, EntryResult<()>> {
        Box::pin(async move {
            let inv = self.invocation(ctx, now)?;
            self.service
                .delete(&inv, &draft.doctype, draft.id, draft.version)
                .await
                .map(|_| ())
                .map_err(|error| draft_failed(&error))
        })
    }

    fn release<'a>(
        &'a self,
        draft: &'a DraftRef,
        ctx: &'a RequestContext,
        now: OffsetDateTime,
    ) -> BoxFuture<'a, EntryResult<()>> {
        Box::pin(async move {
            let inv = self.invocation(ctx, now)?;
            let mut patch = DocumentPatch::default();
            patch
                .fields
                .insert(owner_field()?, Value::Text(String::new()));
            self.service
                .update(&inv, &draft.doctype, draft.id, patch, draft.version)
                .await
                .map(|_| ())
                .map_err(|error| draft_failed(&error))
        })
    }
}

/// What an abort did to each draft.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AbortReport {
    /// Drafts deleted.
    pub discarded: Vec<RecordId>,
    /// Drafts released as ordinary drafts.
    pub released: Vec<RecordId>,
    /// Drafts the lifecycle refused to change, with the reason; the request's undo reports them
    /// as blockers instead of forcing anything.
    pub blocked: Vec<(RecordId, EntryError)>,
}

/// Ends the ownership of `drafts` per `policy`, through their lifecycle. A draft the lifecycle
/// refuses is reported and the rest are still handled.
pub async fn abort_drafts(
    lifecycle: &dyn DraftLifecycle,
    policy: AbortPolicy,
    drafts: &[DraftRef],
    ctx: &RequestContext,
    now: OffsetDateTime,
) -> AbortReport {
    let mut report = AbortReport::default();
    for draft in drafts {
        let outcome = match policy {
            AbortPolicy::Discard => lifecycle.discard(draft, ctx, now).await,
            AbortPolicy::KeepAsDraft => lifecycle.release(draft, ctx, now).await,
        };
        match (outcome, policy) {
            (Ok(()), AbortPolicy::Discard) => report.discarded.push(draft.id),
            (Ok(()), AbortPolicy::KeepAsDraft) => report.released.push(draft.id),
            (Err(error), _) => report.blocked.push((draft.id, error)),
        }
    }
    report
}
