//! Versions, lifecycle events (ADR-056), whitelisted methods (ADR-38, ADR-16) and step
//! overrides with seams (ADR-08).
//!
//! ADR-16/38: every write bumps a document's stored version by one and the caller's expected
//! version must match it (`document.stale_version`). ADR-056: every successful operation of a
//! document emits exactly one lifecycle event on the topic of its operation and failed
//! operations emit none; the payload data matches the topic's schema. ADR-38: calling a
//! method not on the DocType's whitelist fails with `method.not_found`; a whitelisted method
//! runs with a method context, may write through it, may raise messages and returns JSON.
//! ADR-08: overrides change which steps run (Replace, Disable, Wrap through `run_wrapped`)
//! and seams round-trip JSON through a versioned provider.

use std::sync::Arc;

use serde_json::{Value as Json, json};

use spot_app_api::contract_tests::{CaseResult, Failure, Fixture};
use spot_app_api::lifecycle::{HookPoint, Operation, Phase, Target};
use spot_app_api::message::Message;
use spot_app_api::prelude::{Value, Version};
use spot_app_api::seams::OverrideKind;
use spot_app_api::seams::SeamProvider;
use spot_app_api::{
    AppError, AppResult, BoxFuture, DocState, Document, DocumentPatch, ErrorClass, HookContext,
    MethodContext, StepOverride,
};

use crate::shared::{
    Call, Hooked, Log, check, cur, dec, define_catalog, expect_err, hook_fn, ids, inv, n,
    new_customer, new_invoice, push, read, step, take, text,
};

/// Declares the case's runner for the suite's case list.
macro_rules! l3_case {
    ($entry:ident, $case:ident) => {
        pub(crate) fn $entry(fixture: &dyn Fixture) -> BoxFuture<'_, CaseResult> {
            Box::pin(async move { $case(fixture).await })
        }
    };
}

/// How many events of one topic an entity received.
fn events_on(h: &dyn spot_app_api::contract_tests::TestHost, entity: &str, topic: &str) -> usize {
    h.outbox()
        .iter()
        .filter(|e| e.topic.as_str() == topic && e.subject.entity.as_str() == entity)
        .count()
}

/// A whitelisted method that changes nothing, always succeeds and returns its name (test-only).
fn method_stub() -> Call {
    Arc::new(|method: &str, _args: Json, _ctx: &mut dyn MethodContext| {
        Box::pin(async move { Ok(json!({"called": method})) })
    })
}

/// ADR-16, ADR-38: every write bumps the version by one — on creates, on updates, on posts
/// and on posted updates alike — and the host stores the version that was returned.
async fn versions_increment_by_one_per_write_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    check(
        customer.version() == Some(Version::FIRST),
        "created at version 1",
    )?;
    let updated = h
        .update(
            &inv("l3:v-up"),
            "customer",
            customer.id(),
            DocumentPatch {
                fields: [(n("credit_limit"), dec("100.00"))].into(),
                ..Default::default()
            },
            Version::FIRST,
        )
        .await?
        .value;
    check(
        updated.version() == Some(Version(2)),
        "the first update is version 2",
    )?;
    check(
        h.stored("customer", customer.id())
            .map(|d| d.version() == Some(Version(2)))
            .unwrap_or(false),
        "the host stored version 2",
    )?;

    let invoice = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    check(
        invoice.version() == Some(Version::FIRST),
        "the invoice was created at version 1",
    )?;
    let posted = h
        .post(&inv("l3:v-post"), "invoice", invoice.id(), Version::FIRST)
        .await?
        .value;
    check(
        posted.version() == Some(Version(2)),
        "the post is version 2",
    )?;
    let patched = h
        .update(
            &inv("l3:v-patch"),
            "invoice",
            invoice.id(),
            DocumentPatch {
                fields: [(n("remarks"), text("posted change"))].into(),
                ..Default::default()
            },
            Version(2),
        )
        .await?
        .value;
    check(
        patched.version() == Some(Version(3)),
        "the posted update is version 3",
    )?;
    check(
        h.stored("invoice", invoice.id())
            .map(|d| d.version() == Some(Version(3)) && d.state() == Some(DocState::Posted))
            .unwrap_or(false),
        "the host stored version 3 of the posted document",
    )?;
    Ok(())
}

/// ADR-16/38: the caller's expected version must be the document's current version; an
/// update with an old expectation fails `document.stale_version`, the error names the
/// current version, and the stored document keeps its state and version.
async fn versions_second_update_stale_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let first = h
        .update(
            &inv("l3:stale-1"),
            "customer",
            customer.id(),
            DocumentPatch {
                fields: [(n("credit_limit"), dec("10.00"))].into(),
                ..Default::default()
            },
            Version::FIRST,
        )
        .await?
        .value;
    check(
        first.version() == Some(Version(2)),
        "the first update is version 2",
    )?;

    let err = expect_err(
        h.update(
            &inv("l3:stale-2"),
            "customer",
            customer.id(),
            DocumentPatch {
                fields: [(n("credit_limit"), dec("20.00"))].into(),
                ..Default::default()
            },
            Version::FIRST,
        )
        .await,
        "document.stale_version",
    )?;
    check(
        err.details.get("current_version") == Some(&json!(2)),
        "the refusal names the current version",
    )?;
    check(
        h.stored("customer", customer.id())
            .map(|d| d.version() == Some(Version(2)) && d.get("credit_limit") == &dec("10.00"))
            .unwrap_or(false),
        "the refused write did not persist",
    )?;
    Ok(())
}

/// ADR-056: every successful operation emits exactly one lifecycle event on its topic and
/// carries its subject's new version; failed operations emit none (next case).
async fn events_every_successful_operation_emits_exactly_one_lifecycle_event_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let draft = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let invoice_id = draft.id();
    check(
        h.outbox()
            .iter()
            .filter(|e| e.subject.id == invoice_id)
            .count()
            == 1,
        "the create emitted one event",
    )?;

    let updated = h
        .update(
            &inv("l3:ev-up"),
            "invoice",
            invoice_id,
            DocumentPatch {
                fields: [(n("remarks"), text("draft change"))].into(),
                ..Default::default()
            },
            cur(&draft),
        )
        .await?
        .value;
    check(
        updated.version() == Some(Version(2)),
        "the draft update is version 2",
    )?;
    let renamed = h
        .rename(
            &inv("l3:ev-rn"),
            "invoice",
            invoice_id,
            "INV-2",
            cur(&updated),
        )
        .await?
        .value;
    check(
        renamed.version() == Some(Version(3)),
        "the rename is version 3",
    )?;
    let posted = h
        .post(&inv("l3:ev-post"), "invoice", invoice_id, cur(&renamed))
        .await?
        .value;
    check(
        posted.version() == Some(Version(4)),
        "the post is version 4",
    )?;
    let reposted = h
        .update(
            &inv("l3:ev-pup"),
            "invoice",
            invoice_id,
            DocumentPatch {
                fields: [(n("remarks"), text("posted change"))].into(),
                ..Default::default()
            },
            cur(&posted),
        )
        .await?
        .value;
    check(
        reposted.version() == Some(Version(5)),
        "the posted update is version 5",
    )?;
    let amended = h
        .correct(&inv("l3:ev-correct"), "invoice", invoice_id, cur(&reposted))
        .await?
        .value;
    check(
        amended.get("amended_from") == &Value::Id(reposted.id()),
        "the amended draft links the cancelled original",
    )?;

    let other = new_invoice(h, customer.id(), &[("B", "2")]).await?;
    let other_id = other.id();
    h.delete(&inv("l3:ev-del"), "invoice", other_id, cur(&other))
        .await?;
    check(
        h.stored("invoice", other_id).is_none(),
        "the second draft was deleted",
    )?;

    let outbox = h.outbox();
    for (topic, count) in [
        ("doc_created", 1),
        ("doc_updated", 1),
        ("doc_renamed", 1),
        ("doc_posted", 1),
        ("doc_updated_posted", 1),
        ("doc_cancelled", 1),
    ] {
        check(
            outbox
                .iter()
                .filter(|e| e.subject.id == invoice_id && e.topic.as_str() == topic)
                .count()
                == count,
            format!("exactly one {topic} event for the invoice"),
        )?;
    }
    for (topic, version) in [
        ("doc_created", Version::FIRST),
        ("doc_updated", Version(2)),
        ("doc_renamed", Version(3)),
        ("doc_posted", Version(4)),
        ("doc_updated_posted", Version(5)),
        ("doc_cancelled", Version(6)),
    ] {
        check(
            outbox.iter().any(|e| {
                e.subject.id == invoice_id
                    && e.topic.as_str() == topic
                    && e.subject_version == Some(version)
            }),
            format!("the {topic} event carries subject_version {:?}", version.0),
        )?;
    }
    check(
        outbox.iter().filter(|e| e.subject.id == other_id).count() == 2
            && outbox.iter().any(|e| {
                e.subject.id == other_id
                    && e.topic.as_str() == "doc_deleted"
                    && e.subject_version == Some(Version::FIRST)
            }),
        "the other invoice was created and deleted, one event each",
    )?;
    Ok(())
}

/// ADR-056: refusals post nothing — wrong state, read-only field, stale expectation and a
/// linked-document delete all leave the outbox unchanged, and the stored documents keep
/// their states and versions.
async fn events_failed_operations_emit_no_events_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let invoice = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let posted = h
        .post(&inv("l3:evf-post"), "invoice", invoice.id(), cur(&invoice))
        .await?
        .value;
    let draft2 = new_invoice(h, customer.id(), &[("B", "2")]).await?;
    let re_customer = h
        .update(
            &inv("l3:evf-ok"),
            "customer",
            customer.id(),
            DocumentPatch {
                fields: [(n("credit_limit"), dec("5.00"))].into(),
                ..Default::default()
            },
            Version::FIRST,
        )
        .await?
        .value;
    check(
        re_customer.version() == Some(Version(2)),
        "the customer is at version 2",
    )?;

    let read_only = DocumentPatch {
        fields: [(n("total"), dec("1.00"))].into(),
        ..Default::default()
    };
    let stale = DocumentPatch {
        fields: [(n("credit_limit"), dec("9.00"))].into(),
        ..Default::default()
    };
    let before = h.outbox().len();
    expect_err(
        h.post(&inv("l3:evf-2"), "invoice", posted.id(), cur(&posted))
            .await,
        "document.invalid_state",
    )?;
    expect_err(
        h.update(
            &inv("l3:evf-ro"),
            "invoice",
            posted.id(),
            read_only,
            cur(&posted),
        )
        .await,
        "document.read_only_field",
    )?;
    expect_err(
        h.delete(&inv("l3:evf-del"), "invoice", posted.id(), cur(&posted))
            .await,
        "document.invalid_state",
    )?;
    expect_err(
        h.correct(&inv("l3:evf-correct"), "invoice", draft2.id(), cur(&draft2))
            .await,
        "document.invalid_state",
    )?;
    expect_err(
        h.update(
            &inv("l3:evf-stale"),
            "customer",
            customer.id(),
            stale,
            Version::FIRST,
        )
        .await,
        "document.stale_version",
    )?;
    expect_err(
        h.delete(
            &inv("l3:evf-linked"),
            "customer",
            customer.id(),
            cur(&re_customer),
        )
        .await,
        "document.linked",
    )?;
    check(h.outbox().len() == before, "the refusals emitted no events")?;
    check(
        h.stored("invoice", posted.id())
            .map(|d| d.state() == Some(DocState::Posted) && d.version() == Some(Version(2)))
            .unwrap_or(false),
        "the posted document is untouched",
    )?;
    check(
        h.stored("invoice", draft2.id())
            .map(|d| d.state() == Some(DocState::Draft) && d.version() == Some(Version::FIRST))
            .unwrap_or(false),
        "the draft is untouched",
    )?;
    Ok(())
}

/// ADR-056: each topic's event payload data validates against the topic's schema, so the
/// data a consumer receives is shaped as the lifecycle contract promises.
async fn events_lifecycle_data_matches_the_topic_schema_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let draft = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let invoice_id = draft.id();
    let updated = h
        .update(
            &inv("l3:evs-up"),
            "invoice",
            invoice_id,
            DocumentPatch {
                fields: [(n("remarks"), text("schema check"))].into(),
                ..Default::default()
            },
            cur(&draft),
        )
        .await?
        .value;
    h.rename(
        &inv("l3:evs-rn"),
        "invoice",
        invoice_id,
        "INV-2",
        cur(&updated),
    )
    .await?;
    let posted = h
        .post(&inv("l3:evs-post"), "invoice", invoice_id, Version(3))
        .await?
        .value;
    h.update(
        &inv("l3:evs-pup"),
        "invoice",
        invoice_id,
        DocumentPatch {
            fields: [(n("remarks"), text("posted amendment"))].into(),
            ..Default::default()
        },
        cur(&posted),
    )
    .await?;
    h.correct(&inv("l3:evs-correct"), "invoice", invoice_id, Version(5))
        .await?;
    let other = new_invoice(h, customer.id(), &[("B", "2")]).await?;
    let other_id = other.id();
    h.delete(&inv("l3:evs-del"), "invoice", other_id, cur(&other))
        .await?;

    for event in h.outbox() {
        let topic = event.topic.as_str();
        check(
            event.schema_version == 1,
            format!("the {topic} event declared schema version 1"),
        )?;
        let op = match topic {
            "doc_created" => Operation::Create,
            "doc_updated" => Operation::Update,
            "doc_updated_posted" => Operation::UpdatePosted,
            "doc_posted" => Operation::Post,
            "doc_cancelled" => Operation::Cancel,
            "doc_renamed" => Operation::Rename,
            "doc_deleted" => Operation::Delete,
            other => return Err(Failure(format!("unexpected lifecycle topic {other}"))),
        };
        let payload: Json = match serde_json::from_str(&event.payload) {
            Ok(payload) => payload,
            Err(e) => return Err(Failure(format!("unreadable payload for {topic}: {e}"))),
        };
        let data = payload["data"].clone();
        check(
            op.schema().validate(&data).is_ok(),
            format!("the {topic} payload data matches the topic's schema"),
        )?;
    }
    Ok(())
}

/// ADR-38: methods not on the doctype's whitelist fail with `method.not_found` — unknown
/// names on a known doctype, known names on a doctype without a controller, and any method
/// on a doctype with no controller at all. Nothing is stored by a refused call.
async fn methods_not_on_the_whitelist_fail_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    h.register(Arc::new(Hooked::new("customer").method(
        "add_note",
        true,
        method_stub(),
    )));
    let customer = new_customer(h, "Acme").await?;
    let invoice = new_invoice(h, customer.id(), &[("A", "1")]).await?;

    expect_err(
        h.call_method(
            &inv("l3:m-unknown"),
            "customer",
            customer.id(),
            "no_such_method",
            json!({}),
            Some(cur(&customer)),
        )
        .await,
        "method.not_found",
    )?;
    expect_err(
        h.call_method(
            &inv("l3:m-not-mine"),
            "invoice",
            invoice.id(),
            "add_note",
            json!({}),
            Some(cur(&invoice)),
        )
        .await,
        "method.not_found",
    )?;
    expect_err(
        h.call_method(
            &inv("l3:m-no-controller"),
            "note",
            customer.id(),
            "add_note",
            json!({}),
            Some(cur(&customer)),
        )
        .await,
        "method.not_found",
    )?;
    check(
        h.outbox().iter().all(|e| {
            e.subject.entity.as_str() == "customer" || e.subject.entity.as_str() == "invoice"
        }),
        "no events from the refused calls",
    )?;
    check(
        h.stored("customer", customer.id())
            .map(|d| d.version() == Some(Version(1)))
            .unwrap_or(false),
        "the refused calls left the customer untouched",
    )?;
    Ok(())
}

/// ADR-38: a whitelisted method runs with a method context, writes through it (its create
/// and update are part of the same transaction), raises the messages it returns and its
/// JSON value rides along in the result.
async fn methods_whitelisted_method_runs_and_returns_json_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let note_ids = ids(h, 1);
    let note_id = note_ids.lock().unwrap_or_else(|p| p.into_inner())[0];
    let call: Call = {
        let note_ids = note_ids.clone();
        Arc::new(
            move |method: &str, args: Json, ctx: &mut dyn MethodContext| {
                let note_ids = note_ids.clone();
                Box::pin(async move {
                    if method != "add_note" {
                        return Ok(json!({"called": method}));
                    }
                    let mut note = Document::new(n("note"), take(&note_ids)?);
                    note.set(n("text"), text(args["text"].as_str().unwrap_or("")));
                    ctx.create(note).await?;
                    let updated = ctx
                        .update(
                            "customer",
                            ctx.doc().id(),
                            DocumentPatch {
                                fields: [(n("credit_limit"), dec("7.00"))].into(),
                                ..Default::default()
                            },
                            cur(ctx.doc()),
                        )
                        .await?;
                    if updated.version() != Some(Version(2)) {
                        return Err(AppError::new(
                            ErrorClass::Internal,
                            "document.internal_error",
                            "the method's write did not bump the customer's version",
                        ));
                    }
                    ctx.message(Message::info("l3.noted", "the note is in"));
                    Ok(json!({"noted": true}))
                })
            },
        )
    };
    h.register(Arc::new(
        Hooked::new("customer").method("add_note", true, call),
    ));

    let customer = new_customer(h, "Acme").await?;
    let out = h
        .call_method(
            &inv("l3:m-run"),
            "customer",
            customer.id(),
            "add_note",
            json!({"text": "call back"}),
            Some(cur(&customer)),
        )
        .await?;
    check(
        out.value.1 == json!({"noted": true}),
        "the method returned its JSON result",
    )?;
    check(
        out.value.0.version() == Some(Version(2))
            && out.value.0.get("credit_limit") == &dec("7.00"),
        "the customer is returned as it is after the method's write",
    )?;
    check(
        out.messages.iter().any(|m| m.code.as_str() == "l3.noted"),
        "the method's message arrived in the result",
    )?;
    check(
        h.stored("note", note_id).is_some(),
        "the note the method created is stored",
    )?;
    check(
        events_on(h, "customer", "doc_updated") == 1 && events_on(h, "note", "doc_created") == 1,
        "the method's writes emitted their lifecycle events",
    )?;
    Ok(())
}

/// ADR-08: overrides change which steps run — a replacement runs instead of its target, a
/// disabled step does not run, and a wrapping step brackets its target through
/// `run_wrapped` — and a seam round-trips JSON through a versioned provider; calling a
/// missing seam is a `method.not_found` error.
async fn overrides_change_steps_and_seams_round_trip_json_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let customer = new_customer(h, "Acme").await?;

    h.provide_seam("l3_docs:double", 1, Arc::new(Doubler));

    let log = Log::default();
    let at_op = HookPoint::of(Phase::Before, Target::Op(Operation::Create));
    let at_save = HookPoint::of(Phase::Before, Target::Save);
    let mut probe = Hooked::new("invoice");
    // Targets sit at the hook points of the create operation; the `by_*` steps are declared
    // on a point that never fires for a create (load), so they only run through overrides.
    for (name, point, wrap) in [
        ("app:t_disabled", at_op.clone(), false),
        ("app:t_replace", at_op.clone(), false),
        ("app:t_wrap", at_save.clone(), false),
        ("app:by_instead", HookPoint::Load, false),
        ("app:by_wrap", HookPoint::Load, true),
    ] {
        let log = log.clone();
        let label = name.to_owned();
        let wraps = wrap;
        probe = probe.hook(
            step(name),
            point,
            hook_fn(move |ctx: &mut dyn HookContext| {
                let log = log.clone();
                let label = label.clone();
                Box::pin(async move {
                    if wraps {
                        push(&log, "wrap_in");
                        ctx.run_wrapped().await?;
                        push(&log, "wrap_out");
                    } else {
                        push(&log, label);
                    }
                    Ok(())
                })
            }),
        );
    }
    let at_seam = HookPoint::of(Phase::Validate, Target::Save);
    probe = probe.hook(
        step("app:seam"),
        at_seam,
        hook_fn(|ctx: &mut dyn HookContext| {
            Box::pin(async move {
                let doubled = ctx
                    .seams()
                    .call_json("l3_docs:double", 1, json!({"v": 21}))
                    .await?;
                let out = doubled["out"].as_i64().unwrap_or_default();
                if out != 42 {
                    return Err(AppError::new(
                        ErrorClass::Internal,
                        "document.internal_error",
                        "the double seam did not double",
                    ));
                }
                let missing = ctx.seams().call_json("l3_docs:missing", 1, json!({})).await;
                if missing.map_err(|e| e.class) != Err(ErrorClass::MethodNotFound) {
                    return Err(AppError::new(
                        ErrorClass::Internal,
                        "document.internal_error",
                        "a missing seam must be method.not_found",
                    ));
                }
                Ok(())
            })
        }),
    );
    h.register(Arc::new(probe));

    h.add_override(StepOverride {
        target: step("app:t_disabled"),
        kind: OverrideKind::Disable,
        by: None,
        reason: "harness contract test".into(),
    });
    h.add_override(StepOverride {
        target: step("app:t_replace"),
        kind: OverrideKind::Replace,
        by: Some(step("app:by_instead")),
        reason: "harness contract test".into(),
    });
    h.add_override(StepOverride {
        target: step("app:t_wrap"),
        kind: OverrideKind::Wrap,
        by: Some(step("app:by_wrap")),
        reason: "harness contract test".into(),
    });

    let invoice = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    check(
        invoice.version() == Some(Version::FIRST),
        "the document was created with overrides on",
    )?;
    let recorded = read(&log);
    let ran: Vec<String> = recorded.iter().map(|s| s.to_string()).collect();
    check(
        ran == ["app:by_instead", "wrap_in", "app:t_wrap", "wrap_out"],
        format!("the overrides changed which steps ran: {ran:?}"),
    )?;
    check(
        !h.outbox().is_empty(),
        "the create went through despite the overrides",
    )?;
    Ok(())
}

/// Doubles the incoming number, like a real app's pricing helper.
struct Doubler;

impl SeamProvider for Doubler {
    fn call(&self, input: Json) -> BoxFuture<'_, AppResult<Json>> {
        Box::pin(async move { Ok(json!({"out": input["v"].as_i64().unwrap_or(0) * 2})) })
    }
}

l3_case!(
    versions_increment_by_one_per_write,
    versions_increment_by_one_per_write_impl
);
l3_case!(
    versions_second_update_stale,
    versions_second_update_stale_impl
);
l3_case!(
    events_every_successful_operation_emits_exactly_one_lifecycle_event,
    events_every_successful_operation_emits_exactly_one_lifecycle_event_impl
);
l3_case!(
    events_failed_operations_emit_no_events,
    events_failed_operations_emit_no_events_impl
);
l3_case!(
    events_lifecycle_data_matches_the_topic_schema,
    events_lifecycle_data_matches_the_topic_schema_impl
);
l3_case!(
    methods_not_on_the_whitelist_fail,
    methods_not_on_the_whitelist_fail_impl
);
l3_case!(
    methods_whitelisted_method_runs_and_returns_json,
    methods_whitelisted_method_runs_and_returns_json_impl
);
l3_case!(
    overrides_change_steps_and_seams_round_trip_json,
    overrides_change_steps_and_seams_round_trip_json_impl
);
