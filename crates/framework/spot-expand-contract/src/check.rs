//! The usage gate: a contract step against the previous release's usage.
//!
//! ADR-22 forbids removing an object in the same release as the code that stops using it. This
//! module is that rule as a function. Given the release being shipped (its identity, its plan,
//! what its own code touches and the release the train gives the one after it) and the **previous
//! release's** usage manifest, [`check`] walks every [`Contract`](crate::phase::Phase::Contract)
//! change:
//!
//! - the previous release still reads or writes the object → refuse **too early**: the code stops
//!   using it in *this* release, so the contract may ship no earlier than the release after it;
//! - this release's own code still reads or writes it → refuse **while used**: nothing has stopped
//!   using it yet;
//! - neither → allow.
//!
//! A refusal names the object, the release whose code still uses it, the release whose code stops
//! using it (when known) and the earliest release the contract may ship in.
//!
//! # Example
//!
//! ```
//! use spot_expand_contract::{Release, ReleaseId, check};
//! use spot_expand_contract::usage::UsageManifest;
//! use spot_migration_plan::{SchemaPlan, SchemaRefusal, SystemDescription, plan};
//!
//! let schema = SchemaPlan {
//!     steps: Vec::new(),
//!     refusals: vec![SchemaRefusal {
//!         entity: "party".to_owned(),
//!         field: Some("note".to_owned()),
//!         code: "migrate.refused".to_owned(),
//!         reason: "it drops a column".to_owned(),
//!     }],
//! };
//! let release_plan = plan(&SystemDescription::default(), &SystemDescription::default(), &schema);
//! let current = ReleaseId::new("2.0.0").expect("a label");
//! let next = ReleaseId::new("2.1.0").expect("a label");
//! let previous = UsageManifest::from_toml("release = \"1.0.0\"\nobjects = []\n")
//!     .expect("a manifest");
//! let usage = UsageManifest::from_toml("release = \"2.0.0\"\nobjects = []\n")
//!     .expect("a manifest");
//! let release = Release {
//!     id: &current,
//!     next: Some(&next),
//!     plan: &release_plan,
//!     usage: &usage,
//! };
//! // The previous release no longer uses the object, so the contract may ship.
//! assert!(check(&release, &previous).is_ok());
//! ```

use std::fmt;

use spot_migration_plan::ReleasePlan;

use crate::codes;
use crate::object::Object;
use crate::phase::classify;
use crate::release::ReleaseId;
use crate::usage::UsageManifest;

/// The release being shipped, with the facts the gate needs.
#[derive(Clone, Copy, Debug)]
pub struct Release<'a> {
    /// The release's identity.
    pub id: &'a ReleaseId,
    /// The release's plan; its contract changes are what the gate checks.
    pub plan: &'a ReleasePlan,
    /// What this release's own code can touch.
    pub usage: &'a UsageManifest,
    /// The release the train gives the one after this, when it is known. It names the earliest
    /// release a refused contract may ship in.
    pub next: Option<&'a ReleaseId>,
}

/// The earliest release a contract step may ship in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Earliest {
    /// A named release (the train has fixed the label).
    Release(ReleaseId),
    /// One release after the named one; the train has no label for it yet.
    After(ReleaseId),
}

impl fmt::Display for Earliest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Release(id) => write!(f, "`{id}`"),
            Self::After(id) => write!(f, "the release after `{id}`"),
        }
    }
}

/// One contract step the gate allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// The change's id, from the plan.
    pub change_id: String,
    /// The object the contract removes.
    pub object: Object,
    /// Why it is allowed, in a sentence.
    pub detail: String,
}

/// One contract step the gate refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The stable `expand_contract.*` code.
    pub code: &'static str,
    /// The change's id, from the plan.
    pub change_id: String,
    /// The object the contract removes, when the plan named a usable one.
    pub object: Option<Object>,
    /// The change's target, as the plan spelled it.
    pub target: String,
    /// The release whose code still reads or writes the object.
    pub still_used_by: ReleaseId,
    /// The first release whose code no longer uses the object, when known.
    pub stopped_using: Option<ReleaseId>,
    /// The earliest release the contract may ship in.
    pub earliest_allowed: Earliest,
    /// Why the contract is refused and what to do, in a sentence.
    pub detail: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

/// The gate's result: what it allowed and what it refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The contract steps that may ship.
    pub accepted: Vec<Accepted>,
    /// The contract steps that may not, each with the object, the releases and the fix.
    pub refusals: Vec<Refusal>,
}

impl Report {
    /// Whether every contract step may ship.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.refusals.is_empty()
    }

    /// The refusals, in the plan's order.
    #[must_use]
    pub fn refusals(&self) -> &[Refusal] {
        &self.refusals
    }

    /// A deterministic, human-readable report (one line per refusal).
    #[must_use]
    pub fn render_text(&self) -> String {
        let total = self.accepted.len() + self.refusals.len();
        if self.refusals.is_empty() {
            return format!("expand/contract gate: {total} contract step(s) allowed\n");
        }
        let mut out = format!(
            "expand/contract gate: {} of {total} contract step(s) refused\n",
            self.refusals.len()
        );
        for refusal in &self.refusals {
            out.push_str(&format!("- {refusal}\n"));
        }
        out
    }
}

/// Checks a release's contract steps against the previous release's usage.
///
/// The previous release's manifest is the fact the rule turns on: a contract step is allowed only
/// when the previous release's code no longer reads or writes its object. The result's order
/// follows the plan's change order, so a report is stable across runs and machines.
#[must_use]
pub fn check(release: &Release<'_>, previous_release_usage: &UsageManifest) -> Report {
    let mut accepted = Vec::new();
    let mut refusals = Vec::new();

    for classified in classify(release.plan) {
        if !classified.phase.is_contract() {
            continue;
        }
        let Some(object) = classified.object.clone() else {
            refusals.push(Refusal {
                code: codes::CONTRACT_OBJECT_MISSING,
                change_id: classified.change_id,
                object: None,
                target: classified.target.clone(),
                still_used_by: release.id.clone(),
                stopped_using: None,
                earliest_allowed: earliest(release),
                detail: format!(
                    "the {} step `{}` names no usable table or column, so the gate cannot check it",
                    classified.phase, classified.target
                ),
            });
            continue;
        };

        if release.usage.uses(&object) {
            refusals.push(Refusal {
                code: codes::CONTRACT_WHILE_USED,
                change_id: classified.change_id,
                object: Some(object.clone()),
                target: object.to_string(),
                still_used_by: release.id.clone(),
                stopped_using: None,
                earliest_allowed: earliest(release),
                detail: format!(
                    "the {} step removes `{object}`, which release `{}`'s own code still reads or \
                     writes; the code must stop using it first",
                    classified.phase, release.id
                ),
            });
            continue;
        }

        if previous_release_usage.uses(&object) {
            refusals.push(Refusal {
                code: codes::CONTRACT_TOO_EARLY,
                change_id: classified.change_id,
                object: Some(object.clone()),
                target: object.to_string(),
                still_used_by: previous_release_usage.release().clone(),
                stopped_using: Some(release.id.clone()),
                earliest_allowed: earliest(release),
                detail: format!(
                    "`{object}` is still read or written by release `{}`; the code stops using it \
                     in `{}`, so the earliest release this {} step may ship in is {} (ADR-22)",
                    previous_release_usage.release(),
                    release.id,
                    classified.phase,
                    earliest(release)
                ),
            });
            continue;
        }

        accepted.push(Accepted {
            change_id: classified.change_id,
            object: object.clone(),
            detail: format!(
                "release `{}` no longer uses `{object}`, so the {} step may ship",
                previous_release_usage.release(),
                classified.phase
            ),
        });
    }

    Report { accepted, refusals }
}

/// The earliest release a contract refused today may ship in: the named next release when the
/// train has one, otherwise "the release after" this one.
fn earliest(release: &Release<'_>) -> Earliest {
    match release.next {
        Some(next) => Earliest::Release(next.clone()),
        None => Earliest::After(release.id.clone()),
    }
}
