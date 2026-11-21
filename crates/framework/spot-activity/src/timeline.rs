//! The document timeline: every activity record on one time-ordered list (WP-DOC-07.T1).
//!
//! [`Timeline::build`] merges versions, comments, assignments, shares and caller events into
//! one ordered stream. An entry never carries display text: it carries a `kind` and a
//! language-neutral `summary_key` with parameters, so the UI can localise (`activity.changed`
//! `{field, before, after}` and the rest).
//!
//! Order: by time, then by a documented tie-break, so the timeline is deterministic across
//! hosts and runs — the property a sync consumer and a test both need.
//!
//! # The tie-break, in full
//!
//! Two entries at the same instant sort:
//!
//! 1. by kind: versions first (they change the state later entries talk about), then comments,
//!    assignments, shares, then caller events;
//! 2. within one kind, by the record's id: a version by its `doc_version` number (two saves at
//!    one instant ran in version order), a comment or assignment by its record id, a share by
//!    grantee name, an event by its event name and then its own time text;
//! 3. equal on all of that (the same record twice, a caller bug): by summary key — total order,
//!    never unstable, so the same inputs always build the same list.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;

use crate::assignment::Assignment;
use crate::comment::Comment;
use crate::share::Share;
use crate::version::Version;

/// One line of a document's timeline.
///
/// `kind` names the record type; `summary_key` is a stable message id and `summary_params`
/// carries its parameters as JSON. Display text is the UI's job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// When the thing happened (a parameter; entries can never read a clock).
    #[serde(with = "crate::wire::rfc3339")]
    pub at: OffsetDateTime,
    /// What kind of record: `version`, `comment`, `assignment`, `share` or `event`.
    pub kind: EntryKind,
    /// The stable message id the UI localises, e.g. `activity.changed`.
    pub summary_key: String,
    /// The summary's parameters, as JSON in the value encoding.
    pub summary_params: serde_json::Value,
    /// The record's own id in its home list (the version's doc version, the comment's id, …),
    /// as text, so a caller can match a rendered line back to its record.
    pub ref_id: String,
}

/// The kind of record a timeline line carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A save of the document ([`Version`]).
    Version,
    /// A comment ([`Comment`]).
    Comment,
    /// An assignment ([`Assignment`]).
    Assignment,
    /// A share grant ([`Share`]).
    Share,
    /// A caller-supplied event ([`CrateEvent`]).
    Event,
}

impl EntryKind {
    /// The tie-break rank: versions first, events last (rule 1 of the module docs).
    #[must_use]
    pub fn tie_break(self) -> u8 {
        match self {
            EntryKind::Version => 0,
            EntryKind::Comment => 1,
            EntryKind::Assignment => 2,
            EntryKind::Share => 3,
            EntryKind::Event => 4,
        }
    }
}

/// One caller-supplied event: anything else the side panel shows (a post, a cancel, a rename,
/// a state change). The engine names it; the summary key is `activity.event.<event>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrateEvent {
    /// When the event happened (a parameter).
    #[serde(with = "crate::wire::rfc3339")]
    pub at: OffsetDateTime,
    /// The event's kind, e.g. `doc_posted` (an ADR-056 lifecycle topic name or an app's own).
    pub event: String,
    /// The parameters the summary renders from, as JSON in the value encoding.
    pub params: serde_json::Value,
    /// Who caused the event.
    pub actor: spot_app_api::Actor,
}

/// Builds timelines.
#[derive(Clone, Debug, Default)]
pub struct Timeline;

impl Timeline {
    /// Merges every record into one ordered list, by the module's rules: `at`, then kind rank,
    /// then record id, then summary key — total and stable, so the same inputs always build the
    /// same list.
    ///
    /// A comment's line is its **creation**; its edits and deletes are the comment's own
    /// history (the caller shows the current body from the record).
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn build(
        versions: &[Version],
        comments: &[Comment],
        assignments: &[Assignment],
        shares: &[Share],
        events: &[CrateEvent],
    ) -> Vec<TimelineEntry> {
        let mut out: Vec<Sortable> = Vec::new();

        for v in versions {
            // The changes' summary the UI renders from, already language-neutral: each change
            // becomes `activity.changed` `{"field", "before", "after"}`-shaped parameters.
            let change_params: Vec<serde_json::Value> = v
                .changes
                .iter()
                .map(crate::diff::change_summary_params)
                .collect();
            out.push(Sortable {
                rank: (
                    v.at,
                    EntryKind::Version.tie_break(),
                    v.doc_version.0.to_string(),
                    String::new(),
                    String::new(),
                ),
                entry: TimelineEntry {
                    at: v.at,
                    kind: EntryKind::Version,
                    summary_key: "activity.version_changed".to_owned(),
                    summary_params: serde_json::json!({
                        "doc_version": v.doc_version.0,
                        "actor": v.actor,
                        "command_id": v.command_id.as_ref().map(|c| c.as_str()),
                        "changes": change_params,
                    }),
                    ref_id: v.doc_version.0.to_string(),
                },
            });
        }

        for c in comments {
            out.push(Sortable {
                rank: (
                    c.at,
                    EntryKind::Comment.tie_break(),
                    c.id.to_string(),
                    String::new(),
                    String::new(),
                ),
                entry: TimelineEntry {
                    at: c.at,
                    kind: EntryKind::Comment,
                    summary_key: "activity.commented".to_owned(),
                    summary_params: serde_json::json!({
                        "author": c.author,
                        "mentions": c.mentions,
                    }),
                    ref_id: c.id.to_string(),
                },
            });
        }

        for a in assignments {
            out.push(Sortable {
                rank: (
                    a.at,
                    EntryKind::Assignment.tie_break(),
                    a.id.to_string(),
                    String::new(),
                    String::new(),
                ),
                entry: TimelineEntry {
                    at: a.at,
                    kind: EntryKind::Assignment,
                    summary_key: "activity.assignment.assigned".to_owned(),
                    summary_params: serde_json::json!({
                        "assignee": a.assignee,
                        "assigned_by": a.assigned_by,
                        "due": crate::wire::opt_rfc3339_json(&a.due),
                    }),
                    ref_id: a.id.to_string(),
                },
            });
        }

        for s in shares {
            out.push(Sortable {
                rank: (
                    s.at,
                    EntryKind::Share.tie_break(),
                    s.grantee.clone(),
                    String::new(),
                    String::new(),
                ),
                entry: TimelineEntry {
                    at: s.at,
                    kind: EntryKind::Share,
                    summary_key: "activity.shared".to_owned(),
                    summary_params: serde_json::json!({
                        "grantee": s.grantee,
                        "rights": s.rights,
                        "granted_by": s.granted_by,
                        "expires_at": crate::wire::opt_rfc3339_json(&s.expires_at),
                    }),
                    ref_id: s.grantee.clone(),
                },
            });
        }

        for e in events {
            out.push(Sortable {
                rank: (
                    e.at,
                    EntryKind::Event.tie_break(),
                    e.event.clone(),
                    crate::wire::rfc3339_text(&e.at),
                    String::new(),
                ),
                entry: TimelineEntry {
                    at: e.at,
                    kind: EntryKind::Event,
                    summary_key: format!("activity.event.{}", e.event),
                    summary_params: serde_json::json!({
                        "params": e.params,
                        "actor": e.actor,
                    }),
                    ref_id: e.event.clone(),
                },
            });
        }

        // Total order: time, kind, id, then the summary key as the last-ditch totaliser.
        out.sort_by(|a, b| a.rank.cmp(&b.rank));
        out.into_iter().map(|s| s.entry).collect()
    }
}

/// One built line with its sort key; the entry itself carries the public data.
struct Sortable {
    /// `(at, kind rank, record id, extra text, summary key)`: a total, documented order.
    rank: (OffsetDateTime, u8, String, String, String),
    /// The public entry.
    entry: TimelineEntry,
}
