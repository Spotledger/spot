//! Names are validated before anything touches the file system: a name that is not plain is
//! refused without a read.

/// The longest template name, in bytes.
pub(crate) const MAX_NAME_BYTES: usize = 63;
/// The longest locale tag, in bytes.
pub(crate) const MAX_LOCALE_BYTES: usize = 20;

/// A template name is a storage name: `[a-z][a-z0-9_]*`, at most 63 bytes.
pub(crate) fn is_template_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    name.len() <= MAX_NAME_BYTES
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A locale is a BCP 47 style tag: a 2 or 3 letter language, then up to three `-` separated
/// subtags of 2 to 8 letters or digits (`en`, `ur-PK`, `zh-Hant-TW`). At most 20 bytes.
pub(crate) fn is_locale(locale: &str) -> bool {
    if locale.is_empty() || locale.len() > MAX_LOCALE_BYTES {
        return false;
    }
    let mut parts = locale.split('-');
    match parts.next() {
        Some(language)
            if (2..=3).contains(&language.len())
                && language.bytes().all(|b| b.is_ascii_alphabetic()) => {}
        _ => return false,
    }
    let mut subtags = 0;
    for part in parts {
        subtags += 1;
        if subtags > 3
            || !(2..=8).contains(&part.len())
            || !part.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return false;
        }
    }
    true
}

/// A file name inside a template folder: `<stem>.<ext>` or `<stem>.<locale>.<ext>` where the
/// stem is a storage name, the locale passes [`is_locale`] and the extension is `txt` or `html`.
pub(crate) fn is_file_name(file: &str) -> bool {
    let Some((rest, ext)) = file.rsplit_once('.') else {
        return false;
    };
    if ext != "txt" && ext != "html" {
        return false;
    }
    match rest.split_once('.') {
        None => is_template_name(rest),
        Some((stem, locale)) => is_template_name(stem) && is_locale(locale),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(is_template_name("invoice_sent"));
        for bad in [
            "", "..", "../x", "a/b", "A", "1a", "a b", "a.b", "a\\b", "a\0b",
        ] {
            assert!(!is_template_name(bad), "{bad:?}");
        }
        assert!(!is_template_name(&"a".repeat(64)));
    }

    #[test]
    fn locales() {
        for good in ["en", "ur-PK", "zh-Hant-TW", "fil"] {
            assert!(is_locale(good), "{good}");
        }
        for bad in [
            "",
            "e",
            "english",
            "en_US",
            "en-",
            "en-x",
            "../",
            "en/PK",
            "a-b-c-d-e-f",
        ] {
            assert!(!is_locale(bad), "{bad}");
        }
    }

    #[test]
    fn files() {
        for good in [
            "subject.txt",
            "body.html",
            "body.ur-PK.html",
            "subject.en.txt",
        ] {
            assert!(is_file_name(good), "{good}");
        }
        for bad in [
            "body",
            "body.md",
            "../body.txt",
            "body..txt",
            "Body.txt",
            "a/b.txt",
        ] {
            assert!(!is_file_name(bad), "{bad}");
        }
    }
}
