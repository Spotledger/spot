//! Residency, enforced where data is placed (WP-SEC-05.T5): one [`Residency`] handle that every
//! destination the server chooses asks before the bytes move.
//!
//! # What it decides
//!
//! The pure checks live in `spot-data-policy` (`check_placement`, `check_backup_target`,
//! `check_egress_region`, `check_move`). This module gives them a tenant's policy, a table of
//! where each destination is (the region map), an audit trail and a metric:
//!
//! * [`Placement::Tenant`], `Database`, `Schema`, `Replica`, `AnalyticsMirror`, `SearchStore`
//!   and `RehearsalClone` go through `check_placement`; [`Placement::BackupDestination`] through
//!   `check_backup_target`; [`Placement::Egress`] through `check_egress_region`;
//! * a move goes through `check_move`, needs the owner's approval record, and the approval must
//!   not come from the person who requested the move (no self-approval).
//!
//! The policy is the site's (`[services.residency] policy_file`, a `ResidencyPolicy` TOML), one
//! site being one tenant, so another tenant's policy is never consulted. The region of a
//! destination comes from `[[services.residency.resource]]` entries (`id`, `kind`, `region`) in
//! `common.toml`. **A destination with no declared region is refused when a policy exists** (fail
//! closed) and is a start-up warning when none does. With no policy nothing changes.
//!
//! # Where it is called
//!
//! * Start-up ([`Residency::startup_problems`], the service check, **required** level): the
//!   node's own region, and every `[backup.targets.<name>]` of `common.toml` (the destination
//!   `spot backup` writes to: its region is the resource `object_store` named like the target).
//!   A violation stops the site with a message naming the clause, never a region list.
//! * Every other destination the server chooses is a hook: the owning module takes the
//!   [`ResidencyCheck`] port (`ctx.port::<dyn ResidencyCheck>()`) and calls it before it places
//!   anything. The exact call sites are listed in the task report (the crates are not owned
//!   here).
//!
//! # Refusals
//!
//! A refusal returned to a caller names the stable code and the policy clause, never a region
//! list ([`ResidencyRefusal`]). Each refusal is written to the audit chain (scope
//! `residency:<site>`, kind `residency.refused`: the placement, the clause and the resource, no
//! region of the policy) and counted by clause ([`ResidencyCheck::refusal_counts`]). A refusal
//! stands even when the audit entry cannot be written.
//!
//! # Policy changes
//!
//! The policy file is read again every minute while the service runs; a file that does not parse
//! keeps the previous policy in force and is reported by [`Residency::readiness_text`].

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Deserialize;
use spot_audit_chain::{
    AuditChain, AuditError as ChainError, AuditRecord, AuditStore, CheckpointPolicy,
    CheckpointSigner, PayloadValue,
};
use spot_audit_store::PgAuditStore;
use spot_core::Clock;
use spot_data_policy::{
    Clause, Region, RegionChangeApproval, RegionMap, ResidencyPolicy, ResourceKind, ResourceRegion,
    check_backup_target, check_egress_region, check_move, check_placement,
};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: before the modules that place data.
pub const ORDER: u32 = 1002;

/// The fixed prefix of the residency chain's scope; the site name follows.
pub const SCOPE_PREFIX: &str = "residency:";
const KIND_ROOT: &str = "residency";

/// A policy clause refused the destination.
pub const REFUSED: &str = "residency_host.refused";
/// The destination has no declared region and the tenant has a residency policy.
pub const UNDECLARED: &str = "residency_host.undeclared";
/// The move was approved by the person who requested it.
pub const SELF_APPROVAL: &str = "residency_host.self_approval";
/// The residency policy file could not be read or is invalid.
pub const POLICY_INVALID: &str = "residency_host.policy_invalid";

/// What is being placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Placement {
    /// A new tenant.
    Tenant,
    /// A database.
    Database,
    /// A schema.
    Schema,
    /// A replica.
    Replica,
    /// An analytics mirror.
    AnalyticsMirror,
    /// A search store.
    SearchStore,
    /// A rehearsal clone.
    RehearsalClone,
    /// A backup, snapshot or archive target.
    BackupDestination,
    /// Data leaving for an endpoint (an AI provider, a connector).
    Egress,
}

impl Placement {
    /// The stable word used in the audit entry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Database => "database",
            Self::Schema => "schema",
            Self::Replica => "replica",
            Self::AnalyticsMirror => "analytics_mirror",
            Self::SearchStore => "search_store",
            Self::RehearsalClone => "rehearsal_clone",
            Self::BackupDestination => "backup_destination",
            Self::Egress => "egress",
        }
    }
}

/// A refusal as a caller sees it: a code and the clause, never a region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResidencyRefusal {
    /// One of the `residency_host.*` codes.
    pub code: &'static str,
    /// The policy clause (`allowed_regions`, `forbidden_regions`, ...), or empty.
    pub clause: &'static str,
}

impl std::fmt::Display for ResidencyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.code, self.clause)
    }
}

impl std::error::Error for ResidencyRefusal {}

/// A boxed future, as the port's methods return.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What every placing module asks before it places anything.
pub trait ResidencyCheck: Send + Sync {
    /// Whether `what` may be placed at the destination `id` of `kind`.
    fn check_destination<'a>(
        &'a self,
        what: Placement,
        kind: ResourceKind,
        id: &'a str,
    ) -> Fut<'a, std::result::Result<(), ResidencyRefusal>>;

    /// Whether the tenant may move from cluster `from` to cluster `to`, with the owner's
    /// approval record and the identity that requested the move.
    fn check_move<'a>(
        &'a self,
        from: &'a str,
        to: &'a str,
        requested_by: &'a str,
        approval: Option<RegionChangeApproval>,
    ) -> Fut<'a, std::result::Result<(), ResidencyRefusal>>;

    /// Refusals so far, by clause.
    fn refusal_counts(&self) -> BTreeMap<String, u64>;
}

/// `[services.residency]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service exists on this site. Off by default.
    pub enabled: bool,
    /// The region this node's data (the site's database) is in.
    pub node_region: Option<String>,
    /// The tenant's `ResidencyPolicy` TOML, relative to the configuration root. Absent means no
    /// policy: nothing is refused.
    pub policy_file: Option<PathBuf>,
    /// Where each destination is: `[[services.residency.resource]]`.
    pub resource: Vec<ResourceRegion>,
}

impl Settings {
    /// Refuses an unusable node region.
    ///
    /// # Errors
    ///
    /// [`Error`] naming the key.
    pub fn validate(&self) -> Result<()> {
        if let Some(region) = &self.node_region
            && Region::new(region.clone()).is_err()
        {
            return Err(Error(
                "services.residency.node_region is not a region".to_owned(),
            ));
        }
        Ok(())
    }
}

struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }
    fn sign(&self, _message: &[u8]) -> std::result::Result<Vec<u8>, ChainError> {
        Err(ChainError::Signer("no checkpoint key is configured".into()))
    }
}

/// The chain scope of `site`'s residency refusals.
#[must_use]
pub fn scope_of(site: &str) -> String {
    format!("{SCOPE_PREFIX}{site}")
}

/// Where the policy text comes from.
pub trait PolicyText: Send + Sync {
    /// The policy text, `Ok(None)` when the tenant has none.
    ///
    /// # Errors
    ///
    /// A reason it could not be read.
    fn read(&self) -> std::result::Result<Option<String>, String>;
}

/// The policy file of the settings.
pub struct FilePolicy(pub Option<PathBuf>);

impl PolicyText for FilePolicy {
    fn read(&self) -> std::result::Result<Option<String>, String> {
        match &self.0 {
            None => Ok(None),
            Some(path) => std::fs::read_to_string(path)
                .map(Some)
                .map_err(|_| "the residency policy file could not be read".to_owned()),
        }
    }
}

/// The residency handle of one site.
pub struct Residency<S: AuditStore + 'static> {
    site: String,
    node_region: Option<Region>,
    map: RegionMap,
    backup_targets: Vec<String>,
    text: Arc<dyn PolicyText>,
    policy: RwLock<Option<ResidencyPolicy>>,
    chain: AuditChain<S>,
    clock: Arc<dyn Clock>,
    counts: Mutex<BTreeMap<String, u64>>,
    last_error: Mutex<Option<&'static str>>,
}

fn region_map_of(resources: &[ResourceRegion]) -> RegionMap {
    // Built through the crate's own TOML loader so duplicates are refused the same way.
    let mut text = String::new();
    for r in resources {
        text.push_str(&format!(
            "[[resource]]\nid = {:?}\nkind = {:?}\nregion = {:?}\n",
            r.id,
            r.kind.as_str(),
            r.region.as_str()
        ));
    }
    RegionMap::from_toml(&text).unwrap_or_default()
}

impl<S: AuditStore + 'static> Residency<S> {
    /// The handle of `site`.
    ///
    /// # Errors
    ///
    /// [`Error`] when two resources share a kind and id, or the node region is malformed.
    pub fn new(
        site: &str,
        settings: &Settings,
        backup_targets: Vec<String>,
        text: Arc<dyn PolicyText>,
        clock: Arc<dyn Clock>,
        store: S,
    ) -> Result<Arc<Self>> {
        settings.validate()?;
        let mut seen = std::collections::BTreeSet::new();
        for r in &settings.resource {
            if !seen.insert((r.kind, r.id.clone())) {
                return Err(Error(
                    "services.residency.resource repeats a destination".to_owned(),
                ));
            }
        }
        let node_region =
            match &settings.node_region {
                Some(region) => Some(Region::new(region.clone()).map_err(|_| {
                    Error("services.residency.node_region is not a region".to_owned())
                })?),
                None => None,
            };
        let residency = Arc::new(Self {
            site: site.to_owned(),
            node_region,
            map: region_map_of(&settings.resource),
            backup_targets,
            text,
            policy: RwLock::new(None),
            chain: AuditChain::new(
                store,
                scope_of(site),
                Arc::new(NoSigner),
                CheckpointPolicy { every: 0 },
            ),
            clock,
            counts: Mutex::new(BTreeMap::new()),
            last_error: Mutex::new(None),
        });
        residency.reload();
        Ok(residency)
    }

    /// Reads the policy again. A file that is unreadable or invalid keeps the previous policy
    /// (and, at start-up, leaves none, which [`Residency::startup_problems`] reports).
    pub fn reload(&self) {
        let outcome = match self.text.read() {
            Ok(None) => Ok(None),
            Ok(Some(text)) => ResidencyPolicy::from_toml(&text)
                .map(Some)
                .map_err(|_| POLICY_INVALID),
            Err(_) => Err(POLICY_INVALID),
        };
        match outcome {
            Ok(policy) => {
                *self.policy.write().unwrap_or_else(|p| p.into_inner()) = policy;
                *self.last_error.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
            Err(code) => *self.last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(code),
        }
    }

    fn policy(&self) -> Option<ResidencyPolicy> {
        self.policy
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
    }

    /// The decision for `what` at `region`, by the pure checks.
    fn judge(
        policy: &ResidencyPolicy,
        what: Placement,
        region: &Region,
    ) -> std::result::Result<(), Clause> {
        let result = match what {
            Placement::BackupDestination => check_backup_target(policy, region),
            Placement::Egress => check_egress_region(policy, region),
            _ => check_placement(policy, region),
        };
        result.map_err(|refusal| refusal.clause)
    }

    async fn refuse(
        &self,
        what: &str,
        kind: &str,
        id: &str,
        code: &'static str,
        clause: &'static str,
    ) -> ResidencyRefusal {
        *self
            .counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry((if clause.is_empty() { code } else { clause }).to_owned())
            .or_default() += 1;
        let payload: BTreeMap<String, PayloadValue> = [
            ("placement", what),
            ("resource_kind", kind),
            ("resource", id),
            ("clause", clause),
            ("code", code),
        ]
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_owned(),
                PayloadValue::Text(v.chars().take(200).collect()),
            )
        })
        .collect();
        let record = AuditRecord {
            kind: format!("{KIND_ROOT}.refused"),
            actor: "system".to_owned(),
            on_behalf_of: None,
            subject: None,
            at: self.now_micros(),
            payload,
            masked: true,
        };
        // The refusal stands whether or not the entry could be written.
        if self.chain.append(record).await.is_err() {
            tracing::warn!(site = %self.site, "a residency refusal could not be audited");
        }
        ResidencyRefusal { code, clause }
    }

    /// The reasons the site must not start: the node's region and every backup target. Names the
    /// clause and the destination, never a region list. Empty when all is well; with no policy it
    /// is always empty (undeclared destinations are only a warning, logged here).
    pub fn startup_problems(&self) -> Vec<String> {
        let Some(policy) = self.policy() else {
            let undeclared: Vec<&str> = self
                .backup_targets
                .iter()
                .filter(|t| self.map.region_of(ResourceKind::ObjectStore, t).is_none())
                .map(String::as_str)
                .collect();
            if !undeclared.is_empty() {
                tracing::warn!(
                    site = %self.site,
                    targets = %undeclared.join(","),
                    "backup targets have no declared region (no residency policy is in force)"
                );
            }
            let error = *self.last_error.lock().unwrap_or_else(|p| p.into_inner());
            return error.map(|code| vec![code.to_owned()]).unwrap_or_default();
        };
        let mut problems = Vec::new();
        match &self.node_region {
            None => problems.push("the node has no declared region".to_owned()),
            Some(region) => {
                if let Err(clause) = Self::judge(&policy, Placement::Database, region) {
                    problems.push(format!("the node's region is refused by {clause}"));
                }
            }
        }
        for target in &self.backup_targets {
            match self.map.region_of(ResourceKind::ObjectStore, target) {
                None => problems.push(format!(
                    "the backup target `{target}` has no declared region"
                )),
                Some(region) => {
                    if let Err(clause) = Self::judge(&policy, Placement::BackupDestination, region)
                    {
                        problems.push(format!(
                            "the backup target `{target}` is refused by {clause}"
                        ));
                    }
                }
            }
        }
        problems
    }

    /// `residency: ok` or `residency: degraded (<code>)`.
    pub fn readiness_text(&self) -> String {
        match *self.last_error.lock().unwrap_or_else(|p| p.into_inner()) {
            Some(code) => format!("residency: degraded ({code})"),
            None => "residency: ok".to_owned(),
        }
    }
}

impl<S: AuditStore + 'static> ResidencyCheck for Residency<S> {
    fn check_destination<'a>(
        &'a self,
        what: Placement,
        kind: ResourceKind,
        id: &'a str,
    ) -> Fut<'a, std::result::Result<(), ResidencyRefusal>> {
        Box::pin(async move {
            let Some(policy) = self.policy() else {
                return Ok(());
            };
            let Some(region) = self.map.region_of(kind, id) else {
                return Err(self
                    .refuse(what.as_str(), kind.as_str(), id, UNDECLARED, "region_map")
                    .await);
            };
            match Self::judge(&policy, what, region) {
                Ok(()) => Ok(()),
                Err(clause) => Err(self
                    .refuse(what.as_str(), kind.as_str(), id, REFUSED, clause.as_str())
                    .await),
            }
        })
    }

    fn check_move<'a>(
        &'a self,
        from: &'a str,
        to: &'a str,
        requested_by: &'a str,
        approval: Option<RegionChangeApproval>,
    ) -> Fut<'a, std::result::Result<(), ResidencyRefusal>> {
        Box::pin(async move {
            let Some(policy) = self.policy() else {
                return Ok(());
            };
            let (Some(source), Some(target)) = (
                self.map.region_of(ResourceKind::Cluster, from),
                self.map.region_of(ResourceKind::Cluster, to),
            ) else {
                return Err(self
                    .refuse("move", "cluster", to, UNDECLARED, "region_map")
                    .await);
            };
            if approval
                .as_ref()
                .is_some_and(|a| a.approved_by == requested_by)
            {
                return Err(self.refuse("move", "cluster", to, SELF_APPROVAL, "").await);
            }
            match check_move(&policy, source, target, approval.as_ref()) {
                Ok(()) => Ok(()),
                Err(refusal) => Err(self
                    .refuse("move", "cluster", to, REFUSED, refusal.clause.as_str())
                    .await),
            }
        })
    }

    fn refusal_counts(&self) -> BTreeMap<String, u64> {
        self.counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// The hosted service: the start-up check (required) and the policy reload loop.
pub struct ResidencyService<S: AuditStore + 'static> {
    residency: Arc<Residency<S>>,
}

impl<S: AuditStore + 'static> Service for ResidencyService<S> {
    fn name(&self) -> &'static str {
        "residency"
    }

    fn level(&self) -> Level {
        // A destination outside the allowed regions is a legal incident, not a degradation.
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let problems = self.residency.startup_problems();
            if problems.is_empty() {
                Ok(())
            } else {
                Err(problems.join("; "))
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    _ = ticker.tick() => self.residency.reload(),
                }
            }
        })
    }
}

/// Registers the service when `[services.residency] enabled = true` and provides
/// [`ResidencyCheck`] as a port for every module that starts later.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("residency")?;
    settings.validate()?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let policy = settings.policy_file.as_ref().map(|p| env.root.join(p));
    let residency = Residency::new(
        &env.site.site,
        &settings,
        env.loaded.common.backup.targets.keys().cloned().collect(),
        Arc::new(FilePolicy(policy)),
        Arc::clone(ctx.clock()),
        PgAuditStore::new(env.backend.pool()),
    )?;
    ctx.provide::<dyn ResidencyCheck>(Arc::clone(&residency) as Arc<dyn ResidencyCheck>)?;
    ctx.add(Arc::new(ResidencyService { residency }) as Arc<dyn Service>);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_audit_chain::InMemoryStore;
    use time::OffsetDateTime;

    struct Fixed;
    impl Clock for Fixed {
        fn now_utc(&self) -> OffsetDateTime {
            OffsetDateTime::UNIX_EPOCH
        }
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }
    }

    struct Text(Mutex<Option<String>>);
    impl PolicyText for Text {
        fn read(&self) -> std::result::Result<Option<String>, String> {
            Ok(self.0.lock().unwrap_or_else(|p| p.into_inner()).clone())
        }
    }

    fn resource(id: &str, kind: ResourceKind, region: &str) -> ResourceRegion {
        ResourceRegion {
            id: id.to_owned(),
            kind,
            region: Region::new(region).unwrap(),
        }
    }

    fn handle(policy: Option<&str>, targets: &[&str]) -> Arc<Residency<InMemoryStore>> {
        let settings = Settings {
            enabled: true,
            node_region: Some("eu-west".to_owned()),
            policy_file: None,
            resource: vec![
                resource("replica-eu", ResourceKind::Cluster, "eu-west"),
                resource("replica-us", ResourceKind::Cluster, "us-east"),
                resource("search-us", ResourceKind::Cluster, "us-east"),
                resource("bucket-eu", ResourceKind::ObjectStore, "eu-west"),
                resource("bucket-us", ResourceKind::ObjectStore, "us-east"),
                resource("llm-us", ResourceKind::Endpoint, "us-east"),
            ],
        };
        Residency::new(
            "acme",
            &settings,
            targets.iter().map(|t| (*t).to_owned()).collect(),
            Arc::new(Text(Mutex::new(policy.map(str::to_owned)))),
            Arc::new(Fixed),
            InMemoryStore::new(),
        )
        .unwrap()
    }

    const EU_ONLY: &str = "allowed_regions = [\"eu-west\"]\nbackup_regions = [\"eu-west\"]\n";

    #[tokio::test]
    async fn a_policy_for_one_region_refuses_every_other_placement_and_allows_its_own() {
        let r = handle(Some(EU_ONLY), &[]);
        for what in [
            Placement::Replica,
            Placement::AnalyticsMirror,
            Placement::SearchStore,
            Placement::RehearsalClone,
            Placement::Schema,
        ] {
            let refused = r
                .check_destination(what, ResourceKind::Cluster, "replica-us")
                .await
                .unwrap_err();
            assert_eq!((refused.code, refused.clause), (REFUSED, "allowed_regions"));
            assert!(
                r.check_destination(what, ResourceKind::Cluster, "replica-eu")
                    .await
                    .is_ok()
            );
        }
        let backup = r
            .check_destination(
                Placement::BackupDestination,
                ResourceKind::ObjectStore,
                "bucket-us",
            )
            .await
            .unwrap_err();
        assert_eq!(backup.clause, "backup_regions");
        let egress = r
            .check_destination(Placement::Egress, ResourceKind::Endpoint, "llm-us")
            .await
            .unwrap_err();
        assert_eq!(egress.clause, "egress_region");
    }

    #[tokio::test]
    async fn an_undeclared_destination_is_refused_when_a_policy_exists_and_ignored_without_one() {
        let with = handle(Some(EU_ONLY), &[]);
        let refused = with
            .check_destination(Placement::Replica, ResourceKind::Cluster, "mystery")
            .await
            .unwrap_err();
        assert_eq!(refused.code, UNDECLARED);
        let without = handle(None, &[]);
        assert!(
            without
                .check_destination(Placement::Replica, ResourceKind::Cluster, "mystery")
                .await
                .is_ok()
        );
        assert!(
            without
                .check_destination(Placement::Replica, ResourceKind::Cluster, "replica-us")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_move_needs_the_owner_approval_and_not_from_the_requester() {
        let r = handle(Some("allowed_regions = [\"eu-west\", \"us-east\"]\n"), &[]);
        let none = r
            .check_move("replica-eu", "replica-us", "user:a", None)
            .await
            .unwrap_err();
        assert_eq!(none.clause, "region_change_unapproved");
        let own = RegionChangeApproval {
            approved_by: "user:a".to_owned(),
            at: 1,
        };
        assert_eq!(
            r.check_move("replica-eu", "replica-us", "user:a", Some(own))
                .await
                .unwrap_err()
                .code,
            SELF_APPROVAL
        );
        let owner = RegionChangeApproval {
            approved_by: "user:owner".to_owned(),
            at: 1,
        };
        assert!(
            r.check_move("replica-eu", "replica-us", "user:a", Some(owner))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn every_refusal_is_audited_without_a_region_and_counted_by_clause() {
        let r = handle(Some(EU_ONLY), &[]);
        let _ = r
            .check_destination(Placement::Replica, ResourceKind::Cluster, "replica-us")
            .await;
        let _ = r
            .check_destination(Placement::Replica, ResourceKind::Cluster, "replica-us")
            .await;
        assert_eq!(r.refusal_counts().get("allowed_regions"), Some(&2));
        let head = r.chain.head().await.unwrap().unwrap().seq;
        assert_eq!(head, 2);
        let entry = r.chain.read_range(1, 1).await.unwrap().remove(0);
        assert_eq!(entry.record.kind, "residency.refused");
        assert!(!format!("{:?}", entry.record.payload).contains("us-east"));
    }

    #[test]
    fn start_up_refuses_a_backup_target_outside_the_policy_and_an_undeclared_one() {
        let bad = handle(Some(EU_ONLY), &["bucket-us"]);
        let problems = bad.startup_problems();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("backup_regions") && !problems[0].contains("us-east"));
        assert_eq!(
            handle(Some(EU_ONLY), &["unknown-bucket"])
                .startup_problems()
                .len(),
            1
        );
        assert!(
            handle(Some(EU_ONLY), &["bucket-eu"])
                .startup_problems()
                .is_empty()
        );
        assert!(handle(None, &["bucket-us"]).startup_problems().is_empty());
    }

    #[test]
    fn an_invalid_policy_keeps_the_previous_one_and_is_reported() {
        let r = handle(Some(EU_ONLY), &[]);
        let broken = Arc::new(Text(Mutex::new(Some("allowed_regions = [".to_owned()))));
        let again = Residency::new(
            "acme",
            &Settings::default(),
            vec![],
            broken,
            Arc::new(Fixed),
            InMemoryStore::new(),
        )
        .unwrap();
        assert!(again.readiness_text().contains("degraded"));
        assert!(r.readiness_text().contains("ok"));
    }
}
