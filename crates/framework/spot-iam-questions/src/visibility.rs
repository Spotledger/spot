//! Who can see one document: the third answerable question, built on the permission query
//! machinery (`spot-perm-query`) and the share records, with the field-level groups a viewer
//! may read (ADR-070 sections 8.5 and 12).
//!
//! For each candidate the read filter is compiled from the decision core's snapshot and
//! evaluated against the document (`PermissionFilter::for_read`), so "who can see this" is the
//! same rule a list query would apply — not a second implementation. Shares add a reason of
//! their own. The answer names fields and levels only, never a value.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_permissions::{Action, FieldFacts, FieldLevel, Principal};

use crate::engine::{World, user_can_read};
use crate::error::QuestionsError;
use crate::explain::{Holder, holders_for_document};
use crate::model::{AccessModel, DocumentRecord};

/// How one field reads to one viewer: its protection groups and the level the decision core
/// decided, with no value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct FieldVisibility {
    /// The field path.
    pub field: String,
    /// The protection groups the field belongs to.
    pub groups: Vec<String>,
    /// `hidden`, `masked`, `read` or `read_reveal`.
    pub level: String,
    /// May set it at creation.
    pub can_create: bool,
    /// May change it.
    pub can_update: bool,
    /// May use it in reports and lists.
    pub can_report: bool,
    /// May reveal the unmasked value, one document at a time.
    pub can_reveal: bool,
}

/// One user who can see the document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Viewer {
    /// The user.
    pub user: String,
    /// Whether a share on this document granted the read.
    pub shared: bool,
    /// The assignments that let the user read it, with their why-chain.
    pub via: Vec<Holder>,
    /// Every protected field, with the level this viewer gets; name and level only.
    pub fields: Vec<FieldVisibility>,
}

/// Every user who can read `doc`, sorted by user.
///
/// The candidates are the users with an active assignment, the users a share names and the
/// document's creator: a user who can read without a role assignment (a share, the tenant
/// administrator) is not missed.
///
/// # Errors
///
/// [`QuestionsError::ModelInvalid`] when the query machinery refuses the read filter.
pub fn viewers(
    model: &dyn AccessModel,
    world: &World,
    doc: &DocumentRecord,
) -> Result<Vec<Viewer>, QuestionsError> {
    let assignments = model.assignments();
    let mut candidates: BTreeSet<String> = world.users().into_iter().collect();
    if let Some(id) = doc.id {
        for share in model.shares() {
            if share.doc == id {
                candidates.insert(share.user.clone());
            }
        }
    }
    if let Some(creator) = &doc.created_by {
        candidates.insert(creator.clone());
    }
    let protected = protected_fields(world, &doc.doctype);
    let all_holders = holders_for_document(world, assignments, Action::Read, doc);
    let mut out = Vec::new();
    for user in candidates {
        if !user_can_read(world, &user, doc)? {
            continue;
        }
        let principal = world.principal(&user);
        let via = all_holders
            .iter()
            .filter(|h| h.user == user)
            .cloned()
            .collect::<Vec<_>>();
        let shared = model.shares().iter().any(|s| {
            s.user == user
                && Some(s.doc) == doc.id
                && s.rights.contains(&spot_permissions::ShareRight::Read)
        });
        let fields = protected
            .iter()
            .map(|f| field_visibility(world, &principal, &doc.doctype, f, doc))
            .collect();
        out.push(Viewer {
            user,
            shared,
            via,
            fields,
        });
    }
    out.sort_by(|a, b| a.user.cmp(&b.user));
    Ok(out)
}

/// The protected fields of one DocType, from the engine's snapshot facts.
fn protected_fields(world: &World, doctype: &str) -> Vec<FieldFacts> {
    world
        .engine
        .snapshot()
        .facts(doctype)
        .map(|facts| {
            facts
                .fields
                .iter()
                .filter(|f| !f.groups.is_empty())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn field_visibility(
    world: &World,
    principal: &Principal,
    doctype: &str,
    field: &FieldFacts,
    doc: &DocumentRecord,
) -> FieldVisibility {
    let decision = world
        .engine
        .field_access(principal, doctype, field, &doc.facts());
    FieldVisibility {
        field: field.path.clone(),
        groups: field.groups.iter().map(|g| g.as_str().to_owned()).collect(),
        level: level_token(decision.level).to_owned(),
        can_create: decision.can_create,
        can_update: decision.can_update,
        can_report: decision.can_report,
        can_reveal: decision.can_reveal,
    }
}

fn level_token(level: FieldLevel) -> &'static str {
    match level {
        FieldLevel::Hidden => "hidden",
        FieldLevel::Masked(_) => "masked",
        FieldLevel::Read => "read",
        FieldLevel::ReadReveal => "read_reveal",
    }
}
