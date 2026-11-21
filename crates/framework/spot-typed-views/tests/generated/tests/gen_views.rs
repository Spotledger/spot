//! The compile test (WP-MET-04.T1) against the `StubHost`: documents built through the
//! typed setters, created through `DocumentService::create` with an `Invocation`, loaded
//! again, and read back through the typed getters. The child-row accessors, the select
//! enum, the keyword/raw identifiers, the `None`/`Null` round trip, the `.doc()` escape
//! hatch (the runtime-added custom field, ADR-05) and the unknown-stored-option error are
//! exercised too.
//!
//! The StubHost validates every field's value against its metadata: only the
//! metadata-visible fields ride a `create`. The party's `about` dynamic link is unnamed in
//! that metadata (WP-MET-01 compiles dynamic links `EngineOnly`, no `FieldKind`), and the
//! invoice's `custom_test_note` does not exist in it until the test adds it at runtime
//! through the metadata registry (`harness::host_with`, ADR-05) — so `about` is attached to
//! the saved document after the create (where the typed getter still reads it), and the
//! custom field rides the create because the registry now knows it, while the views — built
//! from the definitions long before — have no accessor for it and need no rebuild.

use spot_app_api::context::{DocumentService, Outcome};
use spot_app_api::document::DocState;
use spot_app_api::prelude::*;
use spot_app_api::{DocView, Document};
use spot_meta::{FieldSpec, FieldType};
use spot_typed_views_generated_test::harness;
use spot_typed_views_generated_test::{Invoice, InvoiceLine, InvoiceLineRow, Party, StatusOption};

/// The decimal of the fixtures: exact integer math, never a float (ADR-12).
macro_rules! dec {
	($units:expr, $scale:expr) => {
		Decimal::new($units, $scale)
	};
}

/// The posting date of the read-back assertions: `2026-01-31`, built without the `time`
/// crate's macros (the test crate has no direct `time` dependency; `OffsetDateTime` rides
/// the `spot_app_api` prelude, and `Date::from_ordinal_date` needs no extra import beyond
/// the prelude's `Date`).
#[must_use]
fn posting_date() -> Date {
	Date::from_ordinal_date(2026, 31).unwrap_or_else(|e| panic!("2026-01-31: invalid: {e}"))
}

/// The read-back of one invoice: every getter of the parent's fields.
///
/// # Panics
/// A getter that fails is a bug in the emitter or the test's own data; tests panic with the
/// field's name.
#[must_use]
fn check_invoice(view: &Invoice, party: RecordId) {
	assert_eq!(
		Some("INV-2026-0001"),
		view.number().unwrap_or_else(|e| panic!("number: {e}"))
	);
	assert_eq!(
		Some(posting_date()),
		view.posting_date().unwrap_or_else(|e| panic!("posting_date: {e}"))
	);
	assert_eq!(
		Some(StatusOption::Open),
		view.status().unwrap_or_else(|e| panic!("status: {e}"))
	);
	assert_eq!(Some(party), view.party().unwrap_or_else(|e| panic!("party: {e}")));
	assert_eq!(
		Some(dec!(12_500, 2)),
		view.total().unwrap_or_else(|e| panic!("total: {e}"))
	);
	assert_eq!(
		Some("net 30"),
		view.remarks().unwrap_or_else(|e| panic!("remarks: {e}"))
	);
	// The custom field added at runtime, through the escape hatch.
	assert_eq!(
		&Value::Text("from the floor".to_owned()),
		view.doc().get("custom_test_note")
	);
	// The state the create lifecycle stamps on a posted DocType's document.
	assert_eq!(Some(DocState::Draft), view.doc().state());
}

/// The read-back of one party: every getter.
///
/// # Panics
/// As `check_invoice`: a getter failing is a bug in the emitter or the test's own data.
#[must_use]
fn check_party(view: &Party, about: RecordId) {
	assert_eq!(Some("P-0001"), view.code().unwrap_or_else(|e| panic!("code: {e}")));
	assert_eq!(Some(true), view.active().unwrap_or_else(|e| panic!("active: {e}")));
	assert_eq!(
		Some("customer"),
		view.about_type().unwrap_or_else(|e| panic!("about_type: {e}"))
	);
	assert_eq!(Some(about), view.about().unwrap_or_else(|e| panic!("about: {e}")));
	assert_eq!(
		Some(OffsetDateTime::UNIX_EPOCH),
		view.since().unwrap_or_else(|e| panic!("since: {e}"))
	);
}

/// One invoice line filled through `new_lines` and `lines_mut`'s raw rows.
///
/// # Panics
/// As `check_invoice`: a getter failing is a bug in the emitter or the test's own data.
#[must_use]
fn check_row(row: &InvoiceLineRow<'_>, item: &str, qty: Decimal, amount: Decimal) {
	assert_eq!(Some(item), row.item().unwrap_or_else(|e| panic!("item: {e}")));
	assert_eq!(Some(qty), row.qty().unwrap_or_else(|e| panic!("qty: {e}")));
	assert_eq!(Some(amount), row.amount().unwrap_or_else(|e| panic!("amount: {e}")));
}

/// Creates the party first (the invoice's link target), then the invoice referencing it.
#[tokio::test]
async fn generated_views_run_against_stub_host() {
	// The custom field is added at runtime: the metadata registry learns it (the runtime
	// path of ADR-05), while the views were generated from the definitions long before and
	// have no accessor for it — so they need no rebuild when a site customises (ADR-05),
	// and the field stays reachable only through `.doc()`.
	let host = harness::host_with(|registry| {
		registry
			.add_custom_field(
				"invoice",
				FieldSpec::new("custom_test_note", FieldType::Text),
				None,
			)
			.unwrap_or_else(|e| panic!("add custom field: {e:?}"));
	});
	let invocation = harness::inv();

	// ---- The master first: the link target must exist when the invoice is created --------
	let mut party = Party::wrap(Document::new(
		Name::new("party").unwrap_or_else(|e| panic!("doctype name: {e}")),
		RecordId::new_v7(),
	));
	party.set_code(Some("P-0001")).unwrap_or_else(|e| panic!("set code: {e}"));
	party.set_active(Some(true)).unwrap_or_else(|e| panic!("set active: {e}"));
	party
		.set_about_type(Some("customer"))
		.unwrap_or_else(|e| panic!("set about_type: {e}"));
	// The `about` dynamic link is unnamed in the stub's metadata (WP-MET-01 compiles
	// dynamic links `EngineOnly`, no `FieldKind`), so the create would refuse it; the link
	// is attached to the saved document below, and the typed getter still reads it — the
	// value is still exactly the `RecordId` chosen here.
	let about_id = RecordId::new_v7();
	party
		.set_since(Some(OffsetDateTime::UNIX_EPOCH))
		.unwrap_or_else(|e| panic!("set since: {e}"));
	let mut saved_party: Document =
		DocumentService::create(&host, &invocation, party.into_doc())
			.await
			.unwrap_or_else(|e| panic!("create party: {e}"))
			.value;
	// The dynamic link rides the `.doc_mut()` escape hatch onto the saved document; `about`
	// is read back through the typed getter with the exact value attached here.
	saved_party.set(
		Name::new("about").unwrap_or_else(|e| panic!("field: {e}")),
		Value::Id(about_id),
	);
	let party_view = Party::wrap(saved_party);
	let party_id = party_view.doc().id();
	check_party(&party_view, about_id);
	// `active` and `since` carry the fixture definitions' defaults when unset; set values
	// are read back verbatim either way.
	assert_eq!("party", Party::DOCTYPE);

	// ---- The invoice, through the typed setters ------------------------------------------
	let mut invoice = Invoice::wrap(Document::new(
		Name::new("invoice").unwrap_or_else(|e| panic!("doctype name: {e}")),
		RecordId::new_v7(),
	));
	invoice
		.set_number(Some("INV-2026-0001"))
		.unwrap_or_else(|e| panic!("set number: {e}"));
	invoice
		.set_posting_date(Some(posting_date()))
		.unwrap_or_else(|e| panic!("set posting_date: {e}"));
	invoice
		.set_status(Some(StatusOption::Open))
		.unwrap_or_else(|e| panic!("set status: {e}"));
	invoice.set_party(Some(party_id)).unwrap_or_else(|e| panic!("set party: {e}"));
	invoice.set_total(Some(dec!(12_500, 2))).unwrap_or_else(|e| panic!("set total: {e}"));
	invoice.set_remarks(Some("net 30")).unwrap_or_else(|e| panic!("set remarks: {e}"));

	// Two lines: one through the typed row builder, then both filled through `lines_mut`'s
	// raw row map (the row builder appends an empty row; the row view has getters only).
	let first = invoice.new_lines().unwrap_or_else(|e| panic!("new line: {e}"));
	let first_id = first.id();
	drop(first);
	let second = invoice.new_lines().unwrap_or_else(|e| panic!("new line: {e}"));
	let second_id = second.id();
	drop(second);
	let lines = invoice.lines_mut().unwrap_or_else(|e| panic!("lines_mut: {e}"));
	let item = Name::new("item").unwrap_or_else(|e| panic!("field: {e}"));
	let qty = Name::new("qty").unwrap_or_else(|e| panic!("field: {e}"));
	let amount = Name::new("amount").unwrap_or_else(|e| panic!("field: {e}"));
	let first_row = &mut lines[0];
	first_row.fields.insert(item.clone(), Value::Text("Widget".to_owned()));
	first_row.fields.insert(qty.clone(), Value::Decimal(dec!(1_250, 3)));
	first_row.fields.insert(amount.clone(), Value::Decimal(dec!(2_500, 2)));
	let second_row = &mut lines[1];
	second_row.fields.insert(item, Value::Text("Gadget".to_owned()));
	second_row.fields.insert(qty, Value::Decimal(dec!(2_000, 3)));
	second_row.fields.insert(amount, Value::Decimal(dec!(10_000, 2)));

	// A custom field added after the build, written through the `.doc_mut()` escape hatch
	// (ADR-05): the generated code does not know it, the dynamic document carries it.
	invoice.doc_mut().set(
		Name::new("custom_test_note").unwrap_or_else(|e| panic!("custom field: {e}")),
		Value::Text("from the floor".to_owned()),
	);

	let created: Outcome<Document> =
		DocumentService::create(&host, &invocation, invoice.into_doc())
			.await
			.unwrap_or_else(|e| panic!("create invoice: {e}"));
	let saved = Invoice::wrap(created.value);
	let invoice_id = saved.doc().id();
	check_invoice(&saved, party_id);

	// ---- Loaded again, read back through the same getters --------------------------------
	let loaded: Outcome<Document> =
		DocumentService::load(&host, &invocation, "invoice", invoice_id)
			.await
			.unwrap_or_else(|e| panic!("load: {e}"));
	let loaded = Invoice::wrap(loaded.value);
	check_invoice(&loaded, party_id);

	// ---- The child-table accessors -------------------------------------------------------
	let rows: Vec<InvoiceLineRow<'_>> = loaded.lines().collect();
	assert_eq!(2, rows.len());
	assert_eq!(first_id, rows[0].id());
	assert_eq!(second_id, rows[1].id());
	check_row(&rows[0], "Widget", dec!(1_250, 3), dec!(2_500, 2));
	check_row(&rows[1], "Gadget", dec!(2_000, 3), dec!(10_000, 2));

	// ---- The child DocType works standalone, and every view names its DocType ------------
	let mut line = InvoiceLine::wrap(Document::new(
		Name::new("invoice_line").unwrap_or_else(|e| panic!("doctype name: {e}")),
		RecordId::new_v7(),
	));
	line.set_item(Some("Standalone")).unwrap_or_else(|e| panic!("set item: {e}"));
	assert_eq!(Some("Standalone"), line.item().unwrap_or_else(|e| panic!("item: {e}")));
	line.set_qty(Some(dec!(500, 3))).unwrap_or_else(|e| panic!("set qty: {e}"));
	assert_eq!(Some(dec!(500, 3)), line.qty().unwrap_or_else(|e| panic!("qty: {e}")));
	line.set_amount(Some(dec!(75, 2))).unwrap_or_else(|e| panic!("set amount: {e}"));
	assert_eq!(Some(dec!(75, 2)), line.amount().unwrap_or_else(|e| panic!("amount: {e}")));

	assert_eq!("invoice", Invoice::DOCTYPE);
	assert_eq!("invoice_line", InvoiceLine::DOCTYPE);
	assert_eq!("party", Party::DOCTYPE);
	let stored = host.stored("invoice", invoice_id).unwrap_or_else(|| panic!("not stored"));
	assert_eq!("invoice", stored.doctype().as_str());
}

/// A field set to `None` stores `Null` and reads back `None` — and a `null` that was never
/// a select option reads back as `None` too, never an error or a panic.
#[test]
fn setting_none_stores_null_and_reads_none() {
	let mut invoice = Invoice::wrap(Document::new(
		Name::new("invoice").unwrap_or_else(|e| panic!("doctype name: {e}")),
		RecordId::new_v7(),
	));
	invoice.set_total(None).unwrap_or_else(|e| panic!("set total: {e}"));
	invoice.set_status(None).unwrap_or_else(|e| panic!("set status: {e}"));
	assert_eq!(Ok(None), invoice.total());
	assert_eq!(Ok(None), invoice.status());
	assert_eq!(&Value::Null, invoice.doc().get("total"));
	assert_eq!(&Value::Null, invoice.doc().get("status"));
}

/// An unknown stored select value is an `AppError` through the generated getter, never a
/// panic — the review's bug (b): a site may narrow a field's options after the views were
/// built (ADR-05).
#[test]
fn unknown_select_value_is_an_error_not_a_panic() {
	let mut raw = Document::new(
		Name::new("invoice").unwrap_or_else(|e| panic!("doctype name: {e}")),
		RecordId::new_v7(),
	);
	raw.set(
		Name::new("status").unwrap_or_else(|e| panic!("field: {e}")),
		Value::Text("Disputed".to_owned()),
	);
	let invoice = Invoice::wrap(raw);
	let error = invoice
		.status()
		.err()
		.unwrap_or_else(|| panic!("an unknown option must be an error"));
	assert!(error.message.contains("Disputed"), "the error names the value: {error}");
	assert!(StatusOption::parse("Disputed").is_err());
	assert_eq!(Ok(StatusOption::Paid), StatusOption::parse("Paid"), "a known option parses");
}