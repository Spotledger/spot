//! The persistence port of the SoD rule set and its overrides (WP-SEC-02.T3).
//!
//! The rule set is versioned and append-only: a new version is written against the version
//! the caller read (compare-and-set), the stored text is the validated TOML, and a load
//! validates it again through [`RuleSet::from_toml`] and refuses a set that no longer
//! validates (fail closed: no rules is never the answer to a broken rule set).
//!
//! An override is audit evidence: recorded once, never edited. It covers one (rule, user,
//! scope) until its expiry; revoking appends a revocation record and leaves the override as
//! it was. At most one override per (rule, user, scope) is active at a time. Nobody grants
//! an override on their own access.
//!
//! Every method is scoped by the caller's [`TenantId`]; another tenant's record answers like
//! a missing one. Time is an argument; errors are fixed sentences without values.

use std::future::Future;
use std::pin::Pin;

use spot_tenancy::TenantId;

use crate::error::SodError;
use crate::model::RuleSet;
use crate::override_sod::SodOverride;

/// A boxed, sendable future (the store needs no async runtime in this crate).
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The longest rule id, user, scope or actor, in bytes.
pub const MAX_ID_BYTES: usize = 128;
/// The longest reason or mitigating control, in bytes.
pub const MAX_TEXT_BYTES: usize = 4096;
/// The longest rule-set document, in bytes.
pub const MAX_RULESET_BYTES: usize = 1_048_576;
/// The most rows one page may hold.
pub const MAX_PAGE: u32 = 200;

/// Why a store call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SodStoreError {
    /// The rule-set text does not validate (carries its `sod.*` code).
    #[error(transparent)]
    Rules(#[from] SodError),
    /// No such record in this tenant (also what another tenant's record answers).
    #[error("no such record")]
    NotFound,
    /// The rule set changed since the caller read it.
    #[error("the rule set changed since it was read; reload it and retry")]
    VersionConflict,
    /// An active override already covers this rule, user and scope.
    #[error("an active override already covers this rule, user and scope")]
    AlreadyActive,
    /// The override was already revoked.
    #[error("the override was already revoked")]
    AlreadyRevoked,
    /// An input is out of bounds or incomplete; the text names the field class.
    #[error("input refused: {0}")]
    InputInvalid(&'static str),
    /// The stored rule set no longer validates and was refused.
    #[error("the stored rule set no longer validates and was refused")]
    Corrupt,
    /// The storage could not answer; nothing was changed.
    #[error("the SoD store is unavailable")]
    Unavailable,
}

/// The stable code of [`SodStoreError::NotFound`].
pub const NOT_FOUND: &str = "access_review_pg.sod_not_found";
/// The stable code of [`SodStoreError::VersionConflict`].
pub const VERSION_CONFLICT: &str = "access_review_pg.sod_version_conflict";
/// The stable code of [`SodStoreError::AlreadyActive`].
pub const ALREADY_ACTIVE: &str = "access_review_pg.sod_override_active";
/// The stable code of [`SodStoreError::AlreadyRevoked`].
pub const ALREADY_REVOKED: &str = "access_review_pg.sod_override_revoked";
/// The stable code of [`SodStoreError::InputInvalid`].
pub const INPUT_INVALID: &str = "access_review_pg.sod_input_invalid";
/// The stable code of [`SodStoreError::Corrupt`].
pub const CORRUPT: &str = "access_review_pg.sod_corrupt_rules";
/// The stable code of [`SodStoreError::Unavailable`].
pub const UNAVAILABLE: &str = "access_review_pg.sod_unavailable";

impl SodStoreError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Rules(e) => e.code(),
            Self::NotFound => NOT_FOUND,
            Self::VersionConflict => VERSION_CONFLICT,
            Self::AlreadyActive => ALREADY_ACTIVE,
            Self::AlreadyRevoked => ALREADY_REVOKED,
            Self::InputInvalid(_) => INPUT_INVALID,
            Self::Corrupt => CORRUPT,
            Self::Unavailable => UNAVAILABLE,
        }
    }
}

/// The current rule set with its version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRuleSet {
    /// The version: 1 for the first set, plus one for every replacement.
    pub version: u64,
    /// The validated rules.
    pub rules: RuleSet,
    /// Who stored this version.
    pub set_by: String,
    /// When, epoch seconds.
    pub set_at: i64,
}

/// A revocation of an override.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revocation {
    /// Who revoked.
    pub by: String,
    /// Why.
    pub reason: String,
    /// When, epoch seconds.
    pub at: i64,
}

/// An override as stored: the record, its expiry and, if any, its revocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredOverride {
    /// The position: strictly increasing within a tenant, never reused.
    pub seq: u64,
    /// The override exactly as granted.
    pub record: SodOverride,
    /// When it stops covering anything, epoch seconds.
    pub expires_at: i64,
    /// The revocation, when there is one.
    pub revoked: Option<Revocation>,
}

impl StoredOverride {
    /// Whether the override covers anything at `now`: not expired and not revoked.
    #[must_use]
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked.is_none() && now < self.expires_at
    }
}

/// The persistence port of the rule set and the overrides.
pub trait SodStore: Send + Sync {
    /// Stores a new version of the tenant's rule set, if `expected_version` is the current
    /// one (0 when none was stored). The text is validated first.
    ///
    /// # Errors
    /// [`SodStoreError::Rules`] for text that does not validate,
    /// [`SodStoreError::VersionConflict`], [`SodStoreError::InputInvalid`].
    fn put_rule_set<'a>(
        &'a self,
        tenant: &'a TenantId,
        toml: &'a str,
        expected_version: u64,
        by: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<u64, SodStoreError>>;

    /// The current rule set, validated again on the way out; `None` when none was stored.
    ///
    /// # Errors
    /// [`SodStoreError::Corrupt`] when the stored text no longer validates.
    fn rule_set<'a>(
        &'a self,
        tenant: &'a TenantId,
    ) -> StoreFuture<'a, Result<Option<StoredRuleSet>, SodStoreError>>;

    /// Records an override that expires at `expires_at`. The rule must exist in the current
    /// rule set; reason, mitigating control and grantor must be non-blank; the grantor must
    /// not be the user; no active override may cover the same rule, user and scope.
    ///
    /// # Errors
    /// [`SodStoreError::InputInvalid`], [`SodStoreError::AlreadyActive`].
    fn grant_override<'a>(
        &'a self,
        tenant: &'a TenantId,
        record: SodOverride,
        expires_at: i64,
        now: i64,
    ) -> StoreFuture<'a, Result<u64, SodStoreError>>;

    /// Revokes an override by appending a revocation; the override itself is untouched.
    ///
    /// # Errors
    /// [`SodStoreError::NotFound`], [`SodStoreError::AlreadyRevoked`],
    /// [`SodStoreError::InputInvalid`].
    fn revoke_override<'a>(
        &'a self,
        tenant: &'a TenantId,
        seq: u64,
        by: &'a str,
        reason: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<(), SodStoreError>>;

    /// The overrides active at `now`, in position order.
    ///
    /// # Errors
    /// [`SodStoreError::Unavailable`].
    fn active_overrides<'a>(
        &'a self,
        tenant: &'a TenantId,
        now: i64,
    ) -> StoreFuture<'a, Result<Vec<StoredOverride>, SodStoreError>>;

    /// Every override, active or not, after position `after_seq`, oldest first: the audit
    /// view.
    ///
    /// # Errors
    /// [`SodStoreError::InputInvalid`] for a bad limit.
    fn list_overrides<'a>(
        &'a self,
        tenant: &'a TenantId,
        after_seq: u64,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<StoredOverride>, SodStoreError>>;
}

fn bounded(text: &str, max: usize, what: &'static str) -> Result<(), SodStoreError> {
    if text.len() > max {
        Err(SodStoreError::InputInvalid(what))
    } else {
        Ok(())
    }
}

fn present(text: &str, what: &'static str) -> Result<(), SodStoreError> {
    if text.trim().is_empty() {
        Err(SodStoreError::InputInvalid(what))
    } else {
        Ok(())
    }
}

/// Checks a page size: 1 to [`MAX_PAGE`].
///
/// # Errors
/// [`SodStoreError::InputInvalid`] outside that range.
pub fn check_limit(limit: u32) -> Result<(), SodStoreError> {
    if limit == 0 || limit > MAX_PAGE {
        Err(SodStoreError::InputInvalid("page size"))
    } else {
        Ok(())
    }
}

/// Validates a rule-set document: size bound, then the loader. Every backend calls this on
/// the way in and on the way out.
///
/// # Errors
/// [`SodStoreError::InputInvalid`] for an oversize text, [`SodStoreError::Rules`].
pub fn check_rule_set(toml: &str, actor: &str) -> Result<RuleSet, SodStoreError> {
    bounded(toml, MAX_RULESET_BYTES, "rule set size")?;
    present(actor, "actor")?;
    bounded(actor, MAX_ID_BYTES, "actor")?;
    Ok(RuleSet::from_toml(toml)?)
}

/// Checks everything about an override that needs no storage. A blank reason or control would
/// make `apply_override` refuse it later; it is refused here instead, so an unusable record is
/// never evidence.
///
/// # Errors
/// [`SodStoreError::InputInvalid`] naming the field class.
pub fn check_override(
    record: &SodOverride,
    expires_at: i64,
    now: i64,
) -> Result<(), SodStoreError> {
    present(&record.rule_id, "rule id")?;
    present(&record.user, "user")?;
    present(&record.reason, "reason")?;
    present(&record.mitigating_control, "mitigating control")?;
    present(&record.granted_by, "grantor")?;
    bounded(&record.rule_id, MAX_ID_BYTES, "rule id")?;
    bounded(&record.user, MAX_ID_BYTES, "user")?;
    bounded(&record.granted_by, MAX_ID_BYTES, "grantor")?;
    if let Some(scope) = &record.scope {
        present(scope, "scope")?;
        bounded(scope, MAX_ID_BYTES, "scope")?;
    }
    bounded(&record.reason, MAX_TEXT_BYTES, "reason")?;
    bounded(
        &record.mitigating_control,
        MAX_TEXT_BYTES,
        "mitigating control",
    )?;
    if record.granted_by == record.user {
        return Err(SodStoreError::InputInvalid(
            "nobody grants an override on their own access",
        ));
    }
    if expires_at <= now || expires_at <= record.granted_at {
        return Err(SodStoreError::InputInvalid("expiry must be in the future"));
    }
    Ok(())
}

/// Checks a revocation's text.
///
/// # Errors
/// [`SodStoreError::InputInvalid`].
pub fn check_revocation(by: &str, reason: &str) -> Result<(), SodStoreError> {
    present(by, "actor")?;
    present(reason, "reason")?;
    bounded(by, MAX_ID_BYTES, "actor")?;
    bounded(reason, MAX_TEXT_BYTES, "reason")
}
