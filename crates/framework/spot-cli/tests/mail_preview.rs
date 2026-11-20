//! `spot mail-preview`: golden stdout, the site-over-app rule, exit codes, and that it needs
//! neither a database nor a network: the host configuration points at a closed port and the
//! command still finishes.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A root with one app directory (templates inside it) and one host-wide site `acme`.
fn root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "spot_mail_preview_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let app = root.join("apps/books");
    std::fs::create_dir_all(app.join("doctypes")).unwrap();
    std::fs::create_dir_all(root.join("sites/acme/templates")).unwrap();
    std::fs::write(
        root.join("common.toml"),
        format!(
            "apps = [{:?}]\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://u:p@127.0.0.1:1/postgres'\n",
            app.display().to_string()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("sites/acme/site.toml"),
        "site = 'acme'\ndatabase = 'acme'\n",
    )
    .unwrap();
    template(
        &app.join("templates"),
        "welcome",
        "subject.txt",
        "Hello {{ name }}",
    );
    template(
        &app.join("templates"),
        "welcome",
        "body.txt",
        "Welcome, {{ name }}.\nYour balance is {{ balance | money('PKR') }}.",
    );
    template(
        &app.join("templates"),
        "welcome",
        "body.html",
        "<p>Welcome, {{ name }}.</p>",
    );
    template(&app.join("templates"), "htmlonly", "subject.txt", "Hi");
    template(
        &app.join("templates"),
        "htmlonly",
        "body.html",
        "<p>One</p><p>Two</p>",
    );
    template(
        &root.join("sites/acme/templates"),
        "welcome",
        "subject.txt",
        "Site hello {{ name }}",
    );
    template(
        &root.join("sites/acme/templates"),
        "welcome",
        "body.txt",
        "Site body.",
    );
    root
}

fn template(base: &Path, name: &str, file: &str, text: &str) {
    let dir = base.join("mail").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), text).unwrap();
}

fn run(root: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_spot"))
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            input.write_all(text.as_bytes()).unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const CONTEXT: &str = r#"{"name": "Bilal", "balance": {"$decimal": "1234.56"}}"#;

#[test]
fn the_site_template_overrides_the_app_and_stdout_is_golden() {
    let root = root("site");
    let output = run(
        &root,
        &[
            "mail-preview",
            "welcome",
            "--site",
            "acme",
            "--context",
            "-",
        ],
        Some(CONTEXT),
    );
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(out(&output), "Subject: Site hello Bilal\n\nSite body.\n");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_app_template_prints_text_html_and_json_and_derives_text() {
    let root = root("app");
    // Remove the site override so the app's own template is used.
    std::fs::remove_dir_all(root.join("sites/acme/templates/mail/welcome")).unwrap();
    let ctx_file = root.join("ctx.json");
    std::fs::write(&ctx_file, CONTEXT).unwrap();
    let ctx_arg = ctx_file.to_str().unwrap();

    let text = run(
        &root,
        &[
            "mail-preview",
            "welcome",
            "--site",
            "acme",
            "--context",
            ctx_arg,
        ],
        None,
    );
    assert_eq!(text.status.code(), Some(0), "{}", err(&text));
    assert_eq!(
        out(&text),
        "Subject: Hello Bilal\n\nWelcome, Bilal.\nYour balance is Rs 1,234.56.\n"
    );

    let html = run(
        &root,
        &[
            "mail-preview",
            "welcome",
            "--site",
            "acme",
            "--context",
            ctx_arg,
            "--format",
            "html",
        ],
        None,
    );
    assert_eq!(out(&html), "<p>Welcome, Bilal.</p>\n");

    let json = run(
        &root,
        &[
            "mail-preview",
            "welcome",
            "--site",
            "acme",
            "--context",
            ctx_arg,
            "--format",
            "json",
            "--locale",
            "ur-PK",
        ],
        None,
    );
    let value: serde_json::Value = serde_json::from_str(&out(&json)).unwrap();
    assert_eq!(value["subject"], "Hello Bilal");
    assert_eq!(value["template"], "welcome");
    assert_eq!(value["html"], "<p>Welcome, Bilal.</p>");

    // HTML-only: the text format prints the derived text.
    let derived = run(&root, &["mail-preview", "htmlonly", "--site", "acme"], None);
    assert_eq!(out(&derived), "Subject: Hi\n\nOne\nTwo\n");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn exit_codes_follow_the_registry_convention() {
    let root = root("exit");
    let missing = run(&root, &["mail-preview", "nothere", "--site", "acme"], None);
    assert_eq!(missing.status.code(), Some(3));
    assert!(
        err(&missing).contains("mail_render.template_not_found"),
        "{}",
        err(&missing)
    );

    let bad_format = run(
        &root,
        &[
            "mail-preview",
            "welcome",
            "--site",
            "acme",
            "--format",
            "xml",
        ],
        None,
    );
    assert_eq!(bad_format.status.code(), Some(1));

    let no_site = run(
        &root,
        &["mail-preview", "welcome", "--site", "nowhere"],
        None,
    );
    assert_eq!(no_site.status.code(), Some(3));

    // A missing variable names the file, line and variable and never the values.
    let missing_var = run(
        &root,
        &[
            "mail-preview",
            "welcome",
            "--site",
            "acme",
            "--context",
            "-",
        ],
        Some(r#"{"name": "Bilal"}"#),
    );
    // The site override has no variables, so it renders; the app template is the strict one.
    assert_eq!(missing_var.status.code(), Some(0));
    std::fs::remove_dir_all(root.join("sites/acme/templates/mail/welcome")).unwrap();
    let strict = run(
        &root,
        &[
            "mail-preview",
            "welcome",
            "--site",
            "acme",
            "--context",
            "-",
        ],
        Some(r#"{"name": "Bilal"}"#),
    );
    assert_eq!(strict.status.code(), Some(3));
    let text = err(&strict);
    assert!(text.contains("mail_render.undefined_variable"), "{text}");
    assert!(text.contains("balance"), "{text}");
    assert!(!text.contains("Bilal"), "{text}");
    std::fs::remove_dir_all(root).unwrap();
}
