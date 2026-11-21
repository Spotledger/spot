//! Escaping, resolution, strictness, the cache, and the conversion into a message.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::Path;

use spot_mail::{CaptureTransport, MailTransport, Mailbox};
use spot_mail_render::{MailRenderer, RenderError, TemplateRoots, context_from_json};

fn write(root: &Path, name: &str, file: &str, text: &str) {
    let dir = root.join("mail").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(file), text).unwrap();
}

fn renderer_over(root: &Path) -> MailRenderer {
    MailRenderer::new(TemplateRoots::new(vec![root.to_path_buf()]))
}

fn mailbox(address: &str) -> Mailbox {
    Mailbox::new(None, address).unwrap()
}

#[test]
fn html_escapes_and_text_does_not() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "echo", "subject.txt", "Re: {{ v }}");
    write(dir.path(), "echo", "body.txt", "{{ v }}");
    write(dir.path(), "echo", "body.html", "<p>{{ v }}</p>");
    let context = common::ctx(serde_json::json!({"v": "<script>&"}));
    let rendered = renderer_over(dir.path())
        .render("echo", None, &context)
        .unwrap();
    assert_eq!(rendered.subject, "Re: <script>&");
    assert_eq!(rendered.text.as_deref(), Some("<script>&"));
    assert_eq!(rendered.html.as_deref(), Some("<p>&lt;script&gt;&amp;</p>"));
}

#[test]
fn a_line_break_in_the_subject_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "hi", "subject.txt", "Hi {{ name }}");
    write(dir.path(), "hi", "body.txt", "x");
    let context = common::ctx(serde_json::json!({"name": "a\r\nBcc: evil@example.test"}));
    match renderer_over(dir.path()).render("hi", None, &context) {
        Err(RenderError::SubjectInvalid { template, reason }) => {
            assert_eq!(template, "hi");
            assert_eq!(reason, "line_break");
        }
        other => panic!("{other:?}"),
    }
    // A trailing line break of the file itself is fine.
    write(dir.path(), "hi", "subject.txt", "Hi\n");
    let ok = renderer_over(dir.path())
        .render("hi", None, &context)
        .unwrap();
    assert_eq!(ok.subject, "Hi");
}

#[test]
fn a_long_subject_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "long", "subject.txt", &"s".repeat(256));
    write(dir.path(), "long", "body.txt", "x");
    match renderer_over(dir.path()).render("long", None, &common::ctx(serde_json::json!({}))) {
        Err(RenderError::SubjectInvalid { reason, .. }) => assert_eq!(reason, "too_long"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_site_overrides_the_app() {
    let context = common::ctx(serde_json::json!({}));
    let rendered = common::renderer().render("plain", None, &context).unwrap();
    assert_eq!(rendered.subject, "Weekly note (site)");
    // Without the site root the app's own template is used.
    let app_only = MailRenderer::new(TemplateRoots::new(vec![common::fixtures().join("app")]));
    assert_eq!(
        app_only.render("plain", None, &context).unwrap().subject,
        "Weekly note"
    );
}

#[test]
fn hostile_names_are_refused_before_any_read() {
    let context = common::ctx(serde_json::json!({}));
    let renderer = common::renderer();
    for name in ["../x", "a/b", "..", "", "A", "a b", "a\\b"] {
        assert_eq!(
            renderer.render(name, None, &context),
            Err(RenderError::InvalidName { what: "name" }),
            "{name:?}"
        );
    }
    for locale in ["../en", "en/PK", "en_PK", "", "e"] {
        assert_eq!(
            renderer.render("plain", Some(locale), &context),
            Err(RenderError::InvalidName { what: "locale" }),
            "{locale:?}"
        );
    }
    assert!(matches!(
        renderer.render("nothere", None, &context),
        Err(RenderError::TemplateNotFound { .. })
    ));
}

#[cfg(unix)]
#[test]
fn a_symlink_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
    write(dir.path(), "linked", "body.txt", "x");
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("mail/linked/subject.txt"),
    )
    .unwrap();
    let context = common::ctx(serde_json::json!({}));
    match renderer_over(dir.path()).render("linked", None, &context) {
        Err(RenderError::FileRefused { file, reason, .. }) => {
            assert_eq!(file, "subject.txt");
            assert_eq!(reason, "link");
        }
        other => panic!("{other:?}"),
    }
    // A template folder that is a link is not found at all.
    std::os::unix::fs::symlink(
        dir.path().join("mail/linked"),
        dir.path().join("mail/alias"),
    )
    .unwrap();
    assert!(matches!(
        renderer_over(dir.path()).render("alias", None, &context),
        Err(RenderError::TemplateNotFound { .. })
    ));
}

#[test]
fn an_oversize_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "big", "subject.txt", "s");
    write(dir.path(), "big", "body.txt", &"x".repeat(256 * 1024 + 1));
    let context = common::ctx(serde_json::json!({}));
    match renderer_over(dir.path()).render("big", None, &context) {
        Err(RenderError::FileRefused { file, reason, .. }) => {
            assert_eq!(file, "body.txt");
            assert_eq!(reason, "too_large");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_incomplete_template_is_named() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "nosubject", "body.txt", "x");
    write(dir.path(), "nobody", "subject.txt", "s");
    let context = common::ctx(serde_json::json!({}));
    let renderer = renderer_over(dir.path());
    assert_eq!(
        renderer.render("nosubject", None, &context),
        Err(RenderError::TemplateIncomplete {
            template: "nosubject".into(),
            missing: "subject"
        })
    );
    assert_eq!(
        renderer.render("nobody", None, &context),
        Err(RenderError::TemplateIncomplete {
            template: "nobody".into(),
            missing: "body"
        })
    );
}

#[test]
fn a_missing_variable_names_the_template_file_line_and_variable() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "broken", "subject.txt", "s");
    write(
        dir.path(),
        "broken",
        "body.txt",
        "one\ntwo {{ order.missing }}",
    );
    let context = common::ctx(serde_json::json!({"order": {"id": 1}}));
    match renderer_over(dir.path()).render("broken", None, &context) {
        Err(error @ RenderError::UndefinedVariable { .. }) => {
            assert_eq!(
                error,
                RenderError::UndefinedVariable {
                    template: "broken".into(),
                    file: "body.txt".into(),
                    line: 2,
                    variable: "order.missing".into()
                }
            );
            let text = error.to_string();
            assert!(!text.contains("two {{"), "no source text: {text}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn arithmetic_on_money_is_refused_with_the_engines_message() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "sum", "subject.txt", "s");
    write(dir.path(), "sum", "body.txt", "{{ total + total }}");
    let context = common::ctx(serde_json::json!({"total": {"$decimal": "1.50"}}));
    match renderer_over(dir.path()).render("sum", None, &context) {
        Err(RenderError::NotComputable {
            message, template, ..
        }) => {
            assert_eq!(template, "sum");
            assert!(message.contains("do not compute money"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_changed_file_renders_new_content_and_a_deleted_one_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "live", "subject.txt", "s");
    write(dir.path(), "live", "body.txt", "first");
    let renderer = renderer_over(dir.path());
    let context = common::ctx(serde_json::json!({}));
    assert_eq!(
        renderer
            .render("live", None, &context)
            .unwrap()
            .text
            .as_deref(),
        Some("first")
    );
    write(dir.path(), "live", "body.txt", "second");
    assert_eq!(
        renderer
            .render("live", None, &context)
            .unwrap()
            .text
            .as_deref(),
        Some("second")
    );
    fs::remove_dir_all(dir.path().join("mail/live")).unwrap();
    assert!(matches!(
        renderer.render("live", None, &context),
        Err(RenderError::TemplateNotFound { .. })
    ));
}

#[test]
fn errors_carry_no_values() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "v", "subject.txt", "s");
    write(dir.path(), "v", "body.txt", "{{ secret_value }} {{ gone }}");
    let context = common::ctx(serde_json::json!({"secret_value": "hunter2@example.test"}));
    let error = renderer_over(dir.path())
        .render("v", None, &context)
        .unwrap_err();
    let text = format!("{error} {error:?}");
    assert!(!text.contains("hunter2"), "{text}");
}

#[test]
fn context_json_rules() {
    assert_eq!(
        context_from_json(serde_json::json!([1])),
        Err(RenderError::InvalidContext {
            reason: "not_an_object"
        })
    );
    assert_eq!(
        context_from_json(serde_json::json!({"a": 1.5})),
        Err(RenderError::InvalidContext {
            reason: "non_integer_number"
        })
    );
    assert_eq!(
        context_from_json(serde_json::json!({"a": {"$decimal": "x"}})),
        Err(RenderError::InvalidContext {
            reason: "bad_decimal"
        })
    );
    assert_eq!(
        context_from_json(serde_json::json!({"a": {"$date": "2026-13-40"}})),
        Err(RenderError::InvalidContext { reason: "bad_date" })
    );
    assert_eq!(
        context_from_json(serde_json::json!({"a": {"$datetime": "yesterday"}})),
        Err(RenderError::InvalidContext {
            reason: "bad_datetime"
        })
    );
    assert!(
        context_from_json(serde_json::json!({"a": {"$datetime": "2026-03-10T14:45:00+05:00"}}))
            .is_ok()
    );
}

#[tokio::test]
async fn into_message_validates_and_the_capture_transport_shows_the_mime() {
    let context = common::contexts()
        .into_iter()
        .find(|(n, _)| *n == "notice")
        .unwrap()
        .1;
    let rendered = common::renderer().render("notice", None, &context).unwrap();
    let message = rendered
        .clone()
        .into_message(
            mailbox("noreply@acme.test"),
            vec![mailbox("bilal@example.test")],
        )
        .unwrap();
    assert!(message.validate().is_ok());
    assert_eq!(message.text.as_deref(), Some("Hello Bilal."));
    assert_eq!(message.html.as_deref(), Some("<p>Hello Bilal.</p>"));

    let capture = CaptureTransport::new();
    capture.send(&message).await.unwrap();
    let mime = String::from_utf8_lossy(&capture.messages()[0]).to_string();
    assert!(mime.contains("multipart/alternative"), "{mime}");
    assert!(mime.contains("Notice for Bilal"), "{mime}");

    // No recipient: refused, and the error holds no address.
    let error = rendered
        .into_message(mailbox("noreply@acme.test"), Vec::new())
        .unwrap_err();
    assert_eq!(error, RenderError::InvalidMessage);
    assert!(!error.to_string().contains("acme.test"));
}

#[test]
fn html_only_gets_a_derived_text_and_text_only_gets_no_html() {
    let renderer = common::renderer();
    let contexts = common::contexts();
    let htmlonly = &contexts.iter().find(|(n, _)| *n == "htmlonly").unwrap().1;
    let message = renderer
        .render("htmlonly", None, htmlonly)
        .unwrap()
        .into_message(
            mailbox("noreply@acme.test"),
            vec![mailbox("bilal@example.test")],
        )
        .unwrap();
    assert_eq!(message.text.as_deref(), Some("Welcome\nHello Bilal."));
    assert_eq!(
        message.html.as_deref(),
        Some("<h1>Welcome</h1><p>Hello Bilal.</p>")
    );

    let plain = &contexts.iter().find(|(n, _)| *n == "plain").unwrap().1;
    let message = renderer
        .render("plain", None, plain)
        .unwrap()
        .into_message(
            mailbox("noreply@acme.test"),
            vec![mailbox("bilal@example.test")],
        )
        .unwrap();
    assert_eq!(message.html, None);
    assert_eq!(message.text.as_deref(), Some("Site override."));
}
