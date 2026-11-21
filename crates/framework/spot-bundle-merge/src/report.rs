//! The reviewable text report: every conflict with the three versions it carries.
//!
//! The report is for a human reviewer deciding how to resolve a merge; it lists the conflicts in
//! result order with their kind, a plain-language explanation and the base/ours/theirs bytes of
//! every item the conflict covers.

use std::fmt::Write as _;

use crate::model::{Conflict, ItemVersion, MergeResult};

/// Renders the merge result as a plain-text report.
pub(crate) fn render(result: &MergeResult) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "bundle merge: {} conflict(s) in {} section(s); clean merge digest {}",
        result.conflicts.len(),
        sections(&result.conflicts).len(),
        result.merged.digest().to_hex()
    );
    if result.conflicts.is_empty() {
        let _ = writeln!(out, "no conflicts: the merge is clean");
        return out;
    }
    let _ = writeln!(out);
    for (index, conflict) in result.conflicts.iter().enumerate() {
        render_conflict(&mut out, index + 1, conflict);
    }
    out
}

/// The distinct section kinds the conflicts cover.
fn sections(conflicts: &[Conflict]) -> Vec<&str> {
    let mut kinds: Vec<&str> = conflicts
        .iter()
        .map(|conflict| conflict.section())
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}

/// Renders one conflict.
fn render_conflict(out: &mut String, ordinal: usize, conflict: &Conflict) {
    let _ = writeln!(
        out,
        "[{ordinal}] {} ({})",
        conflict.id,
        conflict.kind.code()
    );
    let _ = writeln!(out, "    {}", conflict.explanation);
    for version in conflict.versions() {
        let _ = writeln!(out, "    {}", version_line(version));
    }
    if let Some(upgrade) = &conflict.upgrade {
        let _ = writeln!(
            out,
            "    upgrade: {} ({}, {}), release item {}",
            upgrade.code, upgrade.category, upgrade.risk, upgrade.release_item
        );
        if !upgrade.artefacts.is_empty() {
            let _ = writeln!(out, "    artefacts: {}", upgrade.artefacts.join(", "));
        }
    }
    let _ = writeln!(out);
}

/// One item's three versions on one line.
fn version_line(version: &ItemVersion) -> String {
    format!(
        "{}: base {} | ours {} | theirs {}",
        format_args!("{}/{}", version.section, version.key),
        preview(version.base.as_deref()),
        preview(version.ours.as_deref()),
        preview(version.theirs.as_deref())
    )
}

/// A short, one-line preview of an item's bytes.
fn preview(value: Option<&[u8]>) -> String {
    let Some(bytes) = value else {
        return "(absent)".to_owned();
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            let one_line = text.replace(['\n', '\r'], " ");
            let shown: String = one_line.chars().take(80).collect();
            if shown.chars().count() < one_line.chars().count() {
                format!("{shown:?}…")
            } else {
                format!("{shown:?}")
            }
        }
        Err(_) => {
            let mut hex = String::new();
            for byte in bytes.iter().take(16) {
                let _ = write!(hex, "{byte:02x}");
            }
            format!("<{} bytes {}…>", bytes.len(), hex)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_marks_absent_and_truncates() {
        assert_eq!(preview(None), "(absent)");
        assert_eq!(preview(Some(b"x")), "\"x\"");
        let long = vec![b'a'; 100];
        assert!(preview(Some(&long)).ends_with('…'));
        assert!(preview(Some(&[0xff, 0x00])).starts_with('<'));
    }
}
