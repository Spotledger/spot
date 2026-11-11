//! The channels a value can leave through (ADR-070 section 12, rule 6).
//!
//! Adding a channel must break every `match` on it, so the enum is deliberately **not**
//! `#[non_exhaustive]`: a new channel cannot ship without choosing how it is protected, and
//! [`Channel::ALL`] (which the leak matrix iterates) must list it.

/// The right a channel needs on a field before a value may leave through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// `read` (the real value) or `read_masked` (the masked form), as the document read path.
    Read,
    /// `report`: list columns, reports, exports and search results, separate from `read`.
    Report,
    /// No right is enough: the channel carries field **names** only, never a value.
    NamesOnly,
}

/// One way data leaves the platform other than the document read path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Channel {
    /// User-facing history and audit diffs built from before and after images.
    /// Needs `read` / `read_masked`; a masked field masks **both** sides of a change.
    Version,
    /// A file export (CSV, spreadsheet, JSON). Needs `report`; a column the caller may not
    /// report is refused, never silently dropped.
    Export,
    /// A printed or rendered document. Needs `report`, but degrades to the masked form
    /// instead of refusing, so a print format still renders.
    Print,
    /// A report result (grouped, aggregated or listed). Needs `report`; a query may not
    /// filter, sort, group or aggregate on a field the caller cannot read unmasked.
    Report,
    /// A column of a list view. Needs `report`.
    ListColumn,
    /// A link lookup and a link's display title. Needs `read` / `read_masked`.
    LinkTitle,
    /// The search index and the snippets of its results. Needs `report`.
    Search,
    /// A notification payload (mail, push, realtime notice). **Never carries a value**:
    /// field names only.
    Notification,
    /// The event journal (ADR-061). **Never carries a value**: field names only; the
    /// journal's own redaction is a different job.
    Journal,
    /// A provisioning or SCIM style API that mirrors a record out of the platform. Needs
    /// `read` / `read_masked`.
    ApiScim,
}

impl Channel {
    /// Every channel, in declaration order. The leak matrix and the tests iterate this.
    pub const ALL: [Channel; 10] = [
        Channel::Version,
        Channel::Export,
        Channel::Print,
        Channel::Report,
        Channel::ListColumn,
        Channel::LinkTitle,
        Channel::Search,
        Channel::Notification,
        Channel::Journal,
        Channel::ApiScim,
    ];

    /// The right the channel needs. An exhaustive match: a new channel must choose.
    #[must_use]
    pub fn need(self) -> Need {
        match self {
            Channel::Version | Channel::LinkTitle | Channel::ApiScim => Need::Read,
            Channel::Export
            | Channel::Print
            | Channel::Report
            | Channel::ListColumn
            | Channel::Search => Need::Report,
            Channel::Notification | Channel::Journal => Need::NamesOnly,
        }
    }

    /// Whether a value may ever leave through the channel. `false` for the channels that
    /// carry field names only.
    #[must_use]
    pub fn carries_values(self) -> bool {
        !matches!(self.need(), Need::NamesOnly)
    }

    /// Whether a column the caller may not report is shown masked instead of refused.
    #[must_use]
    pub fn degrades_to_masked(self) -> bool {
        matches!(self, Channel::Print)
    }

    /// The stable lower-case name used in trace records and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Version => "version",
            Channel::Export => "export",
            Channel::Print => "print",
            Channel::Report => "report",
            Channel::ListColumn => "list_column",
            Channel::LinkTitle => "link_title",
            Channel::Search => "search",
            Channel::Notification => "notification",
            Channel::Journal => "journal",
            Channel::ApiScim => "api_scim",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_channel_once() {
        let mut seen = std::collections::BTreeSet::new();
        for c in Channel::ALL {
            assert!(seen.insert(c), "{c:?} listed twice");
            // The exhaustive match below fails to compile when a variant is added without
            // being listed in `ALL`'s arity.
            match c {
                Channel::Version
                | Channel::Export
                | Channel::Print
                | Channel::Report
                | Channel::ListColumn
                | Channel::LinkTitle
                | Channel::Search
                | Channel::Notification
                | Channel::Journal
                | Channel::ApiScim => {}
            }
        }
        assert_eq!(seen.len(), 10);
    }

    #[test]
    fn only_notification_and_journal_carry_no_value() {
        let names_only: Vec<_> = Channel::ALL
            .into_iter()
            .filter(|c| !c.carries_values())
            .collect();
        assert_eq!(names_only, vec![Channel::Notification, Channel::Journal]);
    }

    #[test]
    fn only_print_degrades() {
        let d: Vec<_> = Channel::ALL
            .into_iter()
            .filter(|c| c.degrades_to_masked())
            .collect();
        assert_eq!(d, vec![Channel::Print]);
    }
}
