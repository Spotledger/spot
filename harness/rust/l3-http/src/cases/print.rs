//! Printing over HTTP on a real `spot serve` (`GET /api/v1/print/{doctype}/{id}?format=`,
//! WP-PLT-13.T3): the built-in layout and a site template give a PDF as the caller, the permission
//! and masking rules hold in the printed text, named formats and unknown ones, a hostile template
//! cannot escape, and the same document prints the same text twice.
//!
//! The eight cases share one private fixture (one database and host, started by the first case and
//! stopped by the last), whose site folder receives the template files each case needs. The print
//! service reads template files on every request, so a file written after the host started is
//! served without a reload. PDF text is read the way `spot-print`'s own tests read it: through each
//! font's `ToUnicode` map, because Typst writes CID fonts.
//!
//! Facts of the route this suite pins, from `spot-http`'s `print_get` (they differ from the task
//! brief's words where noted): the file name is `<doctype>-<id>.pdf` (built from the two validated
//! path names, never from the title); a `format` that is not a print format name is `400
//! api.invalid_request`; an unknown but well-formed format is `print.format_unknown`.

use std::collections::HashMap;
use std::time::Duration;

use lopdf::Object;
use serde_json::{Value as Json, json};
use tokio::sync::Mutex;

use super::{CaseDef, CaseFuture};
use crate::client::{Client, Reply};
use crate::fixture::{Fixture, MEMBER, OBSERVER, VIEWER};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After `function` (40): independent of every other group's data.
pub const ORDER: u32 = 45;

const SECTION: &str = "Print";

/// The fixture the eight cases share. The first case starts it, the last one finishes it.
static SHARED: Mutex<Option<Fixture>> = Mutex::const_new(None);

async fn shared() -> Result<tokio::sync::MutexGuard<'static, Option<Fixture>>, Failure> {
    let mut guard = SHARED.lock().await;
    if guard.is_none() {
        *guard = Fixture::start().await;
    }
    if guard.is_none() {
        return Err(failure!(
            "the print cases could not start their own fixture"
        ));
    }
    Ok(guard)
}

async fn finish_shared() {
    let taken = SHARED.lock().await.take();
    if let Some(mut fixture) = taken {
        fixture.finish().await;
    }
}

macro_rules! print_case {
    ($runner:ident, $case:ident, $last:expr) => {
        fn $runner(_unused: &Fixture) -> CaseFuture<'_> {
            Box::pin(async {
                let outcome = {
                    let guard = shared().await?;
                    match guard.as_ref() {
                        Some(fixture) => $case(fixture).await,
                        None => Err(failure!("the print fixture is gone")),
                    }
                };
                if $last {
                    finish_shared().await;
                }
                outcome
            })
        }
    };
}

print_case!(
    run_headers,
    print_returns_a_pdf_with_the_right_headers,
    false
);
print_case!(run_visibility, print_shows_what_the_caller_may_read, true);
print_case!(
    run_not_found,
    print_of_an_unreadable_or_missing_document_is_one_404,
    false
);
print_case!(run_formats, print_formats, false);
print_case!(run_hostile, a_template_cannot_inject_or_escape, false);
print_case!(run_money, print_money_stays_exact, false);
print_case!(run_bounded, print_is_deterministic_and_bounded, false);
print_case!(run_anonymous, print_requires_sign_in, false);

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "print: print_returns_a_pdf_with_the_right_headers",
            SECTION,
            "a signed-in member prints a project with the built-in layout: 200, `Content-Type: application/pdf`, a body that starts `%PDF-`, `Content-Disposition` `inline; filename=\"project-<id>.pdf\"` (built from the validated path names, no title, no separator, no extra quote), `Cache-Control: private, no-store` and `nosniff`",
            false,
            run_headers,
        ),
        CaseDef::run(
            "print: print_shows_what_the_caller_may_read_and_nothing_else",
            SECTION,
            "a template printing a task's subject and its confidential details shows the subject to a Viewer and never the details' cleartext (hidden or masked exactly as `GET /resource` reads it), and shows a Project Manager what the read path shows them",
            false,
            run_visibility,
        ),
        CaseDef::run(
            "print: print_of_an_unreadable_or_missing_document_is_one_404",
            SECTION,
            "a document that does not exist and an unknown DocType give the same `404 document.not_found` body (request id aside), and so does a document the caller may not read when the sample roles have one",
            false,
            run_not_found,
        ),
        CaseDef::run(
            "print: print_formats",
            SECTION,
            "`?format=invoice` renders `<doctype>.invoice.typ` (its marker is in the PDF text), a well-formed format with no file is `print.format_unknown`, and a name with a separator, a parent step or a percent-encoded variant is `400 api.invalid_request` without echoing the name or reading a file",
            false,
            run_formats,
        ),
        CaseDef::run(
            "print: a_template_cannot_inject_or_escape",
            SECTION,
            "a project whose title holds Typst markup prints it literally; a template that reads `/etc/passwd`, or a symlink in the template folder that points outside it, never puts file content in the PDF or the body",
            false,
            run_hostile,
        ),
        CaseDef::run(
            "print: print_money_stays_exact",
            SECTION,
            "a Time Entry's `0.25` and `0.75` hours print as the stored decimals through the `number` filter (no float noise; the sample's smallest value is 0.25, so the classic 0.1 + 0.2 pair cannot be stored), and a template that does arithmetic on the decimal does not print a computed sum",
            false,
            run_money,
        ),
        CaseDef::run(
            "print: print_is_deterministic_and_bounded",
            SECTION,
            "the same document and format twice give the same page count and text, a task with a 2,000-row checklist prints or is refused with an envelope inside the client timeout (never a hang), and 20 parallel prints each answer 200, or 429 or 503 with `Retry-After`",
            false,
            run_bounded,
        ),
        CaseDef::run(
            "print: print_requires_sign_in",
            SECTION,
            "an anonymous `GET /print/...` is 401 and the body is not a PDF",
            false,
            run_anonymous,
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn templates(fixture: &Fixture) -> std::path::PathBuf {
    fixture
        .root()
        .join("sites")
        .join(fixture.site())
        .join("templates")
}

fn write_template(fixture: &Fixture, name: &str, text: &str) -> Result<(), Failure> {
    let dir = templates(fixture);
    std::fs::create_dir_all(&dir).map_err(|e| failure!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(name);
    std::fs::write(&path, text).map_err(|e| failure!("cannot write {}: {e}", path.display()))
}

async fn print(client: &Client, doctype: &str, id: &str, query: &str) -> Result<Reply, Failure> {
    client
        .get(&format!("/api/v1/print/{doctype}/{id}{query}"))
        .await
}

fn text_of(reply: &Reply) -> Result<(usize, String), Failure> {
    ensure!(
        reply.body.starts_with(b"%PDF-"),
        "the body is not a PDF: {}",
        String::from_utf8_lossy(&reply.body[..reply.body.len().min(200)])
    );
    pdf_text(&reply.body)
}

/// A body with the request id removed, so two answers can be compared.
fn without_request_id(reply: &Reply) -> String {
    let mut json: Json = serde_json::from_slice(&reply.body).unwrap_or(Json::Null);
    if let Some(error) = json.get_mut("error").and_then(Json::as_object_mut) {
        error.remove("request_id");
    }
    json.to_string()
}

// ---------------------------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------------------------

async fn print_returns_a_pdf_with_the_right_headers(fixture: &Fixture) -> CaseResult {
    let (project, _, _) = fixture.project(&fixture.unique("Printable")).await?;
    let member = fixture.sign_in(MEMBER).await?;
    let reply = print(&member, "project", &project, "").await?;
    reply.expect(200)?;
    ensure!(
        reply.header("content-type") == Some("application/pdf"),
        "Content-Type is {:?}",
        reply.header("content-type")
    );
    ensure!(
        reply.body.starts_with(b"%PDF-"),
        "the body does not start with %PDF-"
    );
    let disposition = reply.header("content-disposition").unwrap_or_default();
    ensure!(
        disposition == format!("inline; filename=\"project-{project}.pdf\""),
        "Content-Disposition is {disposition:?}"
    );
    ensure!(
        !disposition.contains('/')
            && !disposition.contains('\\')
            && disposition.matches('"').count() == 2,
        "the file name has a separator or a stray quote: {disposition:?}"
    );
    ensure!(
        reply.header("cache-control") == Some("private, no-store"),
        "Cache-Control is {:?}",
        reply.header("cache-control")
    );
    ensure!(
        reply.header("x-content-type-options") == Some("nosniff"),
        "X-Content-Type-Options is {:?}",
        reply.header("x-content-type-options")
    );
    let (pages, text) = text_of(&reply)?;
    ensure!(pages >= 1, "the PDF has no page");
    ensure!(
        text.contains("Printable"),
        "the title is not in the page text: {text}"
    );
    Ok(())
}

async fn print_shows_what_the_caller_may_read(fixture: &Fixture) -> CaseResult {
    const SECRET: &str = "TOP-SECRET-DETAILS-4242";
    write_template(
        fixture,
        "task.typ",
        "#set text(font: (\"Noto Sans\",), size: 10pt)\nSubject: {{ doc.subject }}\n\nDetails: {{ doc.details }}\n",
    )?;
    let (project, _, _) = fixture.project(&fixture.unique("Visibility")).await?;
    let created = fixture
        .admin()
        .create(
            "task",
            &json!({"subject": "Visible subject", "project": project, "status": "Open", "details": SECRET}),
        )
        .await?;
    created.expect(201)?;
    let task = created.id()?;

    let viewer = fixture.sign_in(VIEWER).await?;
    let read = viewer.document("task", &task).await?;
    read.expect(200)?;
    let (_, text) = text_of(print(&viewer, "task", &task, "").await?.expect(200)?)?;
    ensure!(
        text.contains("Visible subject"),
        "the subject is missing: {text}"
    );
    ensure!(
        !text.contains(SECRET),
        "the confidential details reached a Viewer's page: {text}"
    );
    // What the page shows for the field is what the read path shows: hidden when absent, the
    // masked text when masked.
    if let Some(masked) = read.field("details").as_str().filter(|m| !m.is_empty()) {
        ensure!(
            text.contains(masked),
            "the page does not show the masked value the read path returns ({masked:?}): {text}"
        );
    }

    let manager = fixture.sign_in(crate::fixture::MANAGER).await?;
    let manager_read = manager.document("task", &task).await?;
    manager_read.expect(200)?;
    let (_, manager_text) = text_of(print(&manager, "task", &task, "").await?.expect(200)?)?;
    if let Some(shown) = manager_read
        .field("details")
        .as_str()
        .filter(|m| !m.is_empty())
    {
        ensure!(
            manager_text.contains(shown),
            "the page does not show a Project Manager what the read path shows them: {manager_text}"
        );
    }
    Ok(())
}

async fn print_of_an_unreadable_or_missing_document_is_one_404(fixture: &Fixture) -> CaseResult {
    let viewer = fixture.sign_in(VIEWER).await?;
    let missing = print(
        &viewer,
        "project",
        "01900000-0000-7000-8000-000000000000",
        "",
    )
    .await?;
    missing.expect(404)?.expect_error("document.not_found")?;
    let unknown = print(
        &viewer,
        "nonesuch",
        "01900000-0000-7000-8000-000000000000",
        "",
    )
    .await?;
    unknown.expect(404)?.expect_error("document.not_found")?;
    ensure!(
        without_request_id(&missing) == without_request_id(&unknown),
        "a missing document and an unknown DocType answer differently: {} / {}",
        missing.text(),
        unknown.text()
    );
    // A document the caller may not read, when a sample role has one: find a project the Observer
    // cannot read and compare the answer to the missing one.
    let (project, _, _) = fixture.project(&fixture.unique("Hidden")).await?;
    let observer = fixture.sign_in(OBSERVER).await?;
    let read = observer.document("project", &project).await?;
    if read.status != 200 {
        let refused = print(&observer, "project", &project, "").await?;
        refused.expect(404)?.expect_error("document.not_found")?;
        ensure!(
            without_request_id(&refused) == without_request_id(&missing),
            "an unreadable document answers differently from a missing one: {} / {}",
            refused.text(),
            missing.text()
        );
    } else {
        fixture.note(
            "the Observer can read projects: the unreadable-document branch was not exercised",
        );
    }
    Ok(())
}

async fn print_formats(fixture: &Fixture) -> CaseResult {
    write_template(
        fixture,
        "project.invoice.typ",
        "#set text(font: (\"Noto Sans\",), size: 10pt)\nTPL-MARKER-INVOICE {{ title }}\n",
    )?;
    let (project, _, _) = fixture.project(&fixture.unique("Formats")).await?;
    let admin = fixture.admin();
    let named = print(&admin, "project", &project, "?format=invoice").await?;
    named.expect(200)?;
    let (_, text) = text_of(&named)?;
    ensure!(
        text.contains("TPL-MARKER-INVOICE"),
        "the named template was not used: {text}"
    );

    let unknown = print(&admin, "project", &project, "?format=nope").await?;
    unknown.expect_error("print.format_unknown")?;

    for bad in ["../x", "a/b", "%2e%2e%2fx", "..", "a%2fb", "Invoice"] {
        let reply = print(&admin, "project", &project, &format!("?format={bad}")).await?;
        reply.expect(400)?.expect_error("api.invalid_request")?;
        ensure!(
            !reply.text().contains(bad),
            "the refused format name was echoed back: {}",
            reply.text()
        );
    }
    let twice = print(&admin, "project", &project, "?format=a&format=b").await?;
    twice.expect(400)?;
    let other = print(&admin, "project", &project, "?locale=en").await?;
    other.expect(400)?;
    Ok(())
}

async fn a_template_cannot_inject_or_escape(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    // Document values are data: markup in a title is printed literally.
    let hostile = "#panic(\"x\") *bold* ] #import \"/etc/passwd\"";
    let created = admin
        .create(
            "project",
            &json!({
                "code": fixture.unique("INJ"),
                "title": hostile,
                "status": "Active",
                "owner": fixture.admin_id(),
                "currency": "USD",
                "budget": "1.00",
            }),
        )
        .await?;
    created.expect(201)?;
    let project = created.id()?;
    let printed = print(&admin, "project", &project, "").await?;
    printed.expect(200)?;
    let (_, text) = text_of(&printed)?;
    ensure!(
        text.contains("#panic(") && text.contains("*bold*"),
        "the markup in the title was interpreted instead of printed: {text}"
    );

    // A template that tries to read a file the site folder does not own.
    write_template(fixture, "project.evil.typ", "#read(\"/etc/passwd\")\n")?;
    let evil = print(&admin, "project", &project, "?format=evil").await?;
    ensure!(
        !(evil.status == 200 && text_of(&evil).is_ok_and(|(_, t)| t.contains("root:"))),
        "a template read /etc/passwd into the PDF"
    );
    ensure!(
        !evil.text().contains("root:"),
        "file content reached an error body: {}",
        evil.text()
    );
    write_template(fixture, "project.img.typ", "#image(\"/etc/hostname\")\n")?;
    let image = print(&admin, "project", &project, "?format=img").await?;
    ensure!(
        image.status != 200,
        "a template embedded a file from outside the folder"
    );

    // A symlink in the folder that points outside it is never followed.
    #[cfg(unix)]
    {
        let outside = fixture.root().join("outside-secret.typ");
        std::fs::write(&outside, "OUTSIDE-SECRET-MARKER\n")
            .map_err(|e| failure!("cannot write the outside file: {e}"))?;
        let link = templates(fixture).join("project.link.typ");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside, &link)
            .map_err(|e| failure!("cannot create the symlink: {e}"))?;
        let linked = print(&admin, "project", &project, "?format=link").await?;
        ensure!(linked.status != 200, "a symlinked template was served");
        ensure!(
            !linked.text().contains("OUTSIDE-SECRET-MARKER"),
            "the symlink target reached the answer: {}",
            linked.text()
        );
        linked.expect_error("print.format_unknown")?;
    }
    Ok(())
}

async fn print_money_stays_exact(fixture: &Fixture) -> CaseResult {
    write_template(
        fixture,
        "time_entry.exact.typ",
        "#set text(font: (\"Noto Sans\",), size: 10pt)\nHOURS=[{{ doc.hours | number(2) }}]\n",
    )?;
    write_template(
        fixture,
        "time_entry.sum.typ",
        "#set text(font: (\"Noto Sans\",), size: 10pt)\nSUM=[{{ doc.hours + doc.hours }}]\n",
    )?;
    let (project, _, _) = fixture.project(&fixture.unique("Money")).await?;
    let (task, _) = fixture.task(&project, "Money task").await?;
    let admin = fixture.admin();
    let mut lines = String::new();
    for hours in ["0.25", "0.75"] {
        let (entry, _) = fixture.time_entry(&task, hours, "2026-01-05").await?;
        let (_, text) = text_of(
            print(&admin, "time_entry", &entry, "?format=exact")
                .await?
                .expect(200)?,
        )?;
        let shown = format!("HOURS=[{hours}]");
        ensure!(
            text.contains(&shown),
            "the stored decimal {hours} did not print exactly: {text}"
        );
        ensure!(
            !text.contains("0.30000000000000004") && !text.contains("0.2999"),
            "float noise in the printed decimal: {text}"
        );
        lines.push_str(&entry);
        lines.push(' ');
    }
    // The sample's smallest hours value is 0.25, so the classic 0.1 + 0.2 pair cannot be stored;
    // 0.25 + 0.25 stands for it: a computed sum must not be printed.
    let (entry, _) = fixture.time_entry(&task, "0.25", "2026-01-06").await?;
    let sum = print(&admin, "time_entry", &entry, "?format=sum").await?;
    let computed = sum.status == 200 && text_of(&sum).is_ok_and(|(_, t)| t.contains("SUM=[0.5"));
    ensure!(
        !computed,
        "a template computed on a money value and printed the sum"
    );
    let _ = lines;
    Ok(())
}

async fn print_is_deterministic_and_bounded(fixture: &Fixture) -> CaseResult {
    write_template(
        fixture,
        "project.stable.typ",
        "#set text(font: (\"Noto Sans\",), size: 10pt)\nSTABLE {{ title }}\n",
    )?;
    let (project, _, _) = fixture.project(&fixture.unique("Stable")).await?;
    let admin = fixture.admin();
    let first = text_of(
        print(&admin, "project", &project, "?format=stable")
            .await?
            .expect(200)?,
    )?;
    let second = text_of(
        print(&admin, "project", &project, "?format=stable")
            .await?
            .expect(200)?,
    )?;
    // The print service has no setting that fixes `printed_at` (the PDF metadata carries the
    // render time), so the bytes may differ by that stamp: the page count and the text must not.
    ensure!(
        first == second,
        "the same document printed to different pages or text"
    );

    // A 2,000-row child table: printed within the limits or refused with an envelope, never a hang.
    let rows: Vec<Json> = (0..2000)
        .map(|i| json!({"text": format!("item {i}")}))
        .collect();
    let created = admin
        .create(
            "task",
            &json!({"subject": "Long list", "project": project, "status": "Open", "checklist": rows}),
        )
        .await?;
    if created.status == 201 {
        let long = admin.with_timeout(Duration::from_secs(10));
        let reply = print(&long, "task", &created.id()?, "").await?;
        ensure!(
            reply.status == 200 || reply.error_code().is_some(),
            "a 2,000-row print answered neither a PDF nor an error envelope: {} {}",
            reply.status,
            reply.text()
        );
    } else {
        fixture.note("the sample refuses a 2,000-row checklist at create: the long-table branch was not exercised");
    }

    // A storm of 20 parallel prints: each is a PDF or a documented refusal with Retry-After.
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let client = admin.clone();
        let id = project.clone();
        tasks.push(tokio::spawn(async move {
            print(&client, "project", &id, "").await
        }));
    }
    for task in tasks {
        let reply = task
            .await
            .map_err(|e| failure!("a parallel print task failed: {e}"))??;
        match reply.status {
            200 => ensure!(reply.body.starts_with(b"%PDF-"), "a 200 that is not a PDF"),
            429 | 503 => ensure!(
                reply.header("retry-after").is_some(),
                "a refused print has no Retry-After: {}",
                reply.text()
            ),
            other => {
                return Err(failure!(
                    "a parallel print answered {other}: {}",
                    reply.text()
                ));
            }
        }
    }
    Ok(())
}

async fn print_requires_sign_in(fixture: &Fixture) -> CaseResult {
    let (project, _, _) = fixture.project(&fixture.unique("Anonymous")).await?;
    let reply = print(&fixture.client().anonymous(), "project", &project, "").await?;
    reply.expect(401)?;
    ensure!(
        !reply.body.starts_with(b"%PDF-"),
        "an anonymous request received a PDF"
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Reading text out of a Typst PDF (the approach of spot-print's own tests)
// ---------------------------------------------------------------------------------------------

/// The page count and the text of every page, decoded through each font's `ToUnicode` map.
fn pdf_text(bytes: &[u8]) -> Result<(usize, String), Failure> {
    let doc =
        lopdf::Document::load_mem(bytes).map_err(|e| failure!("the PDF does not parse: {e}"))?;
    let pages = doc.get_pages();
    let mut text = String::new();
    for page_id in pages.values() {
        let maps = font_maps(&doc, *page_id);
        let content = doc
            .get_and_decode_page_content(*page_id)
            .map_err(|e| failure!("page content: {e}"))?;
        let mut current: Option<&HashMap<u16, String>> = None;
        for op in &content.operations {
            match op.operator.as_str() {
                "Tf" => {
                    current = op
                        .operands
                        .first()
                        .and_then(|name| name.as_name().ok())
                        .and_then(|name| maps.get(name));
                }
                "Tj" => {
                    if let (Some(map), Some(Object::String(data, _))) =
                        (current, op.operands.first())
                    {
                        text.push_str(&decode(map, data));
                        text.push('\n');
                    }
                }
                "TJ" => {
                    if let (Some(map), Some(Object::Array(items))) = (current, op.operands.first())
                    {
                        for item in items {
                            if let Object::String(data, _) = item {
                                text.push_str(&decode(map, data));
                            }
                        }
                        text.push('\n');
                    }
                }
                _ => {}
            }
        }
    }
    Ok((pages.len(), text))
}

fn decode(map: &HashMap<u16, String>, data: &[u8]) -> String {
    data.chunks(2)
        .filter(|pair| pair.len() == 2)
        .filter_map(|pair| map.get(&u16::from_be_bytes([pair[0], pair[1]])))
        .map(String::as_str)
        .collect()
}

fn font_maps(
    doc: &lopdf::Document,
    page: lopdf::ObjectId,
) -> HashMap<Vec<u8>, HashMap<u16, String>> {
    let mut maps = HashMap::new();
    for (name, font) in doc.get_page_fonts(page).unwrap_or_default() {
        let stream = font
            .get(b"ToUnicode")
            .and_then(|object| doc.dereference(object))
            .and_then(|(_, object)| object.as_stream());
        if let Ok(stream) = stream {
            let data = stream.decompressed_content().unwrap_or_default();
            maps.insert(name, parse_cmap(&String::from_utf8_lossy(&data)));
        }
    }
    maps
}

fn hex(token: &str) -> Option<u32> {
    u32::from_str_radix(token.trim_matches(|c| c == '<' || c == '>'), 16).ok()
}

fn utf16(token: &str) -> String {
    let digits = token.trim_matches(|c| c == '<' || c == '>');
    let units: Vec<u16> = (0..digits.len() / 4)
        .filter_map(|i| u16::from_str_radix(&digits[i * 4..i * 4 + 4], 16).ok())
        .collect();
    String::from_utf16_lossy(&units)
}

/// Parses `bfchar` and `bfrange` (single destination) sections.
fn parse_cmap(cmap: &str) -> HashMap<u16, String> {
    let mut map = HashMap::new();
    let mut mode = "";
    for line in cmap.lines().map(str::trim) {
        if line.ends_with("beginbfchar") {
            mode = "char";
        } else if line.ends_with("beginbfrange") {
            mode = "range";
        } else if line.starts_with("end") {
            mode = "";
        } else if mode == "char" {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let (Some(src), Some(dst)) = (parts.first().and_then(|t| hex(t)), parts.get(1)) {
                map.insert(src as u16, utf16(dst));
            }
        } else if mode == "range" {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let (Some(lo), Some(hi), Some(dst)) = (
                parts.first().and_then(|t| hex(t)),
                parts.get(1).and_then(|t| hex(t)),
                parts.get(2),
            ) {
                let base = hex(dst).unwrap_or(0);
                for (offset, code) in (lo..=hi).enumerate() {
                    if let Some(c) = char::from_u32(base + offset as u32) {
                        map.insert(code as u16, c.to_string());
                    }
                }
            }
        }
    }
    map
}
