//! [`MemorySodStore`]: the in-process [`SodStore`], the fake the contract suite runs against.
//! It uses the same checks as the PostgreSQL backend.

use std::collections::BTreeMap;
use std::sync::Mutex;

use spot_tenancy::TenantId;

use crate::override_sod::SodOverride;
use crate::store::{
    Revocation, SodStore, SodStoreError, StoreFuture, StoredOverride, StoredRuleSet, check_limit,
    check_override, check_revocation, check_rule_set,
};

#[derive(Default)]
struct Tenant {
    /// (toml, by, at), version = index + 1. Never edited, only appended.
    sets: Vec<(String, String, i64)>,
    overrides: Vec<StoredOverride>,
}

/// An in-memory [`SodStore`]. State is lost with the value.
#[derive(Default)]
pub struct MemorySodStore {
    inner: Mutex<BTreeMap<TenantId, Tenant>>,
}

impl MemorySodStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<TenantId, Tenant>>, SodStoreError> {
        self.inner.lock().map_err(|_| SodStoreError::Unavailable)
    }
}

impl std::fmt::Debug for MemorySodStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySodStore").finish_non_exhaustive()
    }
}

fn current(t: &Tenant) -> Result<Option<StoredRuleSet>, SodStoreError> {
    let Some((toml, by, at)) = t.sets.last() else {
        return Ok(None);
    };
    let rules = crate::model::RuleSet::from_toml(toml).map_err(|_| SodStoreError::Corrupt)?;
    Ok(Some(StoredRuleSet {
        version: t.sets.len() as u64,
        rules,
        set_by: by.clone(),
        set_at: *at,
    }))
}

impl SodStore for MemorySodStore {
    fn put_rule_set<'a>(
        &'a self,
        tenant: &'a TenantId,
        toml: &'a str,
        expected_version: u64,
        by: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<u64, SodStoreError>> {
        Box::pin(async move {
            check_rule_set(toml, by)?;
            let mut inner = self.lock()?;
            let t = inner.entry(*tenant).or_default();
            if t.sets.len() as u64 != expected_version {
                return Err(SodStoreError::VersionConflict);
            }
            t.sets.push((toml.to_string(), by.to_string(), now));
            Ok(t.sets.len() as u64)
        })
    }

    fn rule_set<'a>(
        &'a self,
        tenant: &'a TenantId,
    ) -> StoreFuture<'a, Result<Option<StoredRuleSet>, SodStoreError>> {
        Box::pin(async move { self.lock()?.get(tenant).map_or(Ok(None), current) })
    }

    fn grant_override<'a>(
        &'a self,
        tenant: &'a TenantId,
        record: SodOverride,
        expires_at: i64,
        now: i64,
    ) -> StoreFuture<'a, Result<u64, SodStoreError>> {
        Box::pin(async move {
            check_override(&record, expires_at, now)?;
            let mut inner = self.lock()?;
            let t = inner.entry(*tenant).or_default();
            let rules =
                current(t)?.ok_or(SodStoreError::InputInvalid("the tenant has no rule set"))?;
            if rules.rules.rule(&record.rule_id).is_none() {
                return Err(SodStoreError::InputInvalid("rule id names no rule"));
            }
            if t.overrides.iter().any(|o| {
                o.is_active(now)
                    && o.record
                        .covers(&record.rule_id, &record.user, record.scope.as_deref())
            }) {
                return Err(SodStoreError::AlreadyActive);
            }
            let seq = t.overrides.len() as u64 + 1;
            t.overrides.push(StoredOverride {
                seq,
                record,
                expires_at,
                revoked: None,
            });
            Ok(seq)
        })
    }

    fn revoke_override<'a>(
        &'a self,
        tenant: &'a TenantId,
        seq: u64,
        by: &'a str,
        reason: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<(), SodStoreError>> {
        Box::pin(async move {
            check_revocation(by, reason)?;
            let mut inner = self.lock()?;
            let o = inner
                .get_mut(tenant)
                .and_then(|t| t.overrides.iter_mut().find(|o| o.seq == seq))
                .ok_or(SodStoreError::NotFound)?;
            if o.revoked.is_some() {
                return Err(SodStoreError::AlreadyRevoked);
            }
            o.revoked = Some(Revocation {
                by: by.to_string(),
                reason: reason.to_string(),
                at: now,
            });
            Ok(())
        })
    }

    fn active_overrides<'a>(
        &'a self,
        tenant: &'a TenantId,
        now: i64,
    ) -> StoreFuture<'a, Result<Vec<StoredOverride>, SodStoreError>> {
        Box::pin(async move {
            Ok(self.lock()?.get(tenant).map_or_else(Vec::new, |t| {
                t.overrides
                    .iter()
                    .filter(|o| o.is_active(now))
                    .cloned()
                    .collect()
            }))
        })
    }

    fn list_overrides<'a>(
        &'a self,
        tenant: &'a TenantId,
        after_seq: u64,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<StoredOverride>, SodStoreError>> {
        Box::pin(async move {
            check_limit(limit)?;
            let take = usize::try_from(limit).unwrap_or(usize::MAX);
            Ok(self.lock()?.get(tenant).map_or_else(Vec::new, |t| {
                t.overrides
                    .iter()
                    .filter(|o| o.seq > after_seq)
                    .take(take)
                    .cloned()
                    .collect()
            }))
        })
    }
}
