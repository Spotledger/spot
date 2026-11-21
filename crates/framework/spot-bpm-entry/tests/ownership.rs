//! Draft ownership at the document engine's hooks: while a request owns a draft it can be
//! changed only through that request's steps, in every channel.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::prelude::Value;
use spot_app_api::{Document, DocumentPatch};
use spot_bpm_entry::{AbortPolicy, DocumentServiceDrafts, DraftRef, RequestContext, abort_drafts};
use spot_document::engine::Engine;
use spot_storage_fake::FakeBackend;

type Eng = Arc<Engine<FakeBackend>>;

fn code(error: &spot_app_api::AppError) -> &str {
    &error.code
}

fn patch_name(name: &str) -> DocumentPatch {
    let mut patch = DocumentPatch::default();
    patch
        .fields
        .insert(n("full_name"), Value::Text(name.to_owned()));
    patch
}

/// An invocation carrying the grant the engine would issue the user for this step.
fn request_invocation(ports: &Ports, request: &str, node: &str) -> Invocation {
    let key = ports.grant(&user_actor(), request, node, 1);
    invocation(user_actor(), Some(&key))
}

/// Creates a draft owned by `request` (free mode, so only ownership is in play).
fn owned_draft(ports: &Ports, engine: &Eng, request: &str) -> Document {
    block_on(engine.create(
        &request_invocation(ports, request, "enter"),
        employee("acme", Some(request)),
    ))
    .unwrap()
    .value
}

#[test]
fn another_channel_cannot_change_an_owned_draft() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    let version = draft.version().unwrap();
    for actor in [user_actor(), import_actor(), script_actor()] {
        let error = block_on(engine.update(
            &invocation(actor, None),
            "employee",
            draft.id(),
            patch_name("Someone Else"),
            version,
        ))
        .expect_err("an owned draft is edited only through its request");
        assert_eq!(code(&error), "bpm_entry.owned_by_request");
        // The refusal names the request so the caller can open it.
        assert_eq!(
            error.details.get("request").and_then(|v| v.as_str()),
            Some("req1")
        );
    }
    // A different request is another channel too.
    let error = block_on(engine.update(
        &request_invocation(&ports, "req2", "enter"),
        "employee",
        draft.id(),
        patch_name("Hijack"),
        version,
    ))
    .expect_err("another request cannot take it");
    assert_eq!(code(&error), "bpm_entry.owned_by_request");
}

#[test]
fn the_owning_request_may_edit_its_draft() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    let updated = block_on(engine.update(
        &request_invocation(&ports, "req1", "enter"),
        "employee",
        draft.id(),
        patch_name("Amina R. Rauf"),
        draft.version().unwrap(),
    ))
    .unwrap()
    .value;
    assert_eq!(updated.text("full_name").unwrap(), Some("Amina R. Rauf"));
    assert_eq!(updated.text("owned_by_request").unwrap(), Some("req1"));
}

#[test]
fn a_caller_cannot_free_a_draft_by_sending_an_empty_flag() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    let mut patch = patch_name("Freed");
    patch
        .fields
        .insert(n("owned_by_request"), Value::Text(String::new()));
    let error = block_on(engine.update(
        &invocation(user_actor(), None),
        "employee",
        draft.id(),
        patch,
        draft.version().unwrap(),
    ))
    .expect_err("the stored flag decides, not the one the caller sends");
    assert_eq!(code(&error), "bpm_entry.owned_by_request");
}

#[test]
fn a_caller_cannot_claim_ownership_for_a_request_it_is_not() {
    let ports = Ports::new();
    let engine = ports.engine();
    let error = block_on(engine.create(
        &invocation(user_actor(), None),
        employee("acme", Some("req9")),
    ))
    .expect_err("a forged claim is refused");
    assert_eq!(code(&error), "bpm_entry.owned_by_request");
    // An unowned record is free in every channel.
    assert!(
        block_on(engine.create(&invocation(user_actor(), None), employee("acme", None))).is_ok()
    );
}

#[test]
fn the_owning_request_submits_the_draft_and_the_flag_clears() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    // Another channel cannot post it.
    let error = block_on(engine.post(
        &invocation(user_actor(), None),
        "employee",
        draft.id(),
        draft.version().unwrap(),
    ))
    .expect_err("owned");
    assert_eq!(code(&error), "bpm_entry.owned_by_request");
    // The request's document step posts it, and the flag clears with the operation.
    let posted = block_on(engine.post(
        &request_invocation(&ports, "req1", "submit"),
        "employee",
        draft.id(),
        draft.version().unwrap(),
    ))
    .unwrap()
    .value;
    assert_eq!(posted.text("owned_by_request").unwrap().unwrap_or(""), "");
    let stored = block_on(engine.peek("employee", draft.id()))
        .unwrap()
        .unwrap();
    assert_eq!(stored.text("owned_by_request").unwrap().unwrap_or(""), "");
}

fn drafts_service(engine: &Eng) -> DocumentServiceDrafts {
    let service: Arc<dyn DocumentService> = engine.clone();
    DocumentServiceDrafts::new(service, site(), user_actor())
}

fn context(ports: &Ports) -> RequestContext {
    RequestContext {
        request: "req1".to_owned(),
        node: "enter".to_owned(),
        attempt: 1,
        token: ports.grant(&user_actor(), "req1", "enter", 1),
    }
}

fn draft_ref(draft: &Document) -> DraftRef {
    DraftRef {
        doctype: "employee".to_owned(),
        id: draft.id(),
        version: draft.version().unwrap(),
    }
}

#[test]
fn cancelling_the_request_discards_the_draft_per_on_abort() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    let report = block_on(abort_drafts(
        &drafts_service(&engine),
        AbortPolicy::Discard,
        &[draft_ref(&draft)],
        &context(&ports),
        now(),
    ));
    assert_eq!(report.discarded, vec![draft.id()]);
    assert!(report.blocked.is_empty(), "{:?}", report.blocked);
    assert!(
        block_on(engine.peek("employee", draft.id()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn cancelling_the_request_can_keep_the_draft_as_an_ordinary_draft() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    let report = block_on(abort_drafts(
        &drafts_service(&engine),
        AbortPolicy::KeepAsDraft,
        &[draft_ref(&draft)],
        &context(&ports),
        now(),
    ));
    assert_eq!(report.released, vec![draft.id()]);
    // Now anyone with the right may continue it.
    let stored = block_on(engine.peek("employee", draft.id()))
        .unwrap()
        .unwrap();
    assert_eq!(stored.text("owned_by_request").unwrap().unwrap_or(""), "");
    let edited = block_on(engine.update(
        &invocation(user_actor(), None),
        "employee",
        draft.id(),
        patch_name("Continued"),
        stored.version().unwrap(),
    ));
    assert!(edited.is_ok(), "{edited:?}");
}

#[test]
fn an_abort_by_the_wrong_request_is_blocked_not_forced() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    let wrong = RequestContext {
        request: "req2".to_owned(),
        node: "enter".to_owned(),
        attempt: 1,
        token: ports.grant(&user_actor(), "req2", "enter", 1),
    };
    let report = block_on(abort_drafts(
        &drafts_service(&engine),
        AbortPolicy::Discard,
        &[draft_ref(&draft)],
        &wrong,
        now(),
    ));
    assert!(report.discarded.is_empty());
    assert_eq!(report.blocked.len(), 1);
    assert!(
        block_on(engine.peek("employee", draft.id()))
            .unwrap()
            .is_some()
    );
}

#[test]
fn abort_policy_tokens_round_trip() {
    for policy in [AbortPolicy::Discard, AbortPolicy::KeepAsDraft] {
        assert_eq!(AbortPolicy::parse(policy.token()), Some(policy));
    }
    assert_eq!(AbortPolicy::parse("burn"), None);
    assert_eq!(AbortPolicy::default(), AbortPolicy::Discard);
}

/// The request context is an authenticated grant: a copied owner id, a forged key and a
/// replayed old attempt are all "another channel".
#[test]
fn a_copied_owner_id_or_forged_key_does_not_pass_the_ownership_hook() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    let version = draft.version().unwrap();

    // The refusal names the owner, and the caller writes that id into a key of every old shape.
    for forged in [
        "req1:enter:0",
        "req1:enter:1",
        "req1:submit:7",
        "grant-1",
        "",
    ] {
        let key = (!forged.is_empty()).then_some(forged);
        let error = block_on(engine.update(
            &invocation(user_actor(), key),
            "employee",
            draft.id(),
            patch_name("Forged"),
            version,
        ))
        .expect_err("a typed command id is not a request context");
        assert_eq!(code(&error), "bpm_entry.owned_by_request", "{forged}");
    }

    // Posting with a forged key neither passes nor clears the owner flag.
    let error = block_on(engine.post(
        &invocation(user_actor(), Some("req1:submit:1")),
        "employee",
        draft.id(),
        version,
    ))
    .expect_err("forged");
    assert_eq!(code(&error), "bpm_entry.owned_by_request");
    let stored = block_on(engine.peek("employee", draft.id()))
        .unwrap()
        .unwrap();
    assert_eq!(stored.text("owned_by_request").unwrap(), Some("req1"));

    // A real grant for a different request does not take the draft either.
    let other = ports.grant(&user_actor(), "req2", "enter", 1);
    let error = block_on(engine.update(
        &invocation(user_actor(), Some(&other)),
        "employee",
        draft.id(),
        patch_name("Other request"),
        version,
    ))
    .expect_err("another request");
    assert_eq!(code(&error), "bpm_entry.owned_by_request");
}

#[test]
fn a_grant_issued_to_another_person_or_an_old_attempt_does_not_pass() {
    let ports = Ports::new();
    let engine = ports.engine();
    let draft = owned_draft(&ports, &engine, "req1");
    let version = draft.version().unwrap();

    // The owning request's grant, but issued to the import component: a user who copied it is
    // not that person.
    let to_import = ports.grant(&import_actor(), "req1", "enter", 1);
    let error = block_on(engine.update(
        &invocation(user_actor(), Some(&to_import)),
        "employee",
        draft.id(),
        patch_name("Copied"),
        version,
    ))
    .expect_err("the grant is bound to the person it was issued to");
    assert_eq!(code(&error), "bpm_entry.owned_by_request");

    // An old attempt's grant stops working once a newer attempt of the step is issued.
    let old = ports.grant(&user_actor(), "req1", "enter", 2);
    let current = ports.grant(&user_actor(), "req1", "enter", 3);
    let error = block_on(engine.update(
        &invocation(user_actor(), Some(&old)),
        "employee",
        draft.id(),
        patch_name("Replayed"),
        version,
    ))
    .expect_err("a replayed old attempt");
    assert_eq!(code(&error), "bpm_entry.owned_by_request");
    let ok = block_on(engine.update(
        &invocation(user_actor(), Some(&current)),
        "employee",
        draft.id(),
        patch_name("Current"),
        version,
    ));
    assert!(ok.is_ok(), "{ok:?}");
}
