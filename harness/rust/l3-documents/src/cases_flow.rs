//! Nested lifecycles (ADR-51), amend chains (ADR-42) and messages.
//!
//! ADR-51: four phases; one transaction for phases 1-3, new transactions in phase 4. A nested
//! write started in phase 3 joins the caller's transaction, so when anything in it fails the
//! whole outer operation rolls back, and every `*_committed` step runs after the single
//! commit. ADR-42: `correct` cancels a posted document and creates its amended draft (a new
//! record with `amended_from`, fields and child rows copied) in the same transaction;
//! correcting requires a posted document, and each amendment cancels the document it
//! corrects. Messages raised by hooks arrive in the operation's result, in raised order.

use std::sync::Arc;

use serde_json::Value as Json;
use serde_json::json;

use spot_app_api::contract_tests::{CaseResult, Failure, Fixture};
use spot_app_api::lifecycle::{Operation, Phase, Target};
use spot_app_api::message::Message;
use spot_app_api::prelude::{Value, Version};
use spot_app_api::{
    AppError, BoxFuture, DocState, Document, ErrorClass, HookContext, HookPoint, JobRequest,
};

use crate::shared::{
    Created, Hooked, Log, check, created_of, cur, define_catalog, expect_err, hook_fn, ids, inv, n,
    new_customer, new_invoice, note_created, occurrences, push, read, recorder, step, take, text,
};

/// Declares the case's runner for the suite's case list.
macro_rules! l3_case {
    ($entry:ident, $case:ident) => {
        pub(crate) fn $entry(fixture: &dyn Fixture) -> BoxFuture<'_, CaseResult> {
            Box::pin(async move { $case(fixture).await })
        }
    };
}

/// ADR-51: one operation, one commit. An `after_create` hook runs in the caller's
/// transaction, so a chain of nested creates (customer to invoice to note) is committed at
/// once; the `*_committed` steps of every document in the chain run after that single
/// commit, exactly once each, and one job arrives.
async fn nested_created_children_commit_together_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let log = Log::default();
    let queued = ids(h, 2);
    let created = Created::default();

    let spawn_invoice = hook_fn({
        let queued = queued.clone();
        let created = created.clone();
        move |ctx: &mut dyn HookContext| {
            let queued = queued.clone();
            let created = created.clone();
            Box::pin(async move {
                let mut invoice = Document::new(n("invoice"), take(&queued)?);
                note_created(&created, "invoice", invoice.id());
                invoice.set(n("customer"), Value::Id(ctx.doc().id()));
                invoice.set(n("number"), text("INV-1"));
                ctx.create(invoice).await?;
                Ok(())
            })
        }
    });

    let spawn_note = hook_fn({
        let queued = queued.clone();
        let created = created.clone();
        move |ctx: &mut dyn HookContext| {
            let queued = queued.clone();
            let created = created.clone();
            Box::pin(async move {
                let note = Document::new(n("note"), take(&queued)?);
                note_created(&created, "note", note.id());
                ctx.create(note).await?;
                Ok(())
            })
        }
    });

    let committed = hook_fn({
        let log = log.clone();
        move |ctx: &mut dyn HookContext| {
            let log = log.clone();
            Box::pin(async move {
                push(&log, "customer:create_committed");
                ctx.enqueue_job(JobRequest {
                    job: "l3:record_commit".to_owned(),
                    args: json!({}),
                    idempotency_key: None,
                    not_before: None,
                })?;
                Ok(())
            })
        }
    });

    h.register(Arc::new(
        Hooked::new("customer")
            .hook(
                step("l3:spawn_invoice"),
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                spawn_invoice,
            )
            .hook(
                step("l3:customer_committed"),
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                committed,
            )
            .hook(
                step("l3:customer_save_committed"),
                HookPoint::of(Phase::Committed, Target::Save),
                recorder(log.clone(), "customer"),
            ),
    ));
    h.register(Arc::new(
        Hooked::new("invoice")
            .hook(
                step("l3:spawn_note"),
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                spawn_note,
            )
            .hook(
                step("l3:invoice_create_committed"),
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                recorder(log.clone(), "invoice"),
            )
            .hook(
                step("l3:invoice_save_committed"),
                HookPoint::of(Phase::Committed, Target::Save),
                recorder(log.clone(), "invoice"),
            ),
    ));
    h.register(Arc::new(
        Hooked::new("note")
            .hook(
                step("l3:note_create_committed"),
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                recorder(log.clone(), "note"),
            )
            .hook(
                step("l3:note_save_committed"),
                HookPoint::of(Phase::Committed, Target::Save),
                recorder(log.clone(), "note"),
            ),
    ));

    let mut customer = Document::new(n("customer"), h.fresh_id());
    customer.set(n("customer_name"), text("Acme"));
    let saved = h.create(&inv("l3:nested"), customer).await?.value;

    for label in [
        "customer:create_committed",
        "customer:save_committed",
        "invoice:create_committed",
        "invoice:save_committed",
        "note:create_committed",
        "note:save_committed",
    ] {
        check(
            occurrences(&log, label) == 1,
            format!(
                "step {label} ran exactly once, not {}",
                occurrences(&log, label)
            ),
        )?;
    }

    let invoice_ids = created_of(&created, "invoice");
    check(
        invoice_ids.len() == 1,
        format!("one invoice was created, saw {}", invoice_ids.len()),
    )?;
    let note_ids = created_of(&created, "note");
    check(note_ids.len() == 1, "one note was created")?;
    check(
        h.stored("customer", saved.id()).is_some(),
        "customer stored",
    )?;
    check(
        h.stored("invoice", invoice_ids[0]).is_some(),
        "invoice stored in the same transaction",
    )?;
    check(
        h.stored("note", note_ids[0]).is_some(),
        "note stored in the same transaction",
    )?;
    check(
        h.outbox().len() == 3,
        format!("one lifecycle event per document, saw {}", h.outbox().len()),
    )?;
    check(
        h.jobs().len() == 1,
        "exactly one job, from the outer committed hook",
    )?;
    Ok(())
}

/// ADR-51: a nested create chain is one transaction. When the third document's
/// `validate_create` step fails, the outer create is refused: nothing is stored, and no
/// committed step, lifecycle event or queued job survives.
async fn nested_rollback_cancels_every_child_and_its_events_and_jobs_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let log = Log::default();
    let queued = ids(h, 2);
    let created = Created::default();

    let refuse = hook_fn(|_ctx: &mut dyn HookContext| {
        Box::pin(async {
            Err::<(), AppError>(AppError::new(
                ErrorClass::Internal,
                "l3.refuse_nested",
                "no third document today",
            ))
        })
    });

    let spawn_invoice_and_job = hook_fn({
        let queued = queued.clone();
        let created = created.clone();
        move |ctx: &mut dyn HookContext| {
            let queued = queued.clone();
            let created = created.clone();
            Box::pin(async move {
                let mut invoice = Document::new(n("invoice"), take(&queued)?);
                note_created(&created, "invoice", invoice.id());
                invoice.set(n("customer"), Value::Id(ctx.doc().id()));
                invoice.set(n("number"), text("INV-1"));
                ctx.enqueue_job(JobRequest {
                    job: "l3:record_invoice_step".to_owned(),
                    args: json!({}),
                    idempotency_key: None,
                    not_before: None,
                })?;
                ctx.create(invoice).await?;
                Ok(())
            })
        }
    });

    let spawn_note = hook_fn({
        let queued = queued.clone();
        let created = created.clone();
        move |ctx: &mut dyn HookContext| {
            let queued = queued.clone();
            let created = created.clone();
            Box::pin(async move {
                let note = Document::new(n("note"), take(&queued)?);
                note_created(&created, "note", note.id());
                ctx.create(note).await?;
                Ok(())
            })
        }
    });

    h.register(Arc::new(
        Hooked::new("customer")
            .hook(
                step("l3:spawn_invoice"),
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                spawn_invoice_and_job,
            )
            .hook(
                step("l3:customer_create_committed"),
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                recorder(log.clone(), "customer"),
            )
            .hook(
                step("l3:customer_save_committed"),
                HookPoint::of(Phase::Committed, Target::Save),
                recorder(log.clone(), "customer"),
            ),
    ));
    h.register(Arc::new(
        Hooked::new("invoice")
            .hook(
                step("l3:spawn_note"),
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                spawn_note,
            )
            .hook(
                step("l3:invoice_create_committed"),
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                recorder(log.clone(), "invoice"),
            )
            .hook(
                step("l3:invoice_save_committed"),
                HookPoint::of(Phase::Committed, Target::Save),
                recorder(log.clone(), "invoice"),
            ),
    ));
    h.register(Arc::new(
        Hooked::new("note")
            .hook(
                step("l3:note_refuses"),
                HookPoint::of(Phase::Validate, Target::Op(Operation::Create)),
                refuse,
            )
            .hook(
                step("l3:note_create_committed"),
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                recorder(log.clone(), "note"),
            )
            .hook(
                step("l3:note_save_committed"),
                HookPoint::of(Phase::Committed, Target::Save),
                recorder(log.clone(), "note"),
            ),
    ));

    let mut customer = Document::new(n("customer"), h.fresh_id());
    customer.set(n("customer_name"), text("Acme"));
    let customer_id = customer.id();

    let failure = h.create(&inv("l3:rollback"), customer).await;
    let err = expect_err(failure, "l3.refuse_nested")?;
    check(
        err.details.is_empty(),
        "the refusal carries no extra details",
    )?;

    for label in [
        "customer:create_committed",
        "customer:save_committed",
        "invoice:create_committed",
        "invoice:save_committed",
        "note:create_committed",
        "note:save_committed",
    ] {
        check(occurrences(&log, label) == 0, format!("no {label} ran"))?;
    }
    check(
        h.stored("customer", customer_id).is_none(),
        "customer was not stored",
    )?;
    let invoice_ids = created_of(&created, "invoice");
    check(invoice_ids.len() == 1, "the nested create started")?;
    check(
        h.stored("invoice", invoice_ids[0]).is_none(),
        "invoice was not stored",
    )?;
    let note_ids = created_of(&created, "note");
    check(note_ids.len() == 1, "the third create started")?;
    check(
        h.stored("note", note_ids[0]).is_none(),
        "note was not stored",
    )?;
    check(
        h.outbox().is_empty(),
        format!("no events survived the rollback, saw {}", h.outbox().len()),
    )?;
    check(h.jobs().is_empty(), "no jobs survived the rollback")?;
    Ok(())
}

/// ADR-51: a `*_committed` step runs after the outer commit, in a new transaction. Its
/// nested create sees the outer document stored, and the nested document's own committed
/// steps run with that second commit.
async fn committed_spawns_run_in_new_transactions_after_the_outer_commit_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let log = Log::default();
    let queued = ids(h, 1);
    let created = Created::default();

    let committed_spawn = hook_fn({
        let queued = queued.clone();
        let created = created.clone();
        move |ctx: &mut dyn HookContext| {
            let queued = queued.clone();
            let created = created.clone();
            Box::pin(async move {
                let note = Document::new(n("note"), take(&queued)?);
                note_created(&created, "note", note.id());
                ctx.create(note).await?;
                Ok(())
            })
        }
    });

    let mut customer = Hooked::new("customer");
    for (idx, point) in [
        HookPoint::of(Phase::Before, Target::Op(Operation::Create)),
        HookPoint::of(Phase::Before, Target::Save),
        HookPoint::of(Phase::Validate, Target::Op(Operation::Create)),
        HookPoint::of(Phase::Validate, Target::Save),
        HookPoint::of(Phase::After, Target::Op(Operation::Create)),
        HookPoint::of(Phase::After, Target::Save),
        HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
        HookPoint::of(Phase::Committed, Target::Save),
    ]
    .into_iter()
    .enumerate()
    {
        customer = customer.hook(
            step(&format!("l3:record_point_{idx}")),
            point,
            recorder(log.clone(), "customer"),
        );
    }
    let customer = customer.hook(
        step("l3:spawn_note_committed"),
        HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
        committed_spawn,
    );
    h.register(Arc::new(customer));
    h.register(Arc::new(
        Hooked::new("note")
            .hook(
                step("l3:note_create_committed"),
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                recorder(log.clone(), "note"),
            )
            .hook(
                step("l3:note_save_committed"),
                HookPoint::of(Phase::Committed, Target::Save),
                recorder(log.clone(), "note"),
            ),
    ));

    let mut doc = Document::new(n("customer"), h.fresh_id());
    doc.set(n("customer_name"), text("Acme"));
    let customer_id = doc.id();
    h.create(&inv("l3:committed"), doc).await?;

    for point in [
        "customer:before_create",
        "customer:before_save",
        "customer:validate_create",
        "customer:validate_save",
        "customer:after_create",
        "customer:after_save",
        "customer:create_committed",
        "customer:save_committed",
    ] {
        check(
            occurrences(&log, point) == 1,
            format!("step for {point} ran exactly once"),
        )?;
    }

    let history = read(&log);
    let before_create = history
        .iter()
        .position(|e| e.as_str() == "customer:before_create");
    let committed = history
        .iter()
        .position(|e| e.as_str() == "customer:create_committed");
    check(
        before_create.is_some() && committed.is_some(),
        "the outer lifecycle ran all phases",
    )?;
    let (before_create, committed) = (before_create.unwrap_or(0), committed.unwrap_or(usize::MAX));
    check(
        before_create < committed,
        format!("before_create ran before the commit ({before_create} then {committed})"),
    )?;
    let note_committed = history
        .iter()
        .position(|e| e.as_str() == "note:create_committed");
    check(
        note_committed.is_none() || committed < note_committed.unwrap_or(0),
        "the note's committed step did not run inside the outer transaction",
    )?;

    let note_ids = created_of(&created, "note");
    check(note_ids.len() == 1, "one note was created")?;
    check(
        h.stored("customer", customer_id).is_some(),
        "customer stored",
    )?;
    check(
        h.stored("note", note_ids[0]).is_some(),
        "note stored in the committed step",
    )?;
    check(
        h.outbox().len() == 2,
        format!("one create event per document, saw {}", h.outbox().len()),
    )?;
    Ok(())
}

/// ADR-056: nested operations do not mint their own context. Both lifecycle events - the
/// outer document's and the nested document's - carry the outer operation's command id.
async fn nested_create_events_carry_the_outer_command_id_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let queued = ids(h, 1);

    let spawn_invoice = hook_fn({
        let queued = queued.clone();
        move |ctx: &mut dyn HookContext| {
            let queued = queued.clone();
            Box::pin(async move {
                let mut invoice = Document::new(n("invoice"), take(&queued)?);
                invoice.set(n("customer"), Value::Id(ctx.doc().id()));
                invoice.set(n("number"), text("INV-1"));
                ctx.create(invoice).await?;
                Ok(())
            })
        }
    });
    h.register(Arc::new(Hooked::new("customer").hook(
        step("l3:spawn_invoice"),
        HookPoint::of(Phase::After, Target::Op(Operation::Create)),
        spawn_invoice,
    )));

    let mut doc = Document::new(n("customer"), h.fresh_id());
    doc.set(n("customer_name"), text("Acme"));
    h.create(&inv("l3:nested-cmd"), doc).await?;

    let events: Vec<_> = h
        .outbox()
        .into_iter()
        .filter(|e| e.topic.as_str() == "doc_created")
        .collect();
    check(
        events.len() == 2,
        format!("two doc_created events, got {}", events.len()),
    )?;
    for event in &events {
        check(
            event.subject.entity.as_str() == "customer"
                || event.subject.entity.as_str() == "invoice",
            format!(
                "event subject is a created document, got {}",
                event.subject.entity.as_str()
            ),
        )?;
        let payload: Json = serde_json::from_str(&event.payload)
            .map_err(|e| Failure(format!("event payload is JSON: {e}")))?;
        check(
            payload.get("meta").and_then(|m| m.get("command_id")) == Some(&json!("l3:nested-cmd")),
            "nested create event carries the outer command id",
        )?;
    }
    Ok(())
}

/// Messages raised by hooks - on the outer document and, nested, on inner documents -
/// arrive in the operation's result in raised order (ADR-55): the outer document's earlier
/// phases first, an inner document's message exactly where that inner create happened.
async fn messages_from_nested_hooks_arrive_in_raised_order_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let queued = ids(h, 2);

    let say = |code: &'static str, label: &'static str| {
        hook_fn(move |ctx: &mut dyn HookContext| {
            Box::pin(async move {
                ctx.message(Message::info(code, label));
                Ok(())
            })
        })
    };
    let outer_before = say("l3.outer_before", "outer before_create");
    let outer_validate = say("l3.outer_validate", "outer validate_save");
    let outer_after = say("l3.outer_after", "outer after_save");

    let inner_after_and_spawn = hook_fn({
        let queued = queued.clone();
        move |ctx: &mut dyn HookContext| {
            let queued = queued.clone();
            Box::pin(async move {
                ctx.message(Message::info("l3.inner_after", "inner after_create"));
                let mut invoice = Document::new(n("invoice"), take(&queued)?);
                invoice.set(n("customer"), Value::Id(ctx.doc().id()));
                invoice.set(n("number"), text("INV-1"));
                ctx.create(invoice).await?;
                Ok(())
            })
        }
    });

    h.register(Arc::new(
        Hooked::new("customer")
            .hook(
                step("l3:outer_before"),
                HookPoint::of(Phase::Before, Target::Op(Operation::Create)),
                outer_before,
            )
            .hook(
                step("l3:inner_after"),
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                inner_after_and_spawn,
            )
            .hook(
                step("l3:outer_validate"),
                HookPoint::of(Phase::Validate, Target::Save),
                outer_validate,
            )
            .hook(
                step("l3:outer_after"),
                HookPoint::of(Phase::After, Target::Save),
                outer_after,
            ),
    ));

    let mut doc = Document::new(n("customer"), h.fresh_id());
    doc.set(n("customer_name"), text("Acme"));
    let out = h.create(&inv("l3:messages"), doc).await?;

    let got: Vec<&str> = out.messages.iter().map(|m| m.code.as_str()).collect();
    check(
        got == [
            "l3.outer_before",
            "l3.outer_validate",
            "l3.inner_after",
            "l3.outer_after",
        ],
        format!("messages in raised order: {got:?}"),
    )?;
    Ok(())
}

/// ADR-42: `correct` on a posted document cancels it and creates its amended draft in the
/// same transaction: a new record, `amended_from` link set, fields and child rows copied
/// (child rows become new records), state draft, version reset to 1.
async fn amend_correct_cancels_original_and_opens_draft_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let customer = new_customer(h, "Acme").await?;
    let original = new_invoice(h, customer.id(), &[("A", "1"), ("B", "2")]).await?;
    let original_row = original.rows("items")[0].id;
    let posted = h
        .post(&inv("l3:amend-1"), "invoice", original.id(), cur(&original))
        .await?
        .value;
    check(
        posted.state() == Some(DocState::Posted),
        "the original is posted",
    )?;
    check(
        posted.get("number") == &text("INV-1"),
        "the original kept its number",
    )?;

    let draft = h
        .correct(&inv("l3:amend-1"), "invoice", original.id(), cur(&posted))
        .await?
        .value;

    check(
        draft.state() == Some(DocState::Draft),
        "the amended document is a draft",
    )?;
    check(draft.id() != original.id(), "the draft is a new record")?;
    check(
        draft.get("amended_from") == &Value::Id(original.id()),
        "amended_from links the cancelled original",
    )?;
    check(
        draft.get("number") == &text("INV-1"),
        "the fields were copied",
    )?;
    check(
        draft.get("customer") == &Value::Id(customer.id()),
        "the link field was copied",
    )?;
    check(
        draft.rows("items").len() == 2,
        "both child rows were copied",
    )?;
    let rows = draft.rows("items");
    check(
        rows[0].fields.get(&n("item")) == Some(&text("A")),
        "the first row was copied",
    )?;
    check(
        rows[1].fields.get(&n("item")) == Some(&text("B")),
        "the second row was copied",
    )?;
    check(rows[0].id != original_row, "child rows are new records")?;
    check(
        h.stored("invoice", original.id()).and_then(|d| d.state()) == Some(DocState::Cancelled),
        "the original is cancelled",
    )?;
    check(
        h.stored("invoice", draft.id()).and_then(|d| d.state()) == Some(DocState::Draft),
        "the draft is stored",
    )?;
    Ok(())
}

/// ADR-42: each amendment cancels the posted document it corrects. A chain of amendments
/// therefore posts and corrects again: A stays cancelled and only the newest draft is open,
/// with `amended_from` linking each new draft back one step.
async fn amend_chain_each_amendment_cancels_the_posted_document_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let customer = new_customer(h, "Acme").await?;
    let first = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let posted_a = h
        .post(&inv("l3:chain-1"), "invoice", first.id(), cur(&first))
        .await?
        .value;

    let amended_b = h
        .correct(&inv("l3:chain-1"), "invoice", first.id(), cur(&posted_a))
        .await?
        .value;
    check(
        h.stored("invoice", first.id()).and_then(|d| d.state()) == Some(DocState::Cancelled),
        "the posted original was cancelled",
    )?;
    check(
        amended_b.get("amended_from") == &Value::Id(first.id()),
        "the amended draft links the cancelled original",
    )?;

    let posted_b = h
        .post(
            &inv("l3:chain-2"),
            "invoice",
            amended_b.id(),
            cur(&amended_b),
        )
        .await?
        .value;
    check(
        posted_b.state() == Some(DocState::Posted),
        "the amended draft was posted before the second amendment",
    )?;

    let amended_c = h
        .correct(
            &inv("l3:chain-2"),
            "invoice",
            amended_b.id(),
            cur(&posted_b),
        )
        .await?
        .value;
    check(
        h.stored("invoice", posted_b.id()).and_then(|d| d.state()) == Some(DocState::Cancelled),
        "the second posted document was cancelled too",
    )?;
    check(
        amended_c.get("amended_from") == &Value::Id(posted_b.id()),
        "the newest draft links one step back",
    )?;
    check(
        amended_c.state() == Some(DocState::Draft),
        "the newest draft is open",
    )?;
    Ok(())
}

/// ADR-42: correcting requires a posted document. Correcting a draft would cancel a draft,
/// which is impossible, so it fails with `document.invalid_state` and the draft stays open,
/// with no cancel event emitted.
async fn amend_correct_requires_a_posted_document_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let customer = new_customer(h, "Acme").await?;
    let draft = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let draft_id = draft.id();

    let failure = h
        .correct(&inv("l3:reject"), "invoice", draft.id(), cur(&draft))
        .await;
    expect_err(failure, "document.invalid_state")?;

    check(
        h.stored("invoice", draft_id).and_then(|d| d.state()) == Some(DocState::Draft),
        "the draft is untouched",
    )?;
    check(
        h.outbox()
            .iter()
            .filter(|e| e.topic.as_str() == "doc_cancelled")
            .count()
            == 0,
        "no cancel event from the refused correct",
    )?;
    Ok(())
}

/// ADR-42 with optimistic versions: `correct` needs the latest known version of the posted
/// document. With a stale expectation it fails `document.stale_version`, nothing is
/// cancelled, and no amended draft appears.
async fn amend_correct_with_stale_version_is_refused_and_nothing_cancelled_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let customer = new_customer(h, "Acme").await?;
    let doc = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let posted = h
        .post(&inv("l3:stale"), "invoice", doc.id(), cur(&doc))
        .await?
        .value;
    check(
        posted.version() == Some(Version(2)),
        "the posted document is at version 2",
    )?;

    let failure = h
        .correct(&inv("l3:stale"), "invoice", posted.id(), Version(1))
        .await;
    expect_err(failure, "document.stale_version")?;

    check(
        h.stored("invoice", posted.id()).and_then(|d| d.state()) == Some(DocState::Posted),
        "the posted document was not cancelled",
    )?;
    let leftovers: Vec<_> = h
        .outbox()
        .into_iter()
        .filter(|e| e.topic.as_str() == "doc_created")
        .collect();
    check(
        leftovers.len() == 2,
        format!("no amended draft was created, saw {}", leftovers.len()),
    )?;
    Ok(())
}

/// Messages raised by hooks ride along in the operation's result, but they never fail or
/// block it: the commit completes and the stored document is fine (ADR-55).
async fn messages_from_hooks_do_not_block_commit_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;
    let log = Log::default();

    let say = |code: &'static str, label: &'static str| {
        hook_fn(move |ctx: &mut dyn HookContext| {
            Box::pin(async move {
                ctx.message(Message::info(code, label));
                Ok(())
            })
        })
    };
    let committed = hook_fn({
        let log = log.clone();
        move |ctx: &mut dyn HookContext| {
            let log = log.clone();
            Box::pin(async move {
                push(&log, "committed step ran");
                ctx.message(Message::info("l3.committed", "committed"));
                Ok(())
            })
        }
    });

    h.register(Arc::new(
        Hooked::new("customer")
            .hook(
                step("l3:before"),
                HookPoint::of(Phase::Before, Target::Op(Operation::Create)),
                say("l3.first", "first"),
            )
            .hook(
                step("l3:validate"),
                HookPoint::of(Phase::Validate, Target::Op(Operation::Create)),
                say("l3.second", "second"),
            )
            .hook(
                step("l3:after"),
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                say("l3.third", "third"),
            )
            .hook(
                step("l3:committed"),
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                committed,
            ),
    ));

    let mut doc = Document::new(n("customer"), h.fresh_id());
    doc.set(n("customer_name"), text("Acme"));
    let out = h.create(&inv("l3:messages-only"), doc).await?;

    let got: Vec<&str> = out.messages.iter().map(|m| m.code.as_str()).collect();
    check(
        got == ["l3.first", "l3.second", "l3.third", "l3.committed"],
        format!("raised messages only: {got:?}"),
    )?;
    check(
        occurrences(&log, "committed step ran") == 1,
        "the committed step still ran",
    )?;
    check(h.outbox().len() == 1, "the create event was emitted")?;
    Ok(())
}

l3_case!(
    nested_created_children_commit_together,
    nested_created_children_commit_together_impl
);
l3_case!(
    nested_rollback_cancels_every_child_and_its_events_and_jobs,
    nested_rollback_cancels_every_child_and_its_events_and_jobs_impl
);
l3_case!(
    committed_spawns_run_in_new_transactions_after_the_outer_commit,
    committed_spawns_run_in_new_transactions_after_the_outer_commit_impl
);
l3_case!(
    nested_create_events_carry_the_outer_command_id,
    nested_create_events_carry_the_outer_command_id_impl
);
l3_case!(
    messages_from_nested_hooks_arrive_in_raised_order,
    messages_from_nested_hooks_arrive_in_raised_order_impl
);
l3_case!(
    amend_correct_cancels_original_and_opens_draft,
    amend_correct_cancels_original_and_opens_draft_impl
);
l3_case!(
    amend_chain_each_amendment_cancels_the_posted_document,
    amend_chain_each_amendment_cancels_the_posted_document_impl
);
l3_case!(
    amend_correct_requires_a_posted_document,
    amend_correct_requires_a_posted_document_impl
);
l3_case!(
    amend_correct_with_stale_version_is_refused_and_nothing_cancelled,
    amend_correct_with_stale_version_is_refused_and_nothing_cancelled_impl
);
l3_case!(
    messages_from_hooks_do_not_block_commit,
    messages_from_hooks_do_not_block_commit_impl
);
