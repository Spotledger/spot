//! Golden outputs: six templates in `en` and `ur-PK`, compared byte for byte with the checked-in
//! expectation (`tests/golden/<template>.<locale>.{subject,text,html}`; a part with no file is
//! expected to be absent).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;

fn expected(template: &str, locale: &str, part: &str) -> Option<String> {
    let path = common::fixtures()
        .parent()
        .unwrap()
        .join("golden")
        .join(format!("{template}.{locale}.{part}"));
    fs::read(&path)
        .ok()
        .map(|bytes| String::from_utf8(bytes).unwrap())
}

#[test]
fn six_templates_render_byte_for_byte_in_both_locales() {
    let renderer = common::renderer();
    for (template, context) in common::contexts() {
        for locale in ["en", "ur-PK"] {
            let rendered = renderer
                .render(template, Some(locale), &context)
                .unwrap_or_else(|error| panic!("{template} {locale}: {error}"));
            assert_eq!(
                Some(rendered.subject.clone()),
                expected(template, locale, "subject"),
                "{template} {locale} subject"
            );
            assert_eq!(
                rendered.text,
                expected(template, locale, "text"),
                "{template} {locale} text"
            );
            assert_eq!(
                rendered.html,
                expected(template, locale, "html"),
                "{template} {locale} html"
            );
        }
    }
}

#[test]
fn the_chosen_locale_is_reported() {
    let renderer = common::renderer();
    let contexts = common::contexts();
    let notice = &contexts.iter().find(|(n, _)| *n == "notice").unwrap().1;
    let invoice = &contexts.iter().find(|(n, _)| *n == "invoice").unwrap().1;
    let reset = &contexts.iter().find(|(n, _)| *n == "reset").unwrap().1;
    assert_eq!(
        renderer
            .render("notice", Some("ur-PK"), notice)
            .unwrap()
            .locale
            .as_deref(),
        Some("ur-PK")
    );
    // No `de` files: falls back to `en`, then to the files without a locale.
    assert_eq!(
        renderer
            .render("notice", Some("de"), notice)
            .unwrap()
            .subject,
        "Notice for Bilal"
    );
    // The invoice has only an Urdu subject and HTML body: the text body falls back to the
    // file without a locale, and the result names the locale that was used.
    let urdu_invoice = renderer.render("invoice", Some("ur-PK"), invoice).unwrap();
    assert_eq!(urdu_invoice.locale.as_deref(), Some("ur-PK"));
    // A template with no locale files reports none.
    assert_eq!(
        renderer
            .render("reset", Some("ur-PK"), reset)
            .unwrap()
            .locale,
        None
    );
    assert_eq!(renderer.render("reset", None, reset).unwrap().locale, None);
}
