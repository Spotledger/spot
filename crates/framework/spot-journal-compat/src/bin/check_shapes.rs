//! The shape-change gate: fails when a catalog patch declares a stored-shape change that no
//! upcaster covers (ADR-061 s6).
//!
//! `check_shapes` runs `check_patch` over today's upcaster table and over the shape changes the
//! patches since the frozen release declared. It prints every uncovered change — the entity, the
//! kind and the versions the chain stops between — and exits non-zero, so a patch cannot ship a
//! rename, a drop, a retype or a move without the upcaster that rewrites the images written
//! before it. An additive change (a new entity, a new field) needs no upcaster and is skipped.
//!
//! `--omit <entity>:<from>` leaves one step out of the table. It exists so the tests can prove
//! the gate fails when an upcaster is missing; the CI job never passes it.
//!
//! Exit codes: `0` every change is covered, `1` at least one is not, `2` the arguments or the
//! upcaster table itself are unusable.

use spot_journal_compat::gate_findings_omitting;

fn main() {
    let omissions = match parse(std::env::args().skip(1)) {
        Ok(omissions) => omissions,
        Err(reason) => {
            eprintln!("check_shapes: {reason}");
            std::process::exit(2);
        }
    };
    let borrowed: Vec<(&str, u64)> = omissions
        .iter()
        .map(|(entity, from)| (entity.as_str(), *from))
        .collect();
    let findings = match gate_findings_omitting(&borrowed) {
        Ok(findings) => findings,
        Err(error) => {
            eprintln!("check_shapes: {error}");
            std::process::exit(2);
        }
    };
    if findings.is_empty() {
        println!("check_shapes: every declared shape change has an upcaster");
        return;
    }
    for finding in &findings {
        println!("check_shapes: {finding}");
    }
    eprintln!(
        "check_shapes: {} shape change(s) have no upcaster; the patch cannot ship",
        findings.len()
    );
    std::process::exit(1);
}

/// The `--omit entity:from` arguments.
fn parse<I: Iterator<Item = String>>(args: I) -> Result<Vec<(String, u64)>, String> {
    let mut omissions: Vec<(String, u64)> = Vec::new();
    let mut args = args.peekable();
    while let Some(argument) = args.next() {
        if argument != "--omit" {
            return Err(format!("unexpected argument {argument}"));
        }
        let value = args
            .next()
            .ok_or_else(|| "--omit needs an <entity>:<from> value".to_owned())?;
        let (entity, from) = value
            .split_once(':')
            .ok_or_else(|| format!("{value} is not <entity>:<from>"))?;
        let from: u64 = from
            .parse()
            .map_err(|_| format!("{value} does not name a schema version"))?;
        omissions.push((entity.to_owned(), from));
    }
    Ok(omissions)
}
