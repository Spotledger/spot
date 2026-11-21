//! [`MemoryCampaignStore`]: the in-process [`CampaignStore`], the fake the contract suite and
//! the host's tests run against. It follows the same planning functions as the PostgreSQL
//! backend, so the two cannot disagree about a rule.

use std::collections::BTreeMap;
use std::sync::Mutex;

use spot_tenancy::TenantId;

use crate::campaign::Campaign;
use crate::event::Outcome;
use crate::model::{CampaignSpec, Decision, ReviewItem};
use crate::store::{
    CampaignStore, CampaignSummary, HistoryEntry, HistoryRecord, StoreError, StoreFuture,
    StoredCampaign, check_id, check_limit, check_open, entries_of, plan_decide, plan_tick,
    refusal_entry,
};

#[derive(Default)]
struct Entry {
    stored: Option<StoredCampaign>,
    history: Vec<HistoryRecord>,
}

#[derive(Default)]
struct Inner {
    campaigns: BTreeMap<(TenantId, String), Entry>,
    next_seq: u64,
}

impl Inner {
    fn append(&mut self, key: &(TenantId, String), entries: Vec<HistoryEntry>, at: i64) {
        for entry in entries {
            self.next_seq += 1;
            let seq = self.next_seq;
            if let Some(slot) = self.campaigns.get_mut(key) {
                slot.history.push(HistoryRecord { seq, at, entry });
            }
        }
    }

    fn stored(&self, key: &(TenantId, String)) -> Result<StoredCampaign, StoreError> {
        self.campaigns
            .get(key)
            .and_then(|e| e.stored.clone())
            .ok_or(StoreError::NotFound)
    }
}

/// An in-memory [`CampaignStore`]. State is lost with the value.
#[derive(Default)]
pub struct MemoryCampaignStore {
    inner: Mutex<Inner>,
}

impl MemoryCampaignStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, StoreError> {
        self.inner.lock().map_err(|_| StoreError::Unavailable)
    }
}

impl std::fmt::Debug for MemoryCampaignStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryCampaignStore")
            .finish_non_exhaustive()
    }
}

impl CampaignStore for MemoryCampaignStore {
    fn open<'a>(
        &'a self,
        tenant: &'a TenantId,
        spec: CampaignSpec,
        items: Vec<ReviewItem>,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>> {
        Box::pin(async move {
            check_open(&spec, &items)?;
            let campaign = Campaign::open(spec, items, now)?;
            let key = (*tenant, campaign.spec().id.clone());
            let mut inner = self.lock()?;
            if inner.campaigns.contains_key(&key) {
                return Err(StoreError::AlreadyExists);
            }
            let outcome = Outcome {
                events: campaign.opening_events().to_vec(),
                actions: Vec::new(),
            };
            let versions = vec![0; campaign.items().len()];
            inner.campaigns.insert(
                key.clone(),
                Entry {
                    stored: Some(StoredCampaign {
                        campaign,
                        item_versions: versions,
                    }),
                    history: Vec::new(),
                },
            );
            inner.append(&key, entries_of(&outcome), now);
            Ok(outcome)
        })
    }

    fn load<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
    ) -> StoreFuture<'a, Result<StoredCampaign, StoreError>> {
        Box::pin(async move {
            check_id(campaign)?;
            self.lock()?.stored(&(*tenant, campaign.to_string()))
        })
    }

    fn decide<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        item: &'a str,
        by: &'a str,
        decision: Decision,
        expected_version: u64,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>> {
        Box::pin(async move {
            check_id(campaign)?;
            let key = (*tenant, campaign.to_string());
            let mut inner = self.lock()?;
            let stored = inner.stored(&key)?;
            match plan_decide(&stored, item, by, decision, expected_version, now) {
                Ok(t) => {
                    if let Some(slot) = inner.campaigns.get_mut(&key) {
                        slot.stored = Some(StoredCampaign {
                            campaign: t.campaign,
                            item_versions: t.versions,
                        });
                    }
                    inner.append(&key, t.entries, now);
                    Ok(t.outcome)
                }
                Err(e) => {
                    if let Some(entry) = refusal_entry(&e, item, by) {
                        inner.append(&key, vec![entry], now);
                    }
                    Err(e)
                }
            }
        })
    }

    fn tick<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>> {
        Box::pin(async move {
            check_id(campaign)?;
            let key = (*tenant, campaign.to_string());
            let mut inner = self.lock()?;
            let stored = inner.stored(&key)?;
            let t = plan_tick(&stored, now);
            if let Some(slot) = inner.campaigns.get_mut(&key) {
                slot.stored = Some(StoredCampaign {
                    campaign: t.campaign,
                    item_versions: t.versions,
                });
            }
            inner.append(&key, t.entries, now);
            Ok(t.outcome)
        })
    }

    fn list<'a>(
        &'a self,
        tenant: &'a TenantId,
        after: Option<&'a str>,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<CampaignSummary>, StoreError>> {
        Box::pin(async move {
            check_limit(limit)?;
            if let Some(after) = after {
                check_id(after)?;
            }
            let inner = self.lock()?;
            let take = usize::try_from(limit).unwrap_or(usize::MAX);
            Ok(inner
                .campaigns
                .iter()
                .filter(|((t, id), _)| t == tenant && after.is_none_or(|a| id.as_str() > a))
                .filter_map(|(_, e)| e.stored.as_ref())
                .take(take)
                .map(|s| CampaignSummary {
                    id: s.campaign.spec().id.clone(),
                    title: s.campaign.spec().title.clone(),
                    status: s.campaign.status(),
                    deadline: s.campaign.spec().deadline,
                    items: s.campaign.items().len(),
                })
                .collect())
        })
    }

    fn history<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        after_seq: u64,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<HistoryRecord>, StoreError>> {
        Box::pin(async move {
            check_limit(limit)?;
            check_id(campaign)?;
            let inner = self.lock()?;
            let entry = inner
                .campaigns
                .get(&(*tenant, campaign.to_string()))
                .ok_or(StoreError::NotFound)?;
            let take = usize::try_from(limit).unwrap_or(usize::MAX);
            Ok(entry
                .history
                .iter()
                .filter(|r| r.seq > after_seq)
                .take(take)
                .cloned()
                .collect())
        })
    }
}
