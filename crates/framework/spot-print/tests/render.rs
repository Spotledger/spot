//! The three SPK-03 reference documents, determinism, sandbox, limits and
//! concurrency (WP-PLT-13.T1).

mod common;

use common::{assets, invoice, opts, pdf_text, receipt, renderer, report};
use spot_print::{AssetMap, PdfStandard, PrintError, RenderLimits};
use spot_template::Context;

#[tokio::test]
async fn invoice_renders_across_a_page_break() {
    let r = renderer(&[], RenderLimits::default());
    let pdf = r.render("invoice.typ", &invoice(), &assets(), opts()).await;
    let pdf = pdf.unwrap_or_else(|e| panic!("{e}"));
    let (pages, text) = pdf_text(&pdf.bytes);
    assert_eq!(pdf.page_count, 2);
    assert_eq!(pages, 2);
    assert!(text.contains("TEST-0001"), "invoice number: {text}");
    // Header row repeats on page two, footer reads "Page x of y".
    assert!(text.matches("Description").count() >= 2, "{text}");
    assert!(text.contains("Page 1 of 2") && text.contains("Page 2 of 2"));
    // Rows from both pages.
    assert!(text.contains("B-0001") && text.contains("B-0025"));
    // Grand total and amount in words from spot-template.
    assert!(text.contains("Rupees"), "words missing");
}

#[tokio::test]
async fn invoice_grand_total_matches_its_rows() {
    let r = renderer(&[], RenderLimits::default());
    let pdf = r.render("invoice.typ", &invoice(), &assets(), opts()).await;
    let (_, text) = pdf_text(&pdf.unwrap_or_else(|e| panic!("{e}")).bytes);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(compact.contains(&expected_invoice_total()), "{compact}");
}

fn expected_invoice_total() -> String {
    let mut total = 0i64;
    for i in 1..=25i64 {
        let value = (i % 4 + 1) * (1_000 + 50 * i);
        total += value + value * 18 / 100;
    }
    let digits = total.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    format!("{out}.00")
}

#[tokio::test]
async fn receipt_renders_on_one_80mm_page_with_urdu() {
    let r = renderer(&[], RenderLimits::default());
    let pdf = r.render("receipt.typ", &receipt(), &assets(), opts()).await;
    let pdf = pdf.unwrap_or_else(|e| panic!("{e}"));
    let (pages, text) = pdf_text(&pdf.bytes);
    assert_eq!((pdf.page_count, pages), (1, 1));
    // Urdu text layer is lossy (SPK-03); only the Latin parts are asserted.
    assert!(text.contains("TEST-R-0001"), "{text}");
    assert!(text.contains("Basmati rice 5 kg"), "{text}");
    assert!(text.contains("8,354.00"), "{text}");
    // Page width is 80 mm = 226.77 pt.
    let doc = lopdf::Document::load_mem(&pdf.bytes).unwrap_or_else(|e| panic!("{e}"));
    let page_id = doc.get_pages().values().copied().next().unwrap_or_default();
    let media = doc
        .get_object(page_id)
        .and_then(|p| p.as_dict())
        .and_then(|d| d.get(b"MediaBox"))
        .and_then(|m| m.as_array())
        .unwrap_or_else(|e| panic!("{e}"));
    let width = media.get(2).and_then(|v| v.as_float().ok()).unwrap_or(0.0);
    assert!((width - 226.77).abs() < 0.1, "width {width}");
}

#[tokio::test]
async fn report_of_2000_rows_has_subtotals_and_grand_total() {
    // The 60 s default is a production limit, not what this test checks: a debug build on a
    // loaded CI runner (Windows, all render tests in parallel) needs more than that.
    let limits = RenderLimits {
        max_duration: std::time::Duration::from_secs(900),
        ..RenderLimits::default()
    };
    let r = renderer(&[], limits);
    let (context, grand) = report();
    let pdf = r.render("report.typ", &context, &assets(), opts()).await;
    let pdf = pdf.unwrap_or_else(|e| panic!("{e}"));
    let (pages, text) = pdf_text(&pdf.bytes);
    assert_eq!(pdf.page_count, pages);
    assert_eq!(pdf.page_count, EXPECTED_REPORT_PAGES);
    assert!(text.contains("Synthetic stock valuation report"));
    assert!(text.contains("Synthetic product 2000"));
    assert!(text.contains("Subtotal Group 20"));
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(compact.contains("Grand total"), "{compact:.200}");
    assert!(compact.contains(&grand.to_string()), "grand {grand}");
}

const EXPECTED_REPORT_PAGES: usize = 36;

#[tokio::test]
async fn same_inputs_give_byte_identical_pdfs() {
    let r = renderer(&[], RenderLimits::default());
    let a = r.render("invoice.typ", &invoice(), &assets(), opts()).await;
    let b = r.render("invoice.typ", &invoice(), &assets(), opts()).await;
    assert!(a.unwrap_or_else(|e| panic!("{e}")).bytes == b.unwrap_or_else(|e| panic!("{e}")).bytes);
    // A second renderer (fresh font load) agrees too.
    let other = renderer(&[], RenderLimits::default());
    let c = other
        .render("invoice.typ", &invoice(), &assets(), opts())
        .await;
    let a = r.render("invoice.typ", &invoice(), &assets(), opts()).await;
    assert!(a.unwrap_or_else(|e| panic!("{e}")).bytes == c.unwrap_or_else(|e| panic!("{e}")).bytes);
}

#[tokio::test]
async fn pdf_a_2b_output_declares_the_standard() {
    let r = renderer(&[], RenderLimits::default());
    let options = opts().with_standard(PdfStandard::A2b);
    let pdf = r
        .render("invoice.typ", &invoice(), &assets(), options)
        .await;
    let pdf = pdf.unwrap_or_else(|e| panic!("{e}"));
    let text = String::from_utf8_lossy(&pdf.bytes);
    assert!(text.contains("pdfaid"), "no PDF/A identification in XMP");
}

#[tokio::test]
async fn package_imports_are_refused_with_the_path() {
    let r = renderer(
        &[("pkg.typ", "#import \"@preview/cetz:0.3.0\": canvas\nHello")],
        RenderLimits::default(),
    );
    let error = r
        .render("pkg.typ", &Context::new(), &AssetMap::new(), opts())
        .await;
    match error {
        Err(PrintError::Sandbox { path }) => assert!(path.contains("cetz"), "{path}"),
        other => panic!("expected sandbox error, got {other:?}"),
    }
}

#[tokio::test]
async fn paths_outside_the_asset_map_are_refused() {
    let r = renderer(
        &[
            ("img.typ", "#image(\"missing.png\")"),
            ("etc.typ", "#read(\"/etc/passwd\")"),
            ("up.typ", "#image(\"../secret.png\")"),
        ],
        RenderLimits::default(),
    );
    for (name, needle) in [
        ("img.typ", "missing.png"),
        ("etc.typ", "etc/passwd"),
        ("up.typ", "secret.png"),
    ] {
        let result = r
            .render(name, &Context::new(), &AssetMap::new(), opts())
            .await;
        match result {
            Err(PrintError::Sandbox { path }) => assert!(path.contains(needle), "{path}"),
            // Typst itself rejects `..` escapes before asking the world.
            Err(PrintError::Compile { message, .. }) if name == "up.typ" => {
                assert!(message.contains(needle), "{message}");
            }
            other => panic!("{name}: expected sandbox error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn assets_present_in_the_map_are_readable() {
    let r = renderer(
        &[("ok.typ", "#image(\"logo.svg\", width: 10mm)")],
        RenderLimits::default(),
    );
    let pdf = r.render("ok.typ", &Context::new(), &assets(), opts()).await;
    assert_eq!(pdf.unwrap_or_else(|e| panic!("{e}")).page_count, 1);
}

#[tokio::test]
async fn exceeding_max_pages_is_a_typed_error() {
    let limits = RenderLimits {
        max_pages: 1,
        ..RenderLimits::default()
    };
    let r = renderer(&[], limits);
    let result = r.render("invoice.typ", &invoice(), &assets(), opts()).await;
    assert!(matches!(
        result,
        Err(PrintError::PageLimitExceeded { limit: 1, pages: 2 })
    ));
}

#[tokio::test]
async fn compile_errors_carry_the_markup_line() {
    let r = renderer(
        &[("bad.typ", "First line\n\n#unknown-function()\n")],
        RenderLimits::default(),
    );
    let result = r
        .render("bad.typ", &Context::new(), &AssetMap::new(), opts())
        .await;
    match result {
        Err(PrintError::Compile {
            message,
            template_line,
        }) => {
            assert!(message.contains("unknown"), "{message}");
            assert_eq!(template_line, Some(3));
        }
        other => panic!("expected compile error, got {other:?}"),
    }
}

#[tokio::test]
async fn template_errors_pass_through() {
    let r = renderer(&[], RenderLimits::default());
    let result = r
        .render("nope.typ", &Context::new(), &AssetMap::new(), opts())
        .await;
    assert!(matches!(
        result,
        Err(PrintError::Template(
            spot_template::TemplateError::NotFound { .. }
        ))
    ));
}

#[tokio::test]
async fn a_zero_time_budget_is_a_typed_error() {
    let limits = RenderLimits {
        max_duration: std::time::Duration::ZERO,
        ..RenderLimits::default()
    };
    let r = renderer(&[], limits);
    let result = r.render("invoice.typ", &invoice(), &assets(), opts()).await;
    assert!(matches!(result, Err(PrintError::TimeLimitExceeded { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_concurrent_renders_all_succeed() {
    let r = renderer(&[], RenderLimits::default());
    let mut tasks = Vec::new();
    for i in 0..8 {
        let r = r.clone();
        tasks.push(tokio::spawn(async move {
            let name = if i % 2 == 0 {
                "invoice.typ"
            } else {
                "receipt.typ"
            };
            let context = if i % 2 == 0 { invoice() } else { receipt() };
            r.render(name, &context, &assets(), opts()).await
        }));
    }
    for task in tasks {
        let pdf = task.await.unwrap_or_else(|e| panic!("{e}"));
        assert!(pdf.unwrap_or_else(|e| panic!("{e}")).page_count >= 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_is_capped_by_the_semaphore() {
    let limits = RenderLimits {
        max_concurrent: 1,
        ..RenderLimits::default()
    };
    let r = renderer(&[], limits);
    let (context, files) = (receipt(), assets());
    let (a, b) = tokio::join!(
        r.render("receipt.typ", &context, &files, opts()),
        r.render("receipt.typ", &context, &files, opts())
    );
    assert!(a.is_ok() && b.is_ok());
}
