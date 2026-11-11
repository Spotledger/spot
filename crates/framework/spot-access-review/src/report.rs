//! The auditor's view: counts, per reviewer progress, removals with reasons and every kept
//! stale or overridden item with its note — deterministic in item order — and CSV export
//! with correct quoting (commas, quotes and newlines) for the auditor's tools.

use crate::campaign::ReviewerProgress;
use crate::model::{CampaignSpec, CampaignStatus, ReviewItem, ReviewItemState};

/// The access review campaign's report: what the auditor reads. Deterministic: the
/// per-item rows follow the snapshot's input order, the reviewer table is alphabetical.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewReport {
    /// How many items the campaign holds.
    pub total: usize,
    /// How many items were kept by their reviewer or escalation target.
    pub kept: usize,
    /// How many items were removed (by decision or at the deadline).
    pub removed: usize,
    /// How many items are currently escalated (at creation or at the deadline) and not
    /// yet decided.
    pub escalated: usize,
    /// How many items still await a decision, escalated or not; `kept + removed + pending`
    /// is always `total`.
    pub pending: usize,
    /// Per reviewer progress, alphabetically by reviewer.
    pub reviewers: Vec<ReviewerProgress>,
    /// The removals, in item order, with their audited reasons.
    pub removals: Vec<RemovalRow>,
    /// Every kept item that was stale or carried a SoD override at decision time, with
    /// the note the tenant owed, in item order.
    pub kept_flagged: Vec<KeptRow>,
}

/// One removal row: the item, its user and assignment, and the audited reason.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovalRow {
    /// The item id.
    pub item: String,
    /// The user whose access it was.
    pub user: String,
    /// The assignment removed, as the reference text the campaign was given.
    pub assignment: String,
    /// The reason, audited verbatim (`unreviewed at deadline` for a deadline removal).
    pub reason: String,
}

/// One kept-but-flagged row: a stale or SoD-overridden access the tenant chose to keep,
/// with the note that said why.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeptRow {
    /// The item id.
    pub item: String,
    /// The user whose access it is.
    pub user: String,
    /// The assignment kept, as the reference text the campaign was given.
    pub assignment: String,
    /// What made the item flag-worthy: `stale`, `overridden`, or both, in that order.
    pub flags: Vec<String>,
    /// The note the keep carried.
    pub note: Option<String>,
}

impl ReviewReport {
    /// Builds the report from the spec, the snapshot and the per-item state.
    #[must_use]
    pub fn build(
        spec: &CampaignSpec,
        items: &[ReviewItem],
        state: &[ReviewItemState],
        status: CampaignStatus,
    ) -> Self {
        let _ = (spec, status);
        let total = items.len();
        let mut kept = 0;
        let mut escalated = 0;
        let mut pending = 0;
        let mut removals = Vec::new();
        let mut kept_flagged = Vec::new();
        for (idx, item) in items.iter().enumerate() {
            match &state[idx] {
                ReviewItemState::Pending { escalated_to } => {
                    pending += 1;
                    if escalated_to.is_some() {
                        escalated += 1;
                    }
                }
                ReviewItemState::Kept {
                    note,
                    stale,
                    overridden,
                    ..
                } => {
                    kept += 1;
                    if *stale || *overridden {
                        let mut flags = Vec::new();
                        if *stale {
                            flags.push("stale".to_string());
                        }
                        if *overridden {
                            flags.push("overridden".to_string());
                        }
                        kept_flagged.push(KeptRow {
                            item: item.id.clone(),
                            user: item.user.clone(),
                            assignment: item.assignment.to_string(),
                            flags,
                            note: note.clone(),
                        });
                    }
                }
                ReviewItemState::Removed { reason, .. } => {
                    removals.push(RemovalRow {
                        item: item.id.clone(),
                        user: item.user.clone(),
                        assignment: item.assignment.to_string(),
                        reason: reason.clone(),
                    });
                }
            }
        }
        let removed = removals.len();

        // Per reviewer: their load (original assignment) and how many of those were
        // decided by them; sorted alphabetically by the map.
        let mut progress: std::collections::BTreeMap<&str, (usize, usize)> =
            std::collections::BTreeMap::new();
        for item in items {
            let entry = progress.entry(item.reviewer.as_str()).or_insert((0, 0));
            entry.0 += 1;
        }
        for (idx, item) in items.iter().enumerate() {
            if state[idx].is_terminal()
                && let Some(entry) = progress.get_mut(item.reviewer.as_str())
            {
                entry.1 += 1;
            }
        }
        let reviewers = progress
            .into_iter()
            .map(|(name, (reviewer_total, decided))| ReviewerProgress {
                reviewer: name.to_string(),
                total: reviewer_total,
                decided,
                pending: reviewer_total.saturating_sub(decided),
            })
            .collect();

        Self {
            total,
            kept,
            removed,
            escalated,
            pending,
            reviewers,
            removals,
            kept_flagged,
        }
    }

    /// Whether nothing was decided yet: every item still awaits a decision.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == self.pending
    }

    /// The report as CSV for the auditor: a header row, then one row per removal and one
    /// row per kept stale or overridden item. Fields with a comma, quote, newline or
    /// carriage return are quoted (RFC 4180), a quote doubling.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = String::from("section,item,user,assignment,reason_or_note\n");
        for r in &self.removals {
            out.push_str(&format!(
                "removed,{},{},{},{}\n",
                csv_field(&r.item),
                csv_field(&r.user),
                csv_field(&r.assignment),
                csv_field(&r.reason)
            ));
        }
        for k in &self.kept_flagged {
            let mut field = k.flags.join("+");
            if let Some(note) = &k.note {
                if !field.is_empty() {
                    field.push_str(": ");
                }
                field.push_str(note);
            }
            out.push_str(&format!(
                "kept,{},{},{},{}\n",
                csv_field(&k.item),
                csv_field(&k.user),
                csv_field(&k.assignment),
                csv_field(&field)
            ));
        }
        out
    }
}

/// Quotes one CSV field when needed (RFC 4180): a field with a quote, comma, newline or
/// carriage return is wrapped in quotes, a quote doubling.
fn csv_field(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}
