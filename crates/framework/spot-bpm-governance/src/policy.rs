//! The policy data, its effective-dated change log, and the store the guard reads it through.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;

use crate::error::GovernanceError;
use crate::mode::GovernanceMode;

/// One record type's governance mode in one company.
///
/// It is the value the guard decides on: a company with no policy at all is [`GovernanceMode::Free`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernancePolicy {
    /// The company (an organisation unit scope, ADR-080 section 2.1).
    pub company: String,
    /// The record type (a DocType's storage name).
    pub doctype: String,
    /// How the record type is entered.
    pub mode: GovernanceMode,
}

impl GovernancePolicy {
    /// A policy for `company` and `doctype`.
    #[must_use]
    pub fn new(company: &str, doctype: &str, mode: GovernanceMode) -> Self {
        Self {
            company: company.to_owned(),
            doctype: doctype.to_owned(),
            mode,
        }
    }

    /// The default policy: free, governance off.
    #[must_use]
    pub fn free(company: &str, doctype: &str) -> Self {
        Self::new(company, doctype, GovernanceMode::Free)
    }
}

/// One recorded change of a record type's mode, and the instant it takes effect.
///
/// A change applies to records **created at or after** [`PolicyChange::effective_at`]; records
/// created before it keep the mode that was in force then. The change is itself audited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyChange {
    /// The company the change is for.
    pub company: String,
    /// The record type the change is for.
    pub doctype: String,
    /// The mode before the change.
    pub from: GovernanceMode,
    /// The mode the change sets.
    pub to: GovernanceMode,
    /// When the new mode takes effect.
    pub effective_at: OffsetDateTime,
    /// Who made the change.
    pub changed_by: String,
    /// When the change was recorded.
    pub changed_at: OffsetDateTime,
}

impl PolicyChange {
    /// A change from `from` to `to`, effective at `effective_at`.
    #[must_use]
    pub fn new(
        company: &str,
        doctype: &str,
        from: GovernanceMode,
        to: GovernanceMode,
        effective_at: OffsetDateTime,
        changed_by: &str,
        changed_at: OffsetDateTime,
    ) -> Self {
        Self {
            company: company.to_owned(),
            doctype: doctype.to_owned(),
            from,
            to,
            effective_at,
            changed_by: changed_by.to_owned(),
            changed_at,
        }
    }
}

/// Where the guard reads the policy, the reason floor and the change log from.
///
/// The production implementation ([`crate::settings::SettingsPolicy`]) reads the mode and the
/// floor through the `spot-settings` API, never from a store directly, and keeps the
/// effective-dated change log. Tests drive [`MemoryPolicyStore`].
pub trait PolicyStore: Send + Sync {
    /// The mode in force for `company`/`doctype` at instant `at`.
    ///
    /// # Errors
    /// [`GovernanceError::Settings`] when a setting cannot be read, or
    /// [`GovernanceError::PolicyInvalid`] when a stored value is malformed.
    fn mode_at(
        &self,
        company: &str,
        doctype: &str,
        at: OffsetDateTime,
    ) -> Result<GovernanceMode, GovernanceError>;

    /// The smallest accepted outside-process reason, in characters.
    ///
    /// # Errors
    /// [`GovernanceError::Settings`] when the setting cannot be read.
    fn min_reason_chars(&self, company: &str) -> Result<usize, GovernanceError>;

    /// The current policy for `company`/`doctype` (the settings value, before the change log).
    ///
    /// # Errors
    /// As [`PolicyStore::mode_at`].
    fn current(&self, company: &str, doctype: &str) -> Result<GovernancePolicy, GovernanceError>;

    /// Every recorded change for `company`/`doctype`, in recorded order.
    ///
    /// # Errors
    /// [`GovernanceError::Settings`] when the log cannot be read.
    fn changes(&self, company: &str, doctype: &str) -> Result<Vec<PolicyChange>, GovernanceError>;

    /// Records a change; the caller audits it (see
    /// [`GovernanceGuard::change_policy`](crate::GovernanceGuard::change_policy)).
    ///
    /// # Errors
    /// [`GovernanceError::Settings`] when the log cannot be written.
    fn record_change(&self, change: PolicyChange) -> Result<(), GovernanceError>;
}

/// The effective mode of a change log at one instant: the latest change whose effective instant
/// is at or before `at`, or `fallback` when there is none.
#[must_use]
pub fn effective_mode(
    changes: &[PolicyChange],
    at: OffsetDateTime,
    fallback: GovernanceMode,
) -> GovernanceMode {
    changes
        .iter()
        .filter(|change| change.effective_at <= at)
        .max_by_key(|change| change.effective_at)
        .map_or(fallback, |change| change.to)
}

/// An in-memory policy store: a reference for tests and for a host with no settings yet.
///
/// It holds the current mode per (company, doctype), the reason floor per company, and the
/// effective-dated change log, so the same effective-dating rule as the settings-backed store
/// can be exercised without a settings registry.
#[derive(Debug, Default)]
pub struct MemoryPolicyStore {
    current: RwLock<BTreeMap<(String, String), GovernanceMode>>,
    min_reason: RwLock<BTreeMap<String, usize>>,
    changes: RwLock<Vec<PolicyChange>>,
    default_min_reason: usize,
}

impl MemoryPolicyStore {
    /// A store with no policies (everything free) and the given reason floor.
    #[must_use]
    pub fn new(default_min_reason: usize) -> Self {
        Self {
            default_min_reason,
            ..Self::default()
        }
    }

    /// Sets the current mode for (company, doctype).
    pub fn set_mode(&self, company: &str, doctype: &str, mode: GovernanceMode) {
        self.write(&self.current)
            .insert((company.to_owned(), doctype.to_owned()), mode);
    }

    /// Sets the reason floor for `company`.
    pub fn set_min_reason_chars(&self, company: &str, min: usize) {
        self.write(&self.min_reason).insert(company.to_owned(), min);
    }

    /// Wraps `self` in an `Arc`.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn write<'a, T>(&self, lock: &'a RwLock<T>) -> std::sync::RwLockWriteGuard<'a, T> {
        lock.write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn read<'a, T>(&self, lock: &'a RwLock<T>) -> std::sync::RwLockReadGuard<'a, T> {
        lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl PolicyStore for MemoryPolicyStore {
    fn mode_at(
        &self,
        company: &str,
        doctype: &str,
        at: OffsetDateTime,
    ) -> Result<GovernanceMode, GovernanceError> {
        let fallback = self.current(company, doctype)?.mode;
        let changes = self.read(&self.changes);
        let mine: Vec<PolicyChange> = changes
            .iter()
            .filter(|c| c.company == company && c.doctype == doctype)
            .cloned()
            .collect();
        Ok(effective_mode(&mine, at, fallback))
    }

    fn min_reason_chars(&self, company: &str) -> Result<usize, GovernanceError> {
        Ok(self
            .read(&self.min_reason)
            .get(company)
            .copied()
            .unwrap_or(self.default_min_reason))
    }

    fn current(&self, company: &str, doctype: &str) -> Result<GovernancePolicy, GovernanceError> {
        let mode = self
            .read(&self.current)
            .get(&(company.to_owned(), doctype.to_owned()))
            .copied()
            .unwrap_or(GovernanceMode::Free);
        Ok(GovernancePolicy::new(company, doctype, mode))
    }

    fn changes(&self, company: &str, doctype: &str) -> Result<Vec<PolicyChange>, GovernanceError> {
        Ok(self
            .read(&self.changes)
            .iter()
            .filter(|c| c.company == company && c.doctype == doctype)
            .cloned()
            .collect())
    }

    fn record_change(&self, change: PolicyChange) -> Result<(), GovernanceError> {
        self.write(&self.changes).push(change);
        Ok(())
    }
}
