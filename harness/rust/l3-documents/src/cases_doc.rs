//! Links and child tables (ADR-33, ADR-34, ADR-57) and posted-document rules.
//!
//! ADR-33: a link field points at the target's record id, so renames keep links valid and
//! following writes of the linking document keep validating; a document that another document
//! links to cannot be deleted until the references are gone. ADR-34: a table patch replaces
//! the table; rows keep their ids on reorder and insert, and row errors name rows by their
//! new position. ADR-57/056: table-only changes list the table in the event's changed fields.
//! ADR-42: posted documents accept only changes whose fields are allowed on posted documents,
//! emit `doc_updated_posted`, keep their state through renames and cannot be deleted.

use std::sync::Arc;

use serde_json::{Value as Json, json};

use spot_app_api::contract_tests::{CaseResult, Failure, Fixture};
use spot_app_api::lifecycle::{Operation, Phase, Target};
use spot_app_api::prelude::{Value, Version};
use spot_app_api::{BoxFuture, DocState, Document, DocumentPatch, ErrorClass, HookPoint, RowPatch};

use crate::shared::{
    Created, Hooked, Log, check, created_of, cur, dec, define_catalog, expect_err, hook_fn, ids,
    inv, n, new_customer, new_invoice, note_created, push, read, recorder, step, take, text,
};

/// Declares the case's runner for the suite's case list.
macro_rules! l3_case {
    ($entry:ident, $case:ident) => {
        pub(crate) fn $entry(fixture: &dyn Fixture) -> BoxFuture<'_, CaseResult> {
            Box::pin(async move { $case(fixture).await })
        }
    };
}

/// A patch that sets one field.
fn one_field(name: spot_app_api::prelude::Name, value: Value) -> DocumentPatch {
    DocumentPatch {
        fields: [(name, value)].into(),
        ..Default::default()
    }
}

/// ADR-33: a link field points at the target's id, not at its name. Renaming the target of a
/// draft link therefore keeps every link valid: the links keep pointing at the same id and a
/// following update of the linking document still validates the link against the renamed
/// target.
async fn links_rename_target_keeps_draft_links_valid_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let invoice = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    check(
        invoice.get("customer") == &Value::Id(customer.id()),
        "the invoice links to the customer's record id",
    )?;

    let renamed = h
        .rename(
            &inv("l3:link-rn"),
            "customer",
            customer.id(),
            "Widgco",
            cur(&customer),
        )
        .await?
        .value;
    check(
        renamed.get("customer_name") == &text("Widgco"),
        "the rename changed the target's name",
    )?;
    check(
        h.stored("customer", customer.id())
            .map(|d| d.get("customer_name") == &text("Widgco"))
            .unwrap_or(false),
        "the renamed target is stored",
    )?;

    check(
        h.stored("invoice", invoice.id())
            .map(|d| d.get("customer") == &Value::Id(customer.id()))
            .unwrap_or(false),
        "the link kept pointing at the record id",
    )?;
    let still_valid = h
        .update(
            &inv("l3:link-up"),
            "invoice",
            invoice.id(),
            one_field(n("remarks"), text("renamed target still valid")),
            cur(&invoice),
        )
        .await?
        .value;
    check(
        still_valid.get("remarks") == &text("renamed target still valid"),
        "the linked document still validates after the rename",
    )?;
    Ok(())
}

/// ADR-33, ADR-42: renaming a posted document is an operation of its own: it changes only the
/// title field, keeps the document posted, stores the new name and emits exactly one
/// `doc_renamed` event whose payload carries the old and the new name.
async fn links_renaming_a_posted_document_emits_doc_renamed_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let draft = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let posted = h
        .post(&inv("l3:pren-post"), "invoice", draft.id(), cur(&draft))
        .await?
        .value;

    let renamed = h
        .rename(
            &inv("l3:pren"),
            "invoice",
            posted.id(),
            "INV-2",
            cur(&posted),
        )
        .await?
        .value;
    check(
        renamed.state() == Some(DocState::Posted) && renamed.get("number") == &text("INV-2"),
        "the posted document kept its state and got the new name",
    )?;
    check(
        h.stored("invoice", posted.id())
            .map(|d| d.state() == Some(DocState::Posted) && d.get("number") == &text("INV-2"))
            .unwrap_or(false),
        "the rename is stored",
    )?;

    let payloads: Vec<Json> = h
        .outbox()
        .iter()
        .filter(|e| e.topic.as_str() == "doc_renamed")
        .map(|e| serde_json::from_str(&e.payload).unwrap_or(Json::Null))
        .collect();
    check(payloads.len() == 1, "exactly one doc_renamed event")?;
    let rename = payloads.into_iter().next().unwrap_or(Json::Null);
    check(
        rename["data"]["old_name"] == json!("INV-1")
            && rename["data"]["new_name"] == json!("INV-2"),
        "the doc_renamed payload carries the old and the new name",
    )?;
    let invoice_events = h
        .outbox()
        .iter()
        .filter(|e| e.subject.entity.as_str() == "invoice")
        .count();
    check(
        invoice_events == 3,
        "one event per operation on the invoice",
    )?;
    Ok(())
}

/// ADR-33: a document that another document links to cannot be deleted while the reference
/// exists (`document.linked`, a link conflict); deleting the linking document unblocks the
/// delete of the referenced one, and the deletes of unreferenced drafts are allowed.
async fn links_delete_of_referenced_document_unblocks_after_references_gone_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let invoice = new_invoice(h, customer.id(), &[("A", "1")]).await?;

    let err = expect_err(
        h.delete(&inv("l3:ldel"), "customer", customer.id(), cur(&customer))
            .await,
        "document.linked",
    )?;
    check(
        matches!(err.class, ErrorClass::Linked),
        "the refusal is a link conflict, not a state or validation issue",
    )?;
    check(
        h.stored("customer", customer.id()).is_some(),
        "the referenced customer survived",
    )?;

    h.delete(&inv("l3:ldel-2"), "invoice", invoice.id(), cur(&invoice))
        .await?;
    check(
        h.stored("invoice", invoice.id()).is_none(),
        "the linking invoice deleted",
    )?;
    h.delete(&inv("l3:ldel-3"), "customer", customer.id(), cur(&customer))
        .await?;
    check(
        h.stored("customer", customer.id()).is_none(),
        "with the references gone the target deletes",
    )?;
    let deletes = h
        .outbox()
        .iter()
        .filter(|e| e.topic.as_str() == "doc_deleted")
        .count();
    check(
        deletes == 2,
        "each delete emitted exactly one doc_deleted event",
    )?;
    Ok(())
}

/// ADR-33, ADR-51: a link may be created in the same transaction as its target: a
/// committed-phase hook of the customer's create spawns an invoice that links to it, both
/// documents commit together, the link survives the commit, and a later update of the
/// invoice still validates the stored link.
async fn links_link_created_in_the_same_transaction_and_kept_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let log = Log::default();
    // One id for the spawned invoice; the case records the id the hook created.
    let spawned = ids(h, 1);
    let created = Created::default();
    let spawn_created = created.clone();
    let spawn_log = log.clone();
    let spawn_ids = spawned.clone();
    h.register(Arc::new(
        Hooked::new("customer")
            .hook(
                step("l3:spawn_linked_invoice"),
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                hook_fn(move |ctx| {
                    let log = spawn_log.clone();
                    let spawned = spawn_ids.clone();
                    let created = spawn_created.clone();
                    Box::pin(async move {
                        push(&log, "spawn");
                        let invoice_id = take(&spawned)?;
                        note_created(&created, "invoice", invoice_id);
                        let mut invoice = Document::new(n("invoice"), invoice_id);
                        invoice.set(n("number"), text("INV-9"));
                        invoice.set(n("customer"), Value::Id(ctx.doc().id()));
                        ctx.create(invoice).await?;
                        Ok(())
                    })
                }),
            )
            .hook(
                step("l3:customer_save_committed"),
                HookPoint::of(Phase::Committed, Target::Save),
                recorder(log.clone(), "customer"),
            ),
    ));

    let customer = new_customer(h, "Acme").await?;
    check(
        read(&log) == vec!["spawn", "customer:save_committed"],
        "the spawn ran in phase 3 and the commit step in phase 4",
    )?;
    let spawned_invoice = created_of(&created, "invoice")
        .first()
        .copied()
        .ok_or_else(|| Failure("the hook did not report a spawned invoice".to_owned()))?;
    let invoice = h
        .stored("invoice", spawned_invoice)
        .ok_or_else(|| Failure("the linked invoice was committed".to_owned()))?;
    check(
        invoice.get("customer") == &Value::Id(customer.id()),
        "the invoice's link points at the customer created in the same transaction",
    )?;
    check(
        invoice.id() != customer.id(),
        "the link is not the target itself",
    )?;

    h.update(
        &inv("l3:linkkeep"),
        "invoice",
        invoice.id(),
        one_field(n("remarks"), text("link kept")),
        cur(&invoice),
    )
    .await?;
    check(
        h.outbox()
            .iter()
            .filter(|e| e.topic.as_str() == "doc_created")
            .count()
            == 2,
        "one doc_created event for each of the two documents",
    )?;
    Ok(())
}

l3_case!(
    links_rename_target_keeps_draft_links_valid,
    links_rename_target_keeps_draft_links_valid_impl
);
l3_case!(
    links_renaming_a_posted_document_emits_doc_renamed,
    links_renaming_a_posted_document_emits_doc_renamed_impl
);
l3_case!(
    links_delete_of_referenced_document_unblocks_after_references_gone,
    links_delete_of_referenced_document_unblocks_after_references_gone_impl
);
l3_case!(
    links_link_created_in_the_same_transaction_and_kept,
    links_link_created_in_the_same_transaction_and_kept_impl
);

/// ADR-34: a table patch replaces the table. Reorder rows by listing the same row ids in a
/// new order: the rows keep their record ids and values, the document keeps its version
/// chain, and the stored row ids follow the new order.
async fn tables_child_table_reorder_keeps_row_ids_and_values_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let invoice = new_invoice(h, customer.id(), &[("A", "1"), ("B", "2")]).await?;
    let rows = invoice.rows("items");
    check(rows.len() == 2, "the invoice was created with two rows")?;
    let (first_id, second_id) = (rows[0].id, rows[1].id);
    check(
        rows[0].fields.get(&n("item")) == Some(&text("A"))
            && rows[1].fields.get(&n("item")) == Some(&text("B")),
        "the created rows are in the listed order",
    )?;

    let reordered = h
        .update(
            &inv("l3:reorder"),
            "invoice",
            invoice.id(),
            DocumentPatch {
                tables: [(
                    n("items"),
                    vec![
                        RowPatch {
                            id: Some(second_id),
                            fields: Default::default(),
                        },
                        RowPatch {
                            id: Some(first_id),
                            fields: Default::default(),
                        },
                    ],
                )]
                .into(),
                ..Default::default()
            },
            cur(&invoice),
        )
        .await?
        .value;
    let reordered = reordered.rows("items");
    check(reordered.len() == 2, "both rows are still there")?;
    check(reordered[0].id == second_id, "the second row is now first")?;
    check(reordered[1].id == first_id, "the first row is now last")?;
    check(
        reordered[0].fields.get(&n("item")) == Some(&text("B"))
            && reordered[1].fields.get(&n("item")) == Some(&text("A")),
        "the row values moved with their ids",
    )?;
    check(
        h.stored("invoice", invoice.id())
            .map(|d| d.rows("items")[0].id == second_id)
            .unwrap_or(false),
        "the reorder is stored",
    )?;
    Ok(())
}

/// ADR-34: listing only a subset of rows deletes the unlisted row records; adding a row patch
/// with no id inserts a new row record with a fresh id.
async fn tables_child_table_insert_and_delete_rows_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let invoice = new_invoice(h, customer.id(), &[("A", "1"), ("B", "2")]).await?;
    let rows = invoice.rows("items");
    let first_id = rows[0].id;

    let shrunk = h
        .update(
            &inv("l3:rows-del"),
            "invoice",
            invoice.id(),
            DocumentPatch {
                tables: [(
                    n("items"),
                    vec![RowPatch {
                        id: Some(first_id),
                        fields: Default::default(),
                    }],
                )]
                .into(),
                ..Default::default()
            },
            cur(&invoice),
        )
        .await?
        .value;
    let shrunk_rows = shrunk.rows("items");
    check(shrunk_rows.len() == 1, "the unlisted row was deleted")?;
    check(
        shrunk_rows.first().map(|r| r.id) == Some(first_id),
        "the listed row kept its record id",
    )?;

    let new_row = RowPatch {
        id: None,
        fields: [(n("item"), text("C")), (n("qty"), dec("3"))].into(),
    };
    let grown = h
        .update(
            &inv("l3:rows-add"),
            "invoice",
            invoice.id(),
            DocumentPatch {
                tables: [(
                    n("items"),
                    vec![
                        RowPatch {
                            id: Some(first_id),
                            fields: Default::default(),
                        },
                        new_row,
                    ],
                )]
                .into(),
                ..Default::default()
            },
            cur(&shrunk),
        )
        .await?
        .value;
    let rows = grown.rows("items");
    check(rows.len() == 2, "the table has two rows again")?;
    check(rows[0].id == first_id, "the first row kept its id")?;
    check(
        rows[1].id != first_id && rows[1].fields.get(&n("item")) == Some(&text("C")),
        "the inserted row is a new record with its values",
    )?;
    Ok(())
}

/// ADR-34, ADR-57/056: a change that only edits a table's rows (with row ids) lists the
/// table, not its rows, in the event's changed fields.
async fn tables_table_only_changes_list_the_table_in_changed_fields_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let invoice = new_invoice(h, customer.id(), &[("A", "1"), ("B", "2")]).await?;
    let rows = invoice.rows("items");
    let (first_id, second_id) = (rows[0].id, rows[1].id);

    let updated = h
        .update(
            &inv("l3:rows-ev"),
            "invoice",
            invoice.id(),
            DocumentPatch {
                tables: [(
                    n("items"),
                    vec![
                        RowPatch {
                            id: Some(first_id),
                            fields: [(n("qty"), dec("5"))].into(),
                        },
                        RowPatch {
                            id: Some(second_id),
                            fields: [(n("qty"), dec("7"))].into(),
                        },
                    ],
                )]
                .into(),
                ..Default::default()
            },
            cur(&invoice),
        )
        .await?
        .value;
    let outbox = h.outbox();
    let event: Json = outbox
        .iter()
        .rev()
        .find(|e| e.subject.id == invoice.id() && e.topic.as_str() == "doc_updated")
        .and_then(|e| serde_json::from_str(&e.payload).ok())
        .unwrap_or(Json::Null);
    check(
        event["data"]["changed_fields"] == json!(["items"]),
        format!("the event's changed fields name only the table: {event}"),
    )?;
    check(
        updated.rows("items")[0].fields.get(&n("qty")) == Some(&dec("5")),
        "the row store took the row patch",
    )?;
    Ok(())
}

/// ADR-34, ADR-35: a row error names the row by its new position, not by row id.
async fn tables_row_errors_name_rows_by_their_new_position_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let invoice = new_invoice(h, customer.id(), &[("A", "1"), ("B", "2")]).await?;
    let rows = invoice.rows("items");
    let (first_id, second_id) = (rows[0].id, rows[1].id);

    let err = expect_err(
        h.update(
            &inv("l3:rowerr"),
            "invoice",
            invoice.id(),
            DocumentPatch {
                tables: [(
                    n("items"),
                    vec![
                        RowPatch {
                            id: Some(first_id),
                            fields: Default::default(),
                        },
                        RowPatch {
                            id: Some(second_id),
                            fields: [(n("qty"), text("oops"))].into(),
                        },
                        RowPatch {
                            id: None,
                            fields: [(n("item"), text("C")), (n("qty"), text("9x"))].into(),
                        },
                    ],
                )]
                .into(),
                ..Default::default()
            },
            cur(&invoice),
        )
        .await,
        "document.validation_failed",
    )?;
    let named: Vec<(&str, &str)> = err
        .fields
        .iter()
        .map(|f| (f.field.as_str(), f.code.as_str()))
        .collect();
    check(
        named
            == [
                ("items[2].qty", "document.invalid_value"),
                ("items[3].qty", "document.invalid_value"),
            ],
        format!("the row errors name the second and third row by new position: {named:?}"),
    )?;
    check(
        h.stored("invoice", invoice.id())
            .map(|d| d.rows("items")[0].fields.get(&n("qty")) == Some(&dec("1")))
            .unwrap_or(false),
        "the failed change did not persist",
    )?;
    Ok(())
}

/// Only fields the doctype allows on posted documents may change after posting (ADR-11):
/// an update mixing an allowed and a read-only field is refused whole, with the refusal
/// naming the read-only field.
async fn posted_updates_change_only_fields_allowed_on_posted_documents_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let draft = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let posted = h
        .post(&inv("l3:pupd-post"), "invoice", draft.id(), cur(&draft))
        .await?
        .value;
    check(
        posted.version() == Some(Version(2)),
        "the post is version 2",
    )?;

    let updated = h
        .update(
            &inv("l3:pupd-ok"),
            "invoice",
            posted.id(),
            one_field(n("remarks"), text("still allowed")),
            cur(&posted),
        )
        .await?
        .value;
    check(
        updated.state() == Some(DocState::Posted) && updated.version() == Some(Version(3)),
        "an update touching only fields allowed on posted documents goes through",
    )?;

    let mixed = DocumentPatch {
        fields: [
            (n("remarks"), text("second try")),
            (n("total"), dec("1.00")),
        ]
        .into(),
        ..Default::default()
    };
    let err = expect_err(
        h.update(
            &inv("l3:pupd-no"),
            "invoice",
            posted.id(),
            mixed,
            cur(&updated),
        )
        .await,
        "document.read_only_field",
    )?;
    check(
        matches!(err.class, ErrorClass::Validation),
        "the refusal is a validation error",
    )?;
    let named: Vec<(&str, &str)> = err
        .fields
        .iter()
        .map(|f| (f.field.as_str(), f.code.as_str()))
        .collect();
    check(
        named == [("total", "document.read_only_field")],
        format!("the read-only field was named: {named:?}"),
    )?;
    check(
        h.stored("invoice", posted.id())
            .map(|d| d.version() == Some(Version(3)) && d.get("remarks") == &text("still allowed"))
            .unwrap_or(false),
        "the refused change did not persist, not even its allowed field",
    )?;
    Ok(())
}

/// An update on a posted document emits `doc_updated_posted` (never `doc_updated`), and the
/// event's payload data carries the field-level change for the topic's schema.
async fn posted_updates_emit_doc_updated_posted_impl(fixture: &dyn Fixture) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let draft = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let posted = h
        .post(&inv("l3:puev-post"), "invoice", draft.id(), cur(&draft))
        .await?
        .value;

    let id = posted.id();
    let updated = h
        .update(
            &inv("l3:puev"),
            "invoice",
            id,
            one_field(n("remarks"), text("posted note")),
            cur(&posted),
        )
        .await?
        .value;
    check(
        updated.version() == Some(Version(3)) && updated.state() == Some(DocState::Posted),
        "the posted update bumped the version and kept the state",
    )?;

    let outbox = h.outbox();
    let on = |topic: &str| {
        outbox
            .iter()
            .filter(|e| e.subject.id == id && e.topic.as_str() == topic)
            .count()
    };
    check(
        on("doc_updated_posted") == 1,
        "the posted update emitted doc_updated_posted",
    )?;
    check(on("doc_updated") == 0, "it did not emit doc_updated")?;
    let event: Json = outbox
        .iter()
        .find(|e| e.subject.id == id && e.topic.as_str() == "doc_updated_posted")
        .and_then(|e| serde_json::from_str(&e.payload).ok())
        .unwrap_or(Json::Null);
    check(
        event["data"]["changed_fields"] == json!(["remarks"]),
        "the payload named the field that changed",
    )?;
    check(
        event["data"]["name"] == json!("INV-1"),
        "the payload carried the document's name",
    )?;
    check(
        outbox.iter().any(|e| {
            e.subject.id == id
                && e.topic.as_str() == "doc_updated_posted"
                && e.subject_version == Some(Version(3))
        }),
        "the event carried the new version",
    )?;
    Ok(())
}

/// A posted document keeps its state through a rename: it bumps the version, changes the
/// title field, and emits exactly one `doc_renamed` (and no update-family event).
async fn posted_renaming_a_posted_document_keeps_it_posted_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let draft = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let id = draft.id();
    let posted = h
        .post(&inv("l3:prn-post"), "invoice", id, cur(&draft))
        .await?
        .value;
    let renamed = h
        .rename(&inv("l3:prn"), "invoice", id, "INV-2", cur(&posted))
        .await?
        .value;
    check(
        renamed.state() == Some(DocState::Posted) && renamed.version() == Some(Version(3)),
        "the posted document kept its state and got the new version",
    )?;
    check(
        renamed.get("number") == &text("INV-2"),
        "the rename changed the title field",
    )?;
    check(
        h.stored("invoice", id)
            .map(|d| d.state() == Some(DocState::Posted) && d.version() == Some(Version(3)))
            .unwrap_or(false),
        "the rename is stored",
    )?;
    let outbox = h.outbox();
    let on = |topic: &str| {
        outbox
            .iter()
            .filter(|e| e.subject.id == id && e.topic.as_str() == topic)
            .count()
    };
    check(on("doc_renamed") == 1, "exactly one doc_renamed event")?;
    check(
        on("doc_updated") == 0 && on("doc_updated_posted") == 0,
        "the rename emitted no update-family event",
    )?;
    Ok(())
}

/// ADR-21: posted and cancelled documents cannot be deleted; a draft of the same doctype
/// can. Nothing is removed and no delete event fires for a refused state.
async fn posted_and_cancelled_documents_cannot_be_deleted_impl(
    fixture: &dyn Fixture,
) -> CaseResult {
    let host = fixture.fresh();
    define_catalog(&*host);
    let h = &*host;

    let customer = new_customer(h, "Acme").await?;
    let to_post = new_invoice(h, customer.id(), &[("A", "1")]).await?;
    let posted = h
        .post(&inv("l3:pdel-post"), "invoice", to_post.id(), cur(&to_post))
        .await?
        .value;
    check(
        h.delete(&inv("l3:pdel-1"), "invoice", posted.id(), cur(&posted))
            .await
            .is_err(),
        "the posted document cannot be deleted",
    )?;
    let cancelled = h
        .correct(
            &inv("l3:pdel-correct"),
            "invoice",
            posted.id(),
            cur(&posted),
        )
        .await?
        .value;
    check(
        cancelled.state() == Some(DocState::Draft),
        "correcting a posted document cancels it and creates an amended draft",
    )?;
    let original_after = h
        .stored("invoice", posted.id())
        .ok_or_else(|| Failure("the cancelled original is still stored".to_owned()))?;
    check(
        h.delete(
            &inv("l3:pdel-2"),
            "invoice",
            posted.id(),
            cur(&original_after),
        )
        .await
        .is_err(),
        "the cancelled document cannot be deleted",
    )?;
    let before = h.outbox().len();
    let err = expect_err(
        h.delete(
            &inv("l3:pdel-3"),
            "invoice",
            posted.id(),
            cur(&original_after),
        )
        .await,
        "document.invalid_state",
    )?;
    check(
        err.fields.is_empty() && matches!(err.class, ErrorClass::InvalidState),
        "the refusal names the gate, not a field",
    )?;
    check(
        h.outbox().len() == before,
        "the refused delete emitted no event",
    )?;
    check(
        h.stored("invoice", posted.id()).is_some(),
        "the cancelled document is still stored",
    )?;

    let draft = new_invoice(h, customer.id(), &[("B", "2")]).await?;
    let id = draft.id();
    h.delete(&inv("l3:pdel-ok"), "invoice", id, cur(&draft))
        .await?;
    check(h.stored("invoice", id).is_none(), "the draft was deleted")?;
    Ok(())
}

l3_case!(
    tables_child_table_reorder_keeps_row_ids_and_values,
    tables_child_table_reorder_keeps_row_ids_and_values_impl
);
l3_case!(
    tables_child_table_insert_and_delete_rows,
    tables_child_table_insert_and_delete_rows_impl
);
l3_case!(
    tables_table_only_changes_list_the_table_in_changed_fields,
    tables_table_only_changes_list_the_table_in_changed_fields_impl
);
l3_case!(
    tables_row_errors_name_rows_by_their_new_position,
    tables_row_errors_name_rows_by_their_new_position_impl
);
l3_case!(
    posted_updates_change_only_fields_allowed_on_posted_documents,
    posted_updates_change_only_fields_allowed_on_posted_documents_impl
);
l3_case!(
    posted_updates_emit_doc_updated_posted,
    posted_updates_emit_doc_updated_posted_impl
);
l3_case!(
    posted_renaming_a_posted_document_keeps_it_posted,
    posted_renaming_a_posted_document_keeps_it_posted_impl
);
l3_case!(
    posted_and_cancelled_documents_cannot_be_deleted,
    posted_and_cancelled_documents_cannot_be_deleted_impl
);
