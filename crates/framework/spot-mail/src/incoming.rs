//! The headers an [`IncomingMessage`] carries beyond its parsed fields (WP-PLT-11.T16).
//!
//! The list is **bounded**: five fixed names, at most one value each, each value at most
//! [`MAX_HEADER_VALUE_BYTES`] bytes. A message never carries an arbitrary header, so a hostile
//! sender cannot make the ingestion path hold more than a few kilobytes of header text, and a
//! consumer can only ask for a header the crate chose to keep.
//!
//! What the headers are for: the incoming-mail engine reads `X-Spot-Outgoing` to recognise mail
//! this site sent itself (a keyed marker it verifies; see `spot-mail-intake`) and
//! `Auto-Submitted`/`Precedence` for loop protection. `In-Reply-To` and `References` repeat the
//! parsed `in_reply_to` and `references` fields in their wire form, for threading consumers.

use mail_parser::{HeaderName, Message};

use crate::imap::IncomingMessage;

/// The header names kept, in the spelling [`SelectedHeader::name`] reports.
pub const SELECTED_HEADER_NAMES: [&str; 5] = [
    "X-Spot-Outgoing",
    "Auto-Submitted",
    "Precedence",
    "In-Reply-To",
    "References",
];

/// The longest value kept, in bytes; a longer value is not kept at all (never cut, since a
/// truncated marker or auto-submitted value would read as something else).
pub const MAX_HEADER_VALUE_BYTES: usize = 998;

/// One kept header: a fixed name and its trimmed text value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedHeader {
    /// One of [`SELECTED_HEADER_NAMES`].
    pub name: &'static str,
    /// The header's value, trimmed, no line break inside.
    pub value: String,
}

impl IncomingMessage {
    /// The value of the kept header called `name` (ASCII case-insensitive), if the message had
    /// it and it fitted the bound. Only the five names of [`SELECTED_HEADER_NAMES`] are ever
    /// kept: asking for any other name answers `None`.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }
}

/// Keeps the selected headers of `parsed`. `in_reply_to` and `references` are the values the
/// normalizer already extracted (ids without angle brackets); they are re-wrapped in angle
/// brackets so the kept text is the wire form.
pub(crate) fn selected_headers(
    parsed: &Message<'_>,
    in_reply_to: Option<&str>,
    references: &[String],
) -> Vec<SelectedHeader> {
    let mut kept = Vec::new();
    let text = |name: HeaderName<'static>| {
        parsed
            .header(name)
            .and_then(|value| value.as_text())
            .map(str::to_owned)
    };
    push(
        &mut kept,
        "X-Spot-Outgoing",
        text(HeaderName::Other("x-spot-outgoing".into())),
    );
    push(&mut kept, "Auto-Submitted", text(HeaderName::AutoSubmitted));
    push(
        &mut kept,
        "Precedence",
        text(HeaderName::Other("precedence".into())),
    );
    push(
        &mut kept,
        "In-Reply-To",
        in_reply_to.map(|id| format!("<{id}>")),
    );
    if !references.is_empty() {
        let joined = references
            .iter()
            .map(|id| format!("<{id}>"))
            .collect::<Vec<_>>()
            .join(" ");
        push(&mut kept, "References", Some(joined));
    }
    kept
}

/// Adds `value` under `name` when it is present, free of line breaks and within the bound.
fn push(kept: &mut Vec<SelectedHeader>, name: &'static str, value: Option<String>) {
    let Some(value) = value else {
        return;
    };
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_HEADER_VALUE_BYTES || value.contains(['\r', '\n']) {
        return;
    }
    kept.push(SelectedHeader {
        name,
        value: value.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use mail_parser::MessageParser;

    use super::*;

    fn parse(raw: &str) -> Vec<SelectedHeader> {
        let parsed = MessageParser::default()
            .parse(raw.as_bytes())
            .unwrap_or_else(|| panic!("fixture does not parse"));
        selected_headers(&parsed, None, &[])
    }

    #[test]
    fn only_the_selected_names_are_kept_and_each_once() {
        let kept = parse(
            "From: a@x.test\r\nX-Spot-Outgoing: v1.abc\r\nAuto-Submitted: auto-replied\r\nPrecedence: bulk\r\nX-Other: no\r\nSubject: s\r\n\r\nbody\r\n",
        );
        let names: Vec<&str> = kept.iter().map(|header| header.name).collect();
        assert_eq!(names, ["X-Spot-Outgoing", "Auto-Submitted", "Precedence"]);
        assert_eq!(kept[0].value, "v1.abc");
    }

    #[test]
    fn an_oversize_value_is_dropped_not_cut() {
        let long = "a".repeat(MAX_HEADER_VALUE_BYTES + 1);
        let kept = parse(&format!(
            "From: a@x.test\r\nX-Spot-Outgoing: {long}\r\n\r\nbody\r\n"
        ));
        assert!(kept.is_empty());
    }

    #[test]
    fn reply_headers_are_rewrapped_in_angle_brackets() {
        let parsed = MessageParser::default()
            .parse(b"From: a@x.test\r\n\r\nbody\r\n".as_slice())
            .unwrap_or_else(|| panic!("fixture does not parse"));
        let kept = selected_headers(
            &parsed,
            Some("a@x.test"),
            &["r1@x.test".to_owned(), "r2@x.test".to_owned()],
        );
        assert_eq!(kept[0].value, "<a@x.test>");
        assert_eq!(kept[1].value, "<r1@x.test> <r2@x.test>");
    }
}
