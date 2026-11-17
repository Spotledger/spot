//! Channel `Print`: `spot-http`'s `GuardedPrintPort`, the print adapter that loads the document as
//! the caller, redacts it through the field guard and only then renders (WP-SEC-04.T12). The
//! renderer here lays every field and child row it is given out as text, once as PDF bytes and once
//! as HTML, so anything the guard let through is in the output; the real `spot-print` renderer is
//! the host's and sees exactly this redacted context.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use spot_app_api::Document;
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{Name, Value};
use spot_core::SiteId;
use spot_field_guard::leakcheck::{Output, assert_no_leak, find_leaks};
use spot_http::api::print::{
    GuardedPrintPort, PdfBytes, PrintInput, PrintPort, PrintPortError, PrintRenderer, PrintSource,
};
use spot_http::api::service::{BoxFuture, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_permissions::DocFacts;
use spot_storage_contract::RecordId;

/// The document as the engine would load it: every protected value planted, a child table of
/// two rows carrying the bank fields.
fn planted(corpus: &spot_field_guard::leakcheck::Corpus) -> Document {
    let value = |label: &str| Value::Text(corpus.get(label).unwrap().value().to_owned());
    let mut doc = Document::new(Name::new(DOCTYPE).unwrap(), RecordId::new_v7());
    for (field, v) in [
        ("number", Value::Text(PLAIN_NUMBER.to_owned())),
        ("notes", Value::Text(PLAIN_NOTE.to_owned())),
        ("card_number", value("card")),
        ("holder", value("lookalike")),
        ("profile_json", value("in_json")),
        ("salary", value("salary")),
        ("annual_cost", value("annual")),
    ] {
        doc.set(Name::new(field).unwrap(), v);
    }
    for memo in &corpus.get("split_rows").unwrap().planted {
        doc.rows_mut(Name::new("items").unwrap()).push(ChildRow {
            id: RecordId::new_v7(),
            fields: [
                (Name::new("iban").unwrap(), value("iban")),
                (Name::new("memo").unwrap(), Value::Text(memo.clone())),
            ]
            .into(),
        });
    }
    doc
}

struct LoadAsCaller {
    corpus: spot_field_guard::leakcheck::Corpus,
}

impl PrintSource for LoadAsCaller {
    fn load<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        _doctype: &'a str,
        _id: RecordId,
    ) -> BoxFuture<'a, Result<PrintInput, PrintPortError>> {
        Box::pin(async move {
            // The credential's token names the principal the test signs in as.
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .ok_or_else(PrintPortError::not_found)?;
            Ok(PrintInput {
                document: planted(&self.corpus),
                principal: principal(&token),
                facts: DocFacts::default(),
            })
        })
    }
}

/// Lays out every field and row it is given, as the text a PDF would carry and as HTML.
struct TextLayout {
    html: Arc<Mutex<Vec<String>>>,
}

impl TextLayout {
    fn lines(document: &Document) -> Vec<String> {
        let mut lines: Vec<String> = document
            .fields()
            .iter()
            .map(|(name, value)| format!("{}: {value:?}", name.as_str()))
            .collect();
        for (table, rows) in document.tables() {
            for row in rows {
                for (name, value) in &row.fields {
                    lines.push(format!("{}.{}: {value:?}", table.as_str(), name.as_str()));
                }
            }
        }
        lines
    }
}

impl PrintRenderer for TextLayout {
    fn render<'a>(
        &'a self,
        document: &'a Document,
        _format: Option<&'a str>,
    ) -> BoxFuture<'a, Result<PdfBytes, PrintPortError>> {
        Box::pin(async move {
            let lines = Self::lines(document);
            let html = lines
                .iter()
                .map(|line| format!("<tr><td>{line}</td></tr>"))
                .collect::<String>();
            self.html
                .lock()
                .unwrap()
                .push(format!("<table>{html}</table>"));
            Ok(format!("%PDF-1.4\n{}\n%%EOF\n", lines.join("\n")).into_bytes())
        })
    }
}

fn ctx(token: &str) -> RequestCtx {
    RequestCtx {
        site: SiteId::new("leak.example").unwrap(),
        credential: Some(Credential::new(token)),
        idempotency_key: None,
        language: None,
        request_id: RequestId::generate(),
        peer: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn print_never_leaks() {
    let core = core();
    let corpus = corpus();
    let html = Arc::new(Mutex::new(Vec::new()));
    let port = GuardedPrintPort::new(
        Arc::new(LoadAsCaller {
            corpus: corpus.clone(),
        }),
        Arc::new(TextLayout { html: html.clone() }),
        core.guard.clone(),
    );
    for name in PRINCIPALS {
        html.lock().unwrap().clear();
        // Print needs the `report` right for a value; below that it prints the masked form.
        let visible = visible_labels(name, true);
        let forbidden = corpus.without(&visible);
        let pdf = port
            .render(&ctx(name), DOCTYPE, RecordId::new_v7(), None)
            .await
            .unwrap();
        assert!(pdf.starts_with(b"%PDF-"), "{name}");
        let mut outputs = vec![Output::Bytes(pdf)];
        outputs.extend(html.lock().unwrap().iter().cloned().map(Output::Text));
        assert_eq!(outputs.len(), 2, "{name}: one render, one PDF and one HTML");

        // The positive control: a principal who may report a field gets its value on the page.
        for label in &visible {
            let seen: Vec<String> = find_leaks(&corpus.only(&[label]), &outputs)
                .into_iter()
                .map(|l| l.secret)
                .collect();
            assert!(
                seen.iter().any(|s| s == label),
                "{name}: `{label}` should be printed but is not"
            );
        }
        assert_no_leak(&forbidden, &outputs);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_print_the_caller_may_not_read_is_refused_before_anything_is_rendered() {
    let core = core();
    let corpus = corpus();
    let html = Arc::new(Mutex::new(Vec::new()));
    let port = GuardedPrintPort::new(
        Arc::new(LoadAsCaller { corpus }),
        Arc::new(TextLayout { html: html.clone() }),
        core.guard.clone(),
    );
    // No credential: the source refuses and the renderer is never reached.
    let mut anonymous = ctx("clerk");
    anonymous.credential = None;
    let error = port
        .render(&anonymous, DOCTYPE, RecordId::new_v7(), None)
        .await
        .unwrap_err();
    assert_eq!(error, PrintPortError::not_found());
    assert!(html.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_unguarded_layout_prints_every_value_so_the_harness_is_not_vacuous() {
    let corpus = corpus();
    let html = Arc::new(Mutex::new(Vec::new()));
    let layout = TextLayout { html };
    let pdf = layout.render(&planted(&corpus), None).await.unwrap();
    assert!(!find_leaks(&corpus, &[Output::Bytes(pdf)]).is_empty());
}

#[test]
fn the_pending_entry_for_print_is_gone() {
    use spot_channel_leakcheck::{Coverage, coverage_of};
    use spot_field_guard::Channel;
    assert_eq!(
        coverage_of(Channel::Print),
        Some(Coverage::Test {
            test: "print_never_leaks",
            producer: "spot-http"
        })
    );
}
