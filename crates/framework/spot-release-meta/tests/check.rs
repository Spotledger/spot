#![allow(missing_docs)]

//! `check_release`: every way a release can leave a patch undeclared.

use std::collections::BTreeSet;

use spot_migration_plan::{PatchDecl as PlanPatchDecl, Risk};
use spot_release_meta::check::NoContext;
use spot_release_meta::{
    PatchDecl, PatchRef, ProblemKind, ReleaseContext, check_release, check_release_with,
};

fn decl(id: &str) -> PatchDecl {
    PatchDecl {
        id: id.to_owned(),
        intent: "do the thing".to_owned(),
        doctypes: Vec::new(),
        class: Risk::Additive,
        reversible: true,
        idempotent: true,
        down: None,
        requires_epoch: false,
        irreversible_ack: false,
        guard: None,
    }
}

fn row(id: &str) -> PlanPatchDecl {
    PlanPatchDecl {
        id: id.to_owned(),
        intent: Some("schema".to_owned()),
        reversible: true,
        idempotent: true,
        deps: Vec::new(),
    }
}

fn kinds(problems: &[spot_release_meta::Problem]) -> Vec<ProblemKind> {
    problems.iter().map(|p| p.kind).collect()
}

struct Ctx {
    known: BTreeSet<String>,
    stopped: Vec<String>,
}

impl ReleaseContext for Ctx {
    fn knows_doctype(&self, doctype: &str) -> bool {
        self.known.contains(doctype)
    }
    fn stopped_using(&self) -> Vec<String> {
        self.stopped.clone()
    }
}

#[test]
fn a_declared_release_is_clean() {
    let patch = decl("p1");
    let on_disk = vec![row("p1")];
    assert!(check_release(&on_disk, std::slice::from_ref(&patch)).is_empty());
}

#[test]
fn a_patch_with_no_declaration_is_reported() {
    let problems = check_release(&[row("p1"), row("p2")], &[decl("p1")]);
    assert_eq!(kinds(&problems), vec![ProblemKind::MissingDeclaration]);
    assert_eq!(problems[0].patch.as_deref(), Some("p2"));
}

#[test]
fn a_declaration_with_no_patch_is_reported() {
    let problems = check_release(&[row("p1")], &[decl("p1"), decl("gone")]);
    assert_eq!(kinds(&problems), vec![ProblemKind::DanglingDeclaration]);
    assert_eq!(problems[0].patch.as_deref(), Some("gone"));
}

#[test]
fn an_irreversible_patch_needs_a_down_step_or_an_acknowledgement() {
    let mut patch = decl("p1");
    patch.reversible = false;
    let mut on_disk = row("p1");
    on_disk.reversible = false;

    let problems = check_release(&[on_disk.clone()], std::slice::from_ref(&patch));
    assert_eq!(kinds(&problems), vec![ProblemKind::IrreversibleWithoutDown]);

    patch.down = Some(PatchRef::new("p1_down"));
    assert!(check_release(&[on_disk.clone()], std::slice::from_ref(&patch)).is_empty());

    patch.down = None;
    patch.irreversible_ack = true;
    assert!(check_release(&[on_disk], std::slice::from_ref(&patch)).is_empty());
}

#[test]
fn a_non_idempotent_patch_needs_a_guard() {
    let mut patch = decl("p1");
    patch.idempotent = false;
    let mut on_disk = row("p1");
    on_disk.idempotent = false;

    let problems = check_release(&[on_disk.clone()], std::slice::from_ref(&patch));
    assert_eq!(
        kinds(&problems),
        vec![ProblemKind::NonIdempotentWithoutGuard]
    );

    patch.guard = Some("insert only when absent".to_owned());
    assert!(check_release(&[on_disk], std::slice::from_ref(&patch)).is_empty());
}

#[test]
fn an_unknown_doctype_is_reported() {
    let mut patch = decl("p1");
    patch.doctypes = vec!["Sales Invoice".to_owned(), "Ghost".to_owned()];
    let mut known = BTreeSet::new();
    known.insert("Sales Invoice".to_owned());
    let context = Ctx {
        known,
        stopped: Vec::new(),
    };
    let problems = check_release_with(&[row("p1")], &[patch], &context);
    assert_eq!(kinds(&problems), vec![ProblemKind::UnknownDocType]);
    assert!(problems[0].detail.contains("Ghost"));
}

#[test]
fn a_destructive_patch_while_the_code_stops_using_it_is_reported() {
    let mut patch = decl("p1");
    patch.class = Risk::Destructive;
    patch.doctypes = vec!["Sales Invoice".to_owned()];
    let context = Ctx {
        known: BTreeSet::from(["Sales Invoice".to_owned()]),
        stopped: vec!["Sales Invoice".to_owned()],
    };
    let problems = check_release_with(&[row("p1")], &[patch], &context);
    assert_eq!(
        kinds(&problems),
        vec![ProblemKind::DestructiveWhileStopIsSameRelease]
    );
}

#[test]
fn a_destructive_patch_with_no_stop_is_allowed_this_release() {
    let mut patch = decl("p1");
    patch.class = Risk::Destructive;
    patch.doctypes = vec!["Sales Invoice".to_owned()];
    let context = Ctx {
        known: BTreeSet::from(["Sales Invoice".to_owned()]),
        stopped: Vec::new(),
    };
    assert!(check_release_with(&[row("p1")], &[patch], &context).is_empty());
}

#[test]
fn the_no_context_check_skips_registry_rules() {
    let mut patch = decl("p1");
    patch.doctypes = vec!["Anything At All".to_owned()];
    assert!(check_release_with(&[row("p1")], &[patch], &NoContext).is_empty());
}

#[test]
fn problems_are_deterministic() {
    // Two patches, each with two problems, in reverse id order, must come out sorted.
    let mut a = decl("a");
    a.reversible = false;
    a.idempotent = false;
    let mut b = decl("b");
    b.reversible = false;
    b.idempotent = false;
    let rows = [row("a"), row("b")].map(|mut r| {
        r.reversible = false;
        r.idempotent = false;
        r
    });
    let first = check_release(&rows, &[b.clone(), a.clone()]);
    let second = check_release(&rows, &[a, b]);
    assert_eq!(first, second);
    assert_eq!(
        kinds(&first),
        vec![
            ProblemKind::IrreversibleWithoutDown,
            ProblemKind::IrreversibleWithoutDown,
            ProblemKind::NonIdempotentWithoutGuard,
            ProblemKind::NonIdempotentWithoutGuard,
        ]
    );
    assert_eq!(first[0].patch.as_deref(), Some("a"));
}

#[test]
fn a_declaration_that_disagrees_with_the_manifest_flags_is_refused() {
    let mut declared = decl("p1");
    declared.reversible = false;
    declared.irreversible_ack = true;
    let problems = check_release(&[row("p1")], &[declared]);
    assert_eq!(kinds(&problems), vec![ProblemKind::FlagMismatch]);
}
