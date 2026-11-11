//! [`SodAuthorship`]: separation of duties over `spot-sod`.
//!
//! Authoring a plan and approving it are two duties that conflict **in the scope of that
//! plan** (its digest). The sink of authorship records is the set of `author` assignments; the
//! question "may this person approve?" is `spot_sod::check_grant` for the approve duty.

use std::sync::{Arc, Mutex};

use spot_sod::{Assignment, GrantCheck, RoleCatalogue, RuleSet, check_grant};

use crate::approval::{ApprovalStore, AuthorshipCheck};
use crate::digest::PlanDigest;
use crate::error::ApprovalError;

/// The duty held by whoever authored a plan.
pub const AUTHOR_DUTY: &str = "migration_author_plan";
/// The duty held by whoever approves a plan.
pub const APPROVE_DUTY: &str = "migration_approve_plan";

const RULES: &str = r#"
[[rules]]
id = "sod_migration_author_approver"
title = "Author and approver of a release plan"
description = "A person who authored a release plan may not approve it."
severity = "critical"
scope = "same_scope"
sides = [
  { duties = ["migration_author_plan"] },
  { duties = ["migration_approve_plan"] },
]
"#;

/// Answers [`AuthorshipCheck`] with `spot-sod`.
///
/// Built [`over`](SodAuthorship::over) the approval store, it reads the plan's author from the
/// persisted record on every question, so nothing is lost on a restart. Without a store it keeps
/// authors in a process-lifetime list (the legacy [`record_author`](SodAuthorship::record_author)
/// path). When the answer cannot be given (a poisoned lock, a store failure) the person is
/// treated as the author: the approval is refused.
pub struct SodAuthorship {
    rules: RuleSet,
    catalogue: RoleCatalogue,
    authors: Mutex<Vec<Assignment>>,
    store: Option<Arc<dyn ApprovalStore>>,
}

impl SodAuthorship {
    /// The standard rule: authoring and approving a plan conflict in that plan's scope.
    ///
    /// # Errors
    /// [`ApprovalError::BadPolicy`] if the built-in rule does not load (a defect in this crate).
    pub fn standard() -> Result<Self, ApprovalError> {
        let rules = RuleSet::from_toml(RULES).map_err(|e| ApprovalError::BadPolicy {
            message: format!("the separation-of-duties rule does not load: {e}"),
        })?;
        Ok(Self::with_rules(rules))
    }

    /// A check over `rules`. A rule set that does not conflict [`AUTHOR_DUTY`] with
    /// [`APPROVE_DUTY`] in the same scope makes the check allow everything, so prefer
    /// [`Self::standard`].
    #[must_use]
    pub fn with_rules(rules: RuleSet) -> Self {
        Self {
            rules,
            catalogue: RoleCatalogue::default(),
            authors: Mutex::new(Vec::new()),
            store: None,
        }
    }

    /// Reads authors from the persisted plan records of `store` (the store the approval service
    /// uses), instead of a list that lives only as long as the process.
    #[must_use]
    pub fn over(mut self, store: Arc<dyn ApprovalStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Records that `who` authored the plan with this digest.
    ///
    /// Deprecated: register the plan with its author when it is created
    /// (`ApprovalService::register_plan`), which persists it. With a store attached this calls the
    /// store's `register_plan`; without one the author is kept in memory only.
    ///
    /// # Errors
    /// [`ApprovalError::Backend`] when the record lock is poisoned or the store fails, and
    /// [`ApprovalError::AuthorConflict`] when the plan has another author.
    #[deprecated(note = "use ApprovalService::register_plan at plan creation")]
    pub fn record_author(&self, plan: &PlanDigest, who: &str) -> Result<(), ApprovalError> {
        if let Some(store) = &self.store {
            return store.register_plan(plan, who);
        }
        let mut authors = self.authors.lock().map_err(|_| ApprovalError::Backend {
            message: "the authorship record lock was poisoned".to_owned(),
        })?;
        authors.push(Assignment::duty(who, AUTHOR_DUTY, Some(plan.as_hex())));
        Ok(())
    }
}

impl AuthorshipCheck for SodAuthorship {
    fn is_author(&self, plan: &PlanDigest, who: &str) -> bool {
        let Ok(mut authors) = self.authors.lock().map(|list| list.clone()) else {
            return true;
        };
        if let Some(store) = &self.store {
            match store.author_of(plan) {
                Ok(Some(author)) => {
                    authors.push(Assignment::duty(&author, AUTHOR_DUTY, Some(plan.as_hex())));
                }
                // No recorded author: the service refuses the approval itself; nobody is the
                // author as far as this check can tell.
                Ok(None) => {}
                // The record cannot be read: treat the person as the author.
                Err(_) => return true,
            }
        }
        let proposed = Assignment::duty(who, APPROVE_DUTY, Some(plan.as_hex()));
        !matches!(
            check_grant(&self.rules, &self.catalogue, &authors, &proposed, &[]),
            GrantCheck::Clear
        )
    }
}
