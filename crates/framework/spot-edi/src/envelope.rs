//! Envelope validation (WP-INT-06.T1, brief item 2).
//!
//! Checks the counts and control numbers each envelope level declares against what the document
//! actually holds: X12's `ISA`/`IEA`, `GS`/`GE` and `ST`/`SE`, and EDIFACT's `UNB`/`UNZ`,
//! `UNG`/`UNE` and `UNH`/`UNT`. Every problem is reported as a [`Violation`] with its position,
//! so a sender sees the whole list at once. The control-number comparison is the mismatch
//! detection the brief calls for (item 7).

use crate::error::codes::{ENVELOPE_CONTROL, ENVELOPE_COUNT, UNBALANCED_ENVELOPE};
use crate::model::{Interchange, Segment};
use crate::report::{Position, Violation};

/// Checks every envelope level of `interchange`, returning all violations (empty when the
/// envelope is consistent).
pub fn check(interchange: &Interchange) -> Vec<Violation> {
    let all = interchange.all_segments();
    let mut out = Vec::new();
    match interchange.syntax {
        crate::delimiters::Syntax::X12 => check_x12(&all, &mut out),
        crate::delimiters::Syntax::Edifact => check_edifact(&all, &mut out),
    }
    out
}

/// Whether the interchange's envelope is consistent.
pub fn is_consistent(interchange: &Interchange) -> bool {
    check(interchange).is_empty()
}

fn check_x12(all: &[&Segment], out: &mut Vec<Violation>) {
    let trailer_index = all.len().saturating_sub(1);
    let header = all[0];
    let trailer = all[trailer_index];
    compare_control(
        "IEA02",
        trailer.text(1),
        header.text(12),
        Position::segment(trailer_index, trailer),
        out,
    );

    let body_end = trailer_index;
    let group_starts: Vec<usize> = (1..body_end).filter(|&i| all[i].tag == "GS").collect();
    compare_count(
        "IEA01",
        trailer.text(0),
        group_starts.len() as u64,
        Position::segment(trailer_index, trailer),
        out,
    );
    for start in &group_starts {
        let start = *start;
        match find_closing(all, start, "GE", body_end) {
            None => out.push(Violation::new(
                UNBALANCED_ENVELOPE,
                Position::segment(start, all[start]),
                "the GS segment has no GE segment".to_owned(),
            )),
            Some(end) => {
                let messages = count_between(all, start, end, "ST");
                compare_count(
                    "GE01",
                    all[end].text(0),
                    messages,
                    Position::segment(end, all[end]),
                    out,
                );
                compare_control(
                    "GE02",
                    all[end].text(1),
                    all[start].text(5),
                    Position::segment(end, all[end]),
                    out,
                );
            }
        }
    }

    let message_starts: Vec<usize> = (1..body_end).filter(|&i| all[i].tag == "ST").collect();
    for start in message_starts {
        match find_closing(all, start, "SE", body_end) {
            None => out.push(Violation::new(
                UNBALANCED_ENVELOPE,
                Position::segment(start, all[start]),
                "the ST segment has no SE segment".to_owned(),
            )),
            Some(end) => {
                let found = (end - start + 1) as u64;
                compare_count(
                    "SE01",
                    all[end].text(0),
                    found,
                    Position::segment(end, all[end]),
                    out,
                );
                compare_control(
                    "SE02",
                    all[end].text(1),
                    all[start].text(1),
                    Position::segment(end, all[end]),
                    out,
                );
            }
        }
    }
}

fn check_edifact(all: &[&Segment], out: &mut Vec<Violation>) {
    let trailer_index = all.len().saturating_sub(1);
    let header = all[0];
    let trailer = all[trailer_index];
    compare_control(
        "UNZ02",
        trailer.text(1),
        header.text(4),
        Position::segment(trailer_index, trailer),
        out,
    );

    let body_end = trailer_index;
    let group_starts: Vec<usize> = (1..body_end).filter(|&i| all[i].tag == "UNG").collect();
    let message_count = count_tag(all, 1, body_end, "UNH");
    // UNZ01 counts the groups when the interchange is grouped, and the messages otherwise.
    let expected = if group_starts.is_empty() {
        message_count
    } else {
        group_starts.len() as u64
    };
    compare_count(
        "UNZ01",
        trailer.text(0),
        expected,
        Position::segment(trailer_index, trailer),
        out,
    );

    for start in &group_starts {
        let start = *start;
        match find_closing(all, start, "UNE", body_end) {
            None => out.push(Violation::new(
                UNBALANCED_ENVELOPE,
                Position::segment(start, all[start]),
                "the UNG segment has no UNE segment".to_owned(),
            )),
            Some(end) => {
                let messages = count_between(all, start, end, "UNH");
                compare_count(
                    "UNE01",
                    all[end].text(0),
                    messages,
                    Position::segment(end, all[end]),
                    out,
                );
                compare_control(
                    "UNE02",
                    all[end].text(1),
                    all[start].text(4),
                    Position::segment(end, all[end]),
                    out,
                );
            }
        }
    }

    let message_starts: Vec<usize> = (1..body_end).filter(|&i| all[i].tag == "UNH").collect();
    for start in message_starts {
        match find_closing(all, start, "UNT", body_end) {
            None => out.push(Violation::new(
                UNBALANCED_ENVELOPE,
                Position::segment(start, all[start]),
                "the UNH segment has no UNT segment".to_owned(),
            )),
            Some(end) => {
                let found = (end - start + 1) as u64;
                compare_count(
                    "UNT01",
                    all[end].text(0),
                    found,
                    Position::segment(end, all[end]),
                    out,
                );
                compare_control(
                    "UNT02",
                    all[end].text(1),
                    all[start].text(0),
                    Position::segment(end, all[end]),
                    out,
                );
            }
        }
    }
}

/// The first index after `start` (and before `end`) whose tag is `tag`.
fn find_closing(all: &[&Segment], start: usize, tag: &str, end: usize) -> Option<usize> {
    (start + 1..end).find(|&i| all[i].tag == tag)
}

/// How many segments with `tag` sit strictly between `start` and `end`.
fn count_between(all: &[&Segment], start: usize, end: usize, tag: &str) -> u64 {
    count_tag(all, start + 1, end, tag)
}

/// How many segments with `tag` sit in `[start, end)`.
fn count_tag(all: &[&Segment], start: usize, end: usize, tag: &str) -> u64 {
    (start..end).filter(|&i| all[i].tag == tag).count() as u64
}

/// Compares a declared count with the count found.
fn compare_count(
    what: &str,
    declared: &str,
    found: u64,
    position: Position,
    out: &mut Vec<Violation>,
) {
    match declared.trim().parse::<u64>() {
        Ok(declared) if declared == found => {}
        Ok(declared) => out.push(Violation::new(
            ENVELOPE_COUNT,
            position,
            format!("{what} declares {declared} but the content has {found}"),
        )),
        Err(_) => out.push(Violation::new(
            ENVELOPE_COUNT,
            position,
            format!("{what} declares {declared:?}, which is not a number"),
        )),
    }
}

/// Compares two control numbers that must match.
fn compare_control(
    what: &str,
    found: &str,
    expected: &str,
    position: Position,
    out: &mut Vec<Violation>,
) {
    if found.trim() != expected.trim() {
        out.push(Violation::new(
            ENVELOPE_CONTROL,
            position,
            format!(
                "{what} control {:?} does not match {:?}",
                found.trim(),
                expected.trim()
            ),
        ));
    }
}
