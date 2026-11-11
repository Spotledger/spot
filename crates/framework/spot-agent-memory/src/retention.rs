//! Retention: per-kind TTLs and size caps, deterministic compaction of old episodes, legal
//! holds, and tenant export and deletion.
//!
//! Two steps, as `spot-data-policy` does for records: [`plan_retention`] reads the store and
//! returns what it **would** do, with the reason; [`apply_retention`] performs exactly that plan.
//! The plan is pure, so an operator can read it, and applying it twice is harmless because every
//! action names one entry.
//!
//! # Compaction
//!
//! Old episodes are folded into **aggregate statistics**: per symptom and outcome, how many
//! episodes were folded and the earliest and latest instants. The fold is deterministic — integer
//! counts and instants, no model, no sampling, no clock — so the same episodes always give the
//! same figures. The aggregate is stored as a [`Fact`] whose source is
//! [`SourceKind::Compaction`], so nothing in the namespace pretends a summary is a live episode.
//!
//! # Legal holds
//!
//! A hold from [`spot_data_policy`] suspends **every** retention action for the entries it
//! covers, and they are reported with the hold that saved them. The same check guards tenant
//! deletion: a held entry survives its tenant's deletion and is reported.
//!
//! # Time
//!
//! Time is an argument everywhere; the plan carries the instant it was made for, and the report
//! names the epoch the aggregate facts were written at.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_data_policy::{Hold, Holds, Record};

use crate::embed::Embedder;
use crate::error::MemoryError;
use crate::fact::{EpisodeAggregate, Epoch, Fact};
use crate::id::{FactId, FactKey, TenantId};
use crate::meta::{Confidence, EntryMeta, MemoryKind, SourceKind};
use crate::store::{Entry, MemoryStore};

/// Milliseconds in one day, the unit a TTL is configured in.
pub const DAY_MS: i64 = 86_400_000;

/// The retention settings for one kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KindPolicy {
    /// How many days an entry of this kind lives; `None` keeps it until the cap removes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_days: Option<u32>,
    /// How many entries of this kind a tenant may keep.
    pub max_entries: usize,
}

impl KindPolicy {
    /// A policy with a TTL in days and a cap.
    #[must_use]
    pub fn new(ttl_days: Option<u32>, max_entries: usize) -> Self {
        Self {
            ttl_days,
            max_entries,
        }
    }

    /// The TTL in milliseconds, saturating rather than overflowing.
    #[must_use]
    pub fn ttl_ms(&self) -> Option<i64> {
        self.ttl_days
            .map(|days| i64::from(days).saturating_mul(DAY_MS))
    }

    /// Whether an entry created at `created_ms` has outlived this policy at `now_ms`.
    #[must_use]
    pub fn is_expired(&self, created_ms: i64, now_ms: i64) -> bool {
        self.ttl_ms()
            .is_some_and(|ttl| now_ms >= created_ms.saturating_add(ttl))
    }
}

impl Default for KindPolicy {
    fn default() -> Self {
        Self {
            ttl_days: None,
            max_entries: 10_000,
        }
    }
}

/// The whole retention policy: one setting per kind plus the compaction age.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetentionPolicy {
    /// How old an episode must be before the cap may fold it into statistics rather than drop it.
    /// Stays first so the TOML form writes it before the kind tables.
    pub compaction_min_age_days: u32,
    /// Episodes: short-lived, and the kind compaction folds.
    pub episode: KindPolicy,
    /// Procedures: kept until the cap removes them, because a learned fix stays useful.
    pub procedure: KindPolicy,
    /// Facts: short-lived, because the deployment model re-reads them at each epoch.
    pub fact: KindPolicy,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            compaction_min_age_days: 7,
            episode: KindPolicy::new(Some(90), 10_000),
            procedure: KindPolicy::new(None, 5_000),
            fact: KindPolicy::new(Some(30), 5_000),
        }
    }
}

impl RetentionPolicy {
    /// The policy for one kind.
    #[must_use]
    pub fn of(&self, kind: MemoryKind) -> &KindPolicy {
        match kind {
            MemoryKind::Episode => &self.episode,
            MemoryKind::Procedure => &self.procedure,
            MemoryKind::Fact => &self.fact,
        }
    }

    /// The compaction age in milliseconds.
    #[must_use]
    pub fn compaction_min_age_ms(&self) -> i64 {
        i64::from(self.compaction_min_age_days).saturating_mul(DAY_MS)
    }

    /// Loads a policy from TOML, refusing unknown keys.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Serialise`] when the text does not parse or carries a key the policy does
    /// not know.
    pub fn from_toml(text: &str) -> Result<Self, MemoryError> {
        toml::from_str(text).map_err(|error| MemoryError::Serialise {
            what: "retention policy".to_owned(),
            detail: error.to_string(),
        })
    }

    /// The policy in TOML form, for an operator to edit.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Serialise`] when the policy cannot be rendered.
    pub fn to_toml(&self) -> Result<String, MemoryError> {
        toml::to_string(self).map_err(|error| MemoryError::Serialise {
            what: "retention policy".to_owned(),
            detail: error.to_string(),
        })
    }
}

/// One entry, by kind and id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryRef {
    /// The kind.
    pub kind: MemoryKind,
    /// The id.
    pub id: String,
}

impl EntryRef {
    fn of(entry: &Entry) -> Self {
        Self {
            kind: entry.kind(),
            id: entry.id_str().to_owned(),
        }
    }
}

/// What the plan will do to one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetentionAction {
    /// Remove the entry: its TTL elapsed, or the cap leaves no room for it.
    Expire(EntryRef),
    /// Remove the episode and fold it into the aggregate statistics of its first symptom tag.
    Compact {
        /// The episode.
        entry: EntryRef,
        /// The figures it contributes.
        aggregate: EpisodeAggregate,
    },
    /// Leave the entry alone: a legal hold covers it.
    Held {
        /// The entry.
        entry: EntryRef,
        /// The hold that saved it.
        hold: String,
    },
}

/// What [`plan_retention`] would do, and why. It changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPlan {
    /// The tenant the plan was made for.
    pub tenant: TenantId,
    /// The instant the plan was made for, in Unix milliseconds.
    pub now_ms: i64,
    /// Every action, in kind order (episode, procedure, fact) and then id order.
    pub actions: Vec<RetentionAction>,
}

impl RetentionPlan {
    /// The entries the plan removes.
    #[must_use]
    pub fn expiries(&self) -> Vec<&EntryRef> {
        self.actions
            .iter()
            .filter_map(|action| match action {
                RetentionAction::Expire(entry) => Some(entry),
                _ => None,
            })
            .collect()
    }

    /// The episodes the plan folds into statistics.
    #[must_use]
    pub fn compactions(&self) -> Vec<&EntryRef> {
        self.actions
            .iter()
            .filter_map(|action| match action {
                RetentionAction::Compact { entry, .. } => Some(entry),
                _ => None,
            })
            .collect()
    }

    /// The entries a legal hold saves.
    #[must_use]
    pub fn holds(&self) -> Vec<(&EntryRef, &str)> {
        self.actions
            .iter()
            .filter_map(|action| match action {
                RetentionAction::Held { entry, hold } => Some((entry, hold.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Whether the plan changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// Reads `tenant`'s memory and returns what retention would do, without changing anything.
///
/// # Errors
///
/// [`MemoryError::Store`] when the store refuses a read.
pub fn plan_retention(
    store: &dyn MemoryStore,
    tenant: &TenantId,
    policy: &RetentionPolicy,
    now_ms: i64,
    holds: &Holds,
) -> Result<RetentionPlan, MemoryError> {
    let mut actions = Vec::new();
    for kind in MemoryKind::all() {
        let settings = policy.of(kind);
        let mut entries = store.list(tenant, kind);
        entries.sort_by(|left, right| {
            left.meta()
                .created_ms
                .cmp(&right.meta().created_ms)
                .then_with(|| left.id_str().cmp(right.id_str()))
        });
        let mut live: Vec<Entry> = Vec::new();
        for entry in entries {
            if let Some(hold) = covering_hold(holds, &entry) {
                actions.push(RetentionAction::Held {
                    entry: EntryRef::of(&entry),
                    hold: hold.id.clone(),
                });
                continue;
            }
            if settings.is_expired(entry.meta().created_ms, now_ms) {
                actions.push(RetentionAction::Expire(EntryRef::of(&entry)));
                continue;
            }
            live.push(entry);
        }
        if live.len() <= settings.max_entries {
            continue;
        }
        let overflow = live.len() - settings.max_entries;
        for entry in live.drain(..overflow) {
            let old_enough =
                now_ms.saturating_sub(entry.meta().created_ms) >= policy.compaction_min_age_ms();
            match &entry {
                Entry::Episode(episode) if old_enough => {
                    let Some(symptom) = episode.symptoms.first() else {
                        actions.push(RetentionAction::Expire(EntryRef::of(&entry)));
                        continue;
                    };
                    actions.push(RetentionAction::Compact {
                        entry: EntryRef::of(&entry),
                        aggregate: EpisodeAggregate::first(
                            symptom.clone(),
                            episode.outcome,
                            episode.meta.created_ms,
                        ),
                    });
                }
                _ => actions.push(RetentionAction::Expire(EntryRef::of(&entry))),
            }
        }
    }
    Ok(RetentionPlan {
        tenant: tenant.clone(),
        now_ms,
        actions,
    })
}

/// What [`apply_retention`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionReport {
    /// The entries removed.
    pub expired: Vec<EntryRef>,
    /// The episodes removed and folded into statistics.
    pub compacted: Vec<EntryRef>,
    /// The entries a legal hold saved, with the hold.
    pub held: Vec<(EntryRef, String)>,
    /// The ids of the aggregate facts written.
    pub facts: Vec<String>,
}

impl RetentionReport {
    /// Whether anything changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.expired.is_empty() && self.compacted.is_empty() && self.facts.is_empty()
    }
}

/// Applies `plan`: removals first, then the folded statistics written at `epoch`.
///
/// An aggregate that already exists is merged with the new figures, so applying a second
/// compaction over the same window adds nothing twice.
///
/// # Errors
///
/// [`MemoryError::Store`] when the store refuses a read or a write, and
/// [`MemoryError::Embed`] when the aggregate's vector cannot be embedded.
pub fn apply_retention(
    store: &dyn MemoryStore,
    embedder: &dyn Embedder,
    plan: &RetentionPlan,
    epoch: Epoch,
) -> Result<RetentionReport, MemoryError> {
    let mut report = RetentionReport {
        expired: Vec::new(),
        compacted: Vec::new(),
        held: Vec::new(),
        facts: Vec::new(),
    };
    let mut folded: BTreeMap<(String, String), EpisodeAggregate> = BTreeMap::new();
    for action in &plan.actions {
        match action {
            RetentionAction::Expire(entry) => {
                if store.remove(&plan.tenant, entry.kind, &entry.id)? {
                    report.expired.push(entry.clone());
                }
            }
            RetentionAction::Compact { entry, aggregate } => {
                if !store.remove(&plan.tenant, MemoryKind::Episode, &entry.id)? {
                    continue;
                }
                report.compacted.push(entry.clone());
                let key = (
                    aggregate.symptom.as_str().to_owned(),
                    aggregate.outcome.as_str().to_owned(),
                );
                folded
                    .entry(key)
                    .and_modify(|existing| *existing = existing.clone().merge(aggregate.clone()))
                    .or_insert_with(|| aggregate.clone());
            }
            RetentionAction::Held { entry, hold } => {
                report.held.push((entry.clone(), hold.clone()));
            }
        }
    }
    for ((symptom, outcome), aggregate) in folded {
        let slug = symptom.replace('.', "_");
        let id = FactId::parse(&format!("fact.episode.{slug}.{outcome}")).map_err(|error| {
            MemoryError::Serialise {
                what: "aggregate fact id".to_owned(),
                detail: error.to_string(),
            }
        })?;
        let key =
            FactKey::parse(&format!("episode.aggregate.{slug}.{outcome}")).map_err(|error| {
                MemoryError::Serialise {
                    what: "aggregate fact key".to_owned(),
                    detail: error.to_string(),
                }
            })?;
        let merged = match store.get(&plan.tenant, MemoryKind::Fact, id.as_str()) {
            Some(Entry::Fact(existing)) => match existing.value {
                crate::fact::FactValue::Stats(previous)
                    if previous.symptom.as_str() == symptom
                        && previous.outcome.as_str() == outcome =>
                {
                    previous.merge(aggregate)
                }
                _ => aggregate,
            },
            _ => aggregate,
        };
        let meta = EntryMeta::new(
            spot_agent_tiers::Tier::T0,
            plan.tenant.clone(),
            plan.now_ms,
            SourceKind::Compaction,
        )?
        .with_confidence(Confidence::FULL);
        let fact = Fact::aggregate(embedder, id, key, merged, epoch, meta)?;
        report.facts.push(fact.id_str().to_owned());
        store.put(Entry::Fact(fact))?;
    }
    Ok(report)
}

/// What a tenant's memory holds, as JSON-ready records for an export or a support bundle.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TenantExport {
    /// The tenant.
    pub tenant: String,
    /// Episodes, by id.
    pub episodes: Vec<Json>,
    /// Procedures, by id.
    pub procedures: Vec<Json>,
    /// Facts, by id.
    pub facts: Vec<Json>,
}

impl TenantExport {
    /// How many entries the export carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.episodes.len() + self.procedures.len() + self.facts.len()
    }

    /// Whether the export is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The export as JSON text.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Serialise`] when a record cannot be rendered.
    pub fn to_json(&self) -> Result<String, MemoryError> {
        serde_json::to_string_pretty(self).map_err(|error| MemoryError::Serialise {
            what: "tenant export".to_owned(),
            detail: error.to_string(),
        })
    }
}

/// Exports everything `tenant` holds, in id order within each kind.
///
/// # Errors
///
/// The record rendering error of [`Entry::to_json`].
pub fn export_tenant(
    store: &dyn MemoryStore,
    tenant: &TenantId,
) -> Result<TenantExport, MemoryError> {
    let render = |kind: MemoryKind| -> Result<Vec<Json>, MemoryError> {
        store
            .list(tenant, kind)
            .iter()
            .map(Entry::to_json)
            .collect::<Result<Vec<Json>, MemoryError>>()
    };
    Ok(TenantExport {
        tenant: tenant.as_str().to_owned(),
        episodes: render(MemoryKind::Episode)?,
        procedures: render(MemoryKind::Procedure)?,
        facts: render(MemoryKind::Fact)?,
    })
}

/// What [`delete_tenant`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionReport {
    /// The entries removed.
    pub deleted: Vec<EntryRef>,
    /// The entries a legal hold saved from deletion, with the hold.
    pub held: Vec<(EntryRef, String)>,
}

/// Deletes everything `tenant` holds, except the entries a legal hold covers.
///
/// # Errors
///
/// [`MemoryError::Store`] when the store refuses.
pub fn delete_tenant(
    store: &dyn MemoryStore,
    tenant: &TenantId,
    holds: &Holds,
) -> Result<DeletionReport, MemoryError> {
    let mut report = DeletionReport {
        deleted: Vec::new(),
        held: Vec::new(),
    };
    for kind in MemoryKind::all() {
        for entry in store.list(tenant, kind) {
            if let Some(hold) = covering_hold(holds, &entry) {
                report.held.push((EntryRef::of(&entry), hold.id.clone()));
                continue;
            }
            if store.remove(tenant, kind, entry.id_str())? {
                report.deleted.push(EntryRef::of(&entry));
            }
        }
    }
    Ok(report)
}

/// The first hold covering a memory entry, if one does.
///
/// A memory entry is presented to `spot-data-policy` as a record of its own DocType
/// (`agent_memory_episode`, `agent_memory_procedure`, `agent_memory_fact`) with its id and
/// creation instant; it carries no fields, because memory holds no record values.
fn covering_hold<'a>(holds: &'a Holds, entry: &Entry) -> Option<&'a Hold> {
    let record = Record {
        id: entry.id_str().to_owned(),
        doctype: entry.kind().hold_doctype().to_owned(),
        doc_class: None,
        jurisdiction: None,
        fields: BTreeMap::new(),
        created: u64::try_from(entry.meta().created_ms).unwrap_or(0),
        closed: None,
        events: BTreeMap::new(),
        references: Vec::new(),
    };
    holds.covering(&record)
}

#[cfg(test)]
mod tests {
    use super::{
        DAY_MS, RetentionPolicy, apply_retention, delete_tenant, export_tenant, plan_retention,
    };
    use crate::fact::{Epoch, FactValue};
    use crate::id::TenantId;
    use crate::meta::MemoryKind;
    use crate::store::{Entry, InMemoryStore, MemoryStore};
    use crate::testing::{embedder, episode_saying, fact, procedure};
    use spot_data_policy::Holds;

    fn tenant() -> TenantId {
        TenantId::parse("tenant_a").unwrap()
    }

    fn store_with_old_episodes() -> InMemoryStore {
        let store = InMemoryStore::new();
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                1,
                "stuck job one",
                DAY_MS,
            )))
            .unwrap();
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                2,
                "stuck job two",
                2 * DAY_MS,
            )))
            .unwrap();
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                3,
                "stuck job three",
                3 * DAY_MS,
            )))
            .unwrap();
        store
    }

    #[test]
    fn an_expired_episode_is_planned_for_expiry_and_removed() {
        let store = store_with_old_episodes();
        let policy = RetentionPolicy::default();
        let plan =
            plan_retention(&store, &tenant(), &policy, 100 * DAY_MS, &Holds::empty()).unwrap();
        assert_eq!(
            plan.expiries().len(),
            3,
            "all three are past the 90-day TTL"
        );
        let report = apply_retention(&store, &embedder(), &plan, Epoch::new(1)).unwrap();
        assert_eq!(report.expired.len(), 3);
        assert_eq!(store.count(&tenant(), MemoryKind::Episode), 0);
    }

    #[test]
    fn the_cap_folds_the_oldest_episodes_into_aggregate_statistics() {
        let store = store_with_old_episodes();
        let policy = RetentionPolicy {
            episode: super::KindPolicy::new(None, 1),
            compaction_min_age_days: 0,
            ..RetentionPolicy::default()
        };
        let plan = plan_retention(&store, &tenant(), &policy, 4 * DAY_MS, &Holds::empty()).unwrap();
        assert_eq!(plan.compactions().len(), 2, "the two oldest are folded");
        let report = apply_retention(&store, &embedder(), &plan, Epoch::new(3)).unwrap();
        assert_eq!(report.compacted.len(), 2);
        assert_eq!(
            report.facts.len(),
            1,
            "one aggregate for the symptom and outcome"
        );

        let facts = store.list(&tenant(), MemoryKind::Fact);
        assert_eq!(facts.len(), 1);
        match &facts[0] {
            Entry::Fact(fact) => {
                assert_eq!(fact.epoch, Epoch::new(3));
                assert_eq!(fact.meta.source, crate::meta::SourceKind::Compaction);
                match &fact.value {
                    FactValue::Stats(aggregate) => {
                        assert_eq!(aggregate.count, 2);
                        assert_eq!(aggregate.first_ms, DAY_MS);
                        assert_eq!(aggregate.last_ms, 2 * DAY_MS);
                    }
                    _ => panic!("compaction writes statistics"),
                }
            }
            _ => panic!("compaction writes a fact"),
        }
        assert_eq!(store.count(&tenant(), MemoryKind::Episode), 1);
    }

    #[test]
    fn a_second_compaction_merges_rather_than_double_counting() {
        let store = store_with_old_episodes();
        let policy = RetentionPolicy {
            episode: super::KindPolicy::new(None, 1),
            compaction_min_age_days: 0,
            ..RetentionPolicy::default()
        };
        let plan = plan_retention(&store, &tenant(), &policy, 4 * DAY_MS, &Holds::empty()).unwrap();
        apply_retention(&store, &embedder(), &plan, Epoch::new(3)).unwrap();
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                4,
                "stuck job four",
                5 * DAY_MS,
            )))
            .unwrap();
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                5,
                "stuck job five",
                6 * DAY_MS,
            )))
            .unwrap();
        let plan = plan_retention(&store, &tenant(), &policy, 7 * DAY_MS, &Holds::empty()).unwrap();
        let report = apply_retention(&store, &embedder(), &plan, Epoch::new(4)).unwrap();
        assert_eq!(report.facts.len(), 1);
        let facts = store.list(&tenant(), MemoryKind::Fact);
        match &facts[0] {
            Entry::Fact(fact) => match &fact.value {
                FactValue::Stats(aggregate) => {
                    // Two folded by the first compaction, and the two the cap now evicts (the
                    // kept episode and the next-oldest one).
                    assert_eq!(aggregate.count, 4);
                }
                _ => panic!("compaction writes statistics"),
            },
            _ => panic!("compaction writes a fact"),
        }
    }

    #[test]
    fn a_legal_hold_saves_an_entry_from_retention_and_from_deletion() {
        let store = store_with_old_episodes();
        store
            .put(Entry::Procedure(procedure("tenant_a", 1)))
            .unwrap();
        let holds = Holds::from_toml(
            "[[hold]]\nid = \"litigation-1\"\nreason = \"preserve the incident\"\n\
             set_by = \"user:legal-1\"\nscope = \"doctype:agent_memory_episode\"\n",
        )
        .unwrap();
        let policy = RetentionPolicy::default();
        let plan = plan_retention(&store, &tenant(), &policy, 100 * DAY_MS, &holds).unwrap();
        assert_eq!(plan.holds().len(), 3);
        assert!(plan.expiries().is_empty());

        let report = delete_tenant(&store, &tenant(), &holds).unwrap();
        assert_eq!(report.held.len(), 3);
        assert_eq!(report.deleted.len(), 1, "the procedure is not held");
        assert_eq!(store.count(&tenant(), MemoryKind::Episode), 3);
        assert_eq!(store.count(&tenant(), MemoryKind::Procedure), 0);
    }

    #[test]
    fn an_export_carries_only_the_tenants_entries() {
        let store = store_with_old_episodes();
        store
            .put(Entry::Procedure(procedure("tenant_a", 1)))
            .unwrap();
        store.put(Entry::Fact(fact("tenant_b", 1))).unwrap();
        let export = export_tenant(&store, &tenant()).unwrap();
        assert_eq!(export.tenant, "tenant_a");
        assert_eq!(export.episodes.len(), 3);
        assert_eq!(export.procedures.len(), 1);
        assert!(
            export.facts.is_empty(),
            "the other tenant's fact is not exported"
        );
        assert!(export.to_json().unwrap().contains("tenant_a"));
    }

    #[test]
    fn a_policy_round_trips_through_toml() {
        let policy = RetentionPolicy::default();
        let text = policy.to_toml().unwrap();
        assert_eq!(RetentionPolicy::from_toml(&text).unwrap(), policy);
        assert!(RetentionPolicy::from_toml("[episode]\nttl_days = 90\nnonsense = 1\n").is_err());
        assert_eq!(
            RetentionPolicy::from_toml("[fact]\nttl_days = 7\n")
                .unwrap()
                .fact
                .ttl_days,
            Some(7)
        );
    }
}
