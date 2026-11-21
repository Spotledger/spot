//! `check_release`: the CI gate that rejects an undeclared patch.
//!
//! A release is two lists: the patches that exist on disk (the app manifests' `[[patches]]`
//! rows, [`spot_migration_plan::PatchDecl`]) and the declarations this contract requires
//! (`release-meta/patches/<id>.toml`, [`PatchDecl`](crate::PatchDecl)). [`check_release`] returns
//! one [`Problem`] for every way the two can disagree:
//!
//! - a patch on disk with no declaration;
//! - a declaration with no patch on disk;
//! - an irreversible patch with no down-step and no explicit acknowledgement;
//! - a non-idempotent patch with no guard;
//! - a destructive patch in the same release as code that stops using it (the expand/contract
//!   rule of ADR-22, handed to MIG-09 through [`ReleaseContext`]);
//! - a declaration naming a DocType the platform does not know;
//! - a declaration whose `reversible` or `idempotent` flag disagrees with the manifest row.
//!
//! The registry and expand/contract facts come from the caller through [`ReleaseContext`], so
//! this crate stays pure and the migration planner and MIG-09 can supply real data.

use std::collections::BTreeSet;

use spot_migration_plan::{PatchDecl as PlanPatchDecl, Risk};

use crate::patch::PatchDecl;

/// What is wrong with a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProblemKind {
    /// A patch exists on disk but has no declaration.
    MissingDeclaration,
    /// A declaration names a patch that exists nowhere on disk.
    DanglingDeclaration,
    /// An irreversible patch has no down-step and no explicit acknowledgement.
    IrreversibleWithoutDown,
    /// A non-idempotent patch declares no guard.
    NonIdempotentWithoutGuard,
    /// A destructive patch ships in the same release as the code that stops using its object.
    DestructiveWhileStopIsSameRelease,
    /// A declaration names a DocType the platform does not know.
    UnknownDocType,
    /// The manifest row and the declaration disagree on `reversible` or `idempotent`.
    FlagMismatch,
}

impl ProblemKind {
    /// The stable label, for a report or a CI annotation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingDeclaration => "missing-declaration",
            Self::DanglingDeclaration => "dangling-declaration",
            Self::IrreversibleWithoutDown => "irreversible-without-down",
            Self::NonIdempotentWithoutGuard => "non-idempotent-without-guard",
            Self::DestructiveWhileStopIsSameRelease => "destructive-while-stop-is-same-release",
            Self::UnknownDocType => "unknown-doctype",
            Self::FlagMismatch => "flag-mismatch",
        }
    }
}

/// One reason a release is refused.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Problem {
    /// What is wrong.
    pub kind: ProblemKind,
    /// The patch the problem is about, when it is about a patch.
    pub patch: Option<String>,
    /// A sentence naming the object and the fix.
    pub detail: String,
}

impl Problem {
    /// A problem about a patch.
    pub fn patch(kind: ProblemKind, patch: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            patch: Some(patch.into()),
            detail: detail.into(),
        }
    }

    /// A problem not tied to a single patch.
    pub fn release(kind: ProblemKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            patch: None,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.patch {
            Some(patch) => write!(f, "{} `{patch}`: {}", self.kind.as_str(), self.detail),
            None => write!(f, "{}: {}", self.kind.as_str(), self.detail),
        }
    }
}

/// The facts a release check needs from outside this crate.
///
/// WP-MIG-09 (`spot-expand-contract`) implements this (or a wrapper does) so a destructive patch
/// is refused in the same release as the code that stops using its object, and the platform
/// names the DocTypes it knows.
pub trait ReleaseContext {
    /// Whether the platform knows this DocType. An installation may add DocTypes, so this is
    /// asked of the caller, never compiled in.
    fn knows_doctype(&self, doctype: &str) -> bool;

    /// The objects this release's code stops reading or writing. MIG-09 supplies these from its
    /// usage manifest; a destructive patch on one of them may only ship one release later.
    fn stopped_using(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A context that knows every DocType and no stopped-using facts: the checks that need no
/// external data still run.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoContext;

impl ReleaseContext for NoContext {
    fn knows_doctype(&self, _doctype: &str) -> bool {
        true
    }
}

impl ReleaseContext for () {
    fn knows_doctype(&self, _doctype: &str) -> bool {
        true
    }
}

/// Checks a release with only the two lists: the declaration-local rules and the
/// presence rules. The DocType and expand/contract rules need a [`ReleaseContext`]; use
/// [`check_release_with`] for those.
#[must_use]
pub fn check_release(
    patches_on_disk: &[PlanPatchDecl],
    declarations: &[PatchDecl],
) -> Vec<Problem> {
    check_release_with(patches_on_disk, declarations, &NoContext)
}

/// Checks a release against a [`ReleaseContext`].
///
/// The returned problems are deterministic (sorted by kind, then patch, then detail), so a
/// report is stable across runs and machines.
#[must_use]
pub fn check_release_with(
    patches_on_disk: &[PlanPatchDecl],
    declarations: &[PatchDecl],
    context: &dyn ReleaseContext,
) -> Vec<Problem> {
    let on_disk: BTreeSet<&str> = patches_on_disk.iter().map(|p| p.id.as_str()).collect();
    let declared: BTreeSet<&str> = declarations.iter().map(|d| d.id.as_str()).collect();
    let stopped: BTreeSet<String> = context.stopped_using().into_iter().collect();

    let mut problems = Vec::new();

    for id in &on_disk {
        if !declared.contains(id) {
            problems.push(Problem::patch(
                ProblemKind::MissingDeclaration,
                *id,
                "the patch exists on disk but has no `release-meta/patches` declaration",
            ));
        }
    }
    for id in &declared {
        if !on_disk.contains(id) {
            problems.push(Problem::patch(
                ProblemKind::DanglingDeclaration,
                *id,
                "the declaration names a patch that exists nowhere on disk",
            ));
        }
    }

    for decl in declarations {
        if let Some(row) = patches_on_disk.iter().find(|p| p.id == decl.id)
            && (row.reversible != decl.reversible || row.idempotent != decl.idempotent)
        {
            problems.push(Problem::patch(
                ProblemKind::FlagMismatch,
                decl.id.clone(),
                format!(
                    "the manifest row says reversible={}, idempotent={} but the declaration says reversible={}, idempotent={}; make them agree",
                    row.reversible, row.idempotent, decl.reversible, decl.idempotent
                ),
            ));
        }
        if !decl.reversible && decl.down.is_none() && !decl.irreversible_ack {
            problems.push(Problem::patch(
                ProblemKind::IrreversibleWithoutDown,
                decl.id.clone(),
                "the patch is irreversible: declare a down-step, or acknowledge it with `irreversible_ack = true`",
            ));
        }
        if !decl.idempotent && decl.guard.is_none() {
            problems.push(Problem::patch(
                ProblemKind::NonIdempotentWithoutGuard,
                decl.id.clone(),
                "the patch is not idempotent: declare the `guard` that makes a second run safe",
            ));
        }
        for doctype in &decl.doctypes {
            if !context.knows_doctype(doctype) {
                problems.push(Problem::patch(
                    ProblemKind::UnknownDocType,
                    decl.id.clone(),
                    format!("the declaration names the unknown DocType `{doctype}`"),
                ));
            }
        }
        if decl.class == Risk::Destructive {
            for doctype in &decl.doctypes {
                if stopped.contains(doctype) {
                    problems.push(Problem::patch(
                        ProblemKind::DestructiveWhileStopIsSameRelease,
                        decl.id.clone(),
                        format!(
                            "the destructive patch touches `{doctype}`, which this release's code stops using; a contract step ships one release later (ADR-22, MIG-09)"
                        ),
                    ));
                }
            }
        }
    }

    problems.sort();
    problems
}
