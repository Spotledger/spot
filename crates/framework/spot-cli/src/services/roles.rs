//! Roles on a running site (WP-IAM-03.T2): the role administration reads and writes of the
//! identity API (`identity_role_list|get|customise|upgrade_diff`) over one **atomically published
//! snapshot**, and the publisher that rebuilds it on every role or assignment change.
//!
//! `spot-iam-roles` owns the rules (duties, inheritance, copy on customise, the upgrade diff, the
//! snapshot build with the portal guard). `spot-http` owns the routes ([`RoleAdmin`]). This
//! module is the host:
//!
//! - **the snapshot**: [`SnapshotPublisher`] holds the current [`Published`] snapshot behind an
//!   `Arc`. A rebuild loads the roles and assignments from the site's [`RoleSource`], builds the
//!   new [`SnapshotInput`] off to the side and swaps it in one step. A request that took the
//!   `Arc` before the swap finishes under the snapshot it started with. A build that fails
//!   (source error, an invalid assignment, an unknown role) **keeps the previous snapshot**,
//!   marks the module degraded and never publishes an empty or partial one; a build failure
//!   never widens access because the old snapshot stays in force;
//! - **when it rebuilds**: on [`RoleChangeSink::changed`] (the outbox subscriber of a role or
//!   assignment event calls it; so does this module after its own write), at the next instant an
//!   assignment starts or ends (time-boxed assignments change at the second), and every
//!   `refresh_seconds` (default 5) as the cross-node bound: another node's write is in the
//!   shared stored records, so each node sees it within that interval without a new channel. A
//!   rebuild whose input equals the published one swaps nothing;
//! - **the administration**: reads answer from the snapshot (inheritance flattened to the
//!   `permissions` list), `customise` copies a built-in role (the copy records the version it
//!   came from) and `upgrade_diff` is the read-only diff between a copy and its reference role.
//!   A customised role is never changed by a pack upgrade.
//!
//! # Level
//!
//! Optional. Without a [`RoleSource`] port (no module of this site provides one) or while no
//! snapshot has been built, the check fails with a reason and the site is degraded; the role
//! routes answer `503 server.unavailable`, never an empty list. The engine's own authorisation
//! is not changed by this module (see `docs/running-spot/roles.md`, "What the engine
//! reads"). The setting is `services.roles` (`enabled`, default true; `refresh_seconds`, 1 to
//! 300, default 5).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use spot_http::api::ports::identity::{IdResult, Page, PageIn, RoleAdmin, RoleOut, UpgradeDiffOut};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_iam_roles::{
    Assignment, Origin, OutsideGuard, Registry, Role, RoleName, SnapshotInput,
    build_snapshot_input_guarded, copy_role, diff_reference,
};
use time::OffsetDateTime;
use tokio::sync::{Notify, watch};

use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services and the session routes.
pub const ORDER: u32 = 1045;

/// The catalogued codes of this module (they appear in readiness reasons and logs only).
pub const CODE_NO_SOURCE: &str = "roles_host.no_source";
/// A rebuild failed and the previous snapshot stays in force.
pub const CODE_BUILD_FAILED: &str = "roles_host.snapshot_build_failed";
/// The stored roles or assignments could not be read.
pub const CODE_SOURCE_UNAVAILABLE: &str = "roles_host.source_unavailable";

/// `[services.roles]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site hosts roles at all.
    pub enabled: bool,
    /// The longest a node waits before it re-reads the stored records (the cross-node bound).
    pub refresh_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_seconds: 5,
        }
    }
}

/// Everything one rebuild reads: the role registry (reference packs and the tenant's custom
/// roles), the assignments in force or pending, the portal guard and the record id of every role.
pub struct RoleData {
    /// The registry of duties and roles.
    pub registry: Registry,
    /// Every assignment record, active or not (the build filters by the instant).
    pub assignments: Vec<Assignment>,
    /// The portal users and the allow-list (ADR-083 Barrier 3).
    pub guard: OutsideGuard,
    /// The record id of each role, by role name. A role without an id is not administrable.
    pub ids: BTreeMap<String, String>,
}

/// The stored roles of the site. The host's adapter reads the platform records; this module
/// never touches storage itself. Errors are operator text, never returned to a client.
pub trait RoleSource: Send + Sync {
    /// The current roles and assignments of the site, in one consistent read.
    fn load<'a>(&'a self) -> BoxFuture<'a, std::result::Result<RoleData, String>>;
    /// Stores a new custom role and answers its record id.
    fn create_role<'a>(&'a self, role: Role) -> BoxFuture<'a, std::result::Result<String, String>>;
}

/// What a writer of role or assignment records (or the outbox subscriber that sees them) calls
/// so the snapshot is rebuilt now instead of at the next refresh.
pub trait RoleChangeSink: Send + Sync {
    /// A role, duty, assignment or scope changed.
    fn changed(&self);
}

/// The published snapshot, read-only.
pub trait RoleSnapshots: Send + Sync {
    /// The current snapshot; `None` until the first build succeeded.
    fn current(&self) -> Option<Arc<Published>>;
}

/// One published snapshot: immutable once built.
pub struct Published {
    /// 1 for the first build, +1 for each swap.
    pub version: u64,
    /// When it was built.
    pub built_at: OffsetDateTime,
    /// The engine's input.
    pub input: SnapshotInput,
    /// What it was built from.
    pub data: RoleData,
}

/// Holds the current snapshot and swaps it atomically.
#[derive(Default)]
pub struct SnapshotPublisher {
    current: RwLock<Option<Arc<Published>>>,
    degraded: Mutex<Option<String>>,
    counter: AtomicU64,
}

impl SnapshotPublisher {
    /// The reason the last rebuild failed, `None` when healthy.
    pub fn degraded(&self) -> Option<String> {
        self.degraded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn set_degraded(&self, reason: Option<String>) {
        *self.degraded.lock().unwrap_or_else(|p| p.into_inner()) = reason;
    }

    /// Rebuilds from `source` at `now` and swaps the result in. `Ok(true)` when a new snapshot
    /// was published, `Ok(false)` when the input did not change. On `Err` the previous
    /// snapshot is untouched and the module is degraded.
    ///
    /// # Errors
    ///
    /// The operator-facing reason (a catalogued code first); it holds no record content.
    pub async fn refresh(
        &self,
        source: &dyn RoleSource,
        now: OffsetDateTime,
    ) -> std::result::Result<bool, String> {
        let data = match source.load().await {
            Ok(data) => data,
            Err(_) => {
                let reason = format!("{CODE_SOURCE_UNAVAILABLE}: the roles could not be read");
                self.set_degraded(Some(reason.clone()));
                return Err(reason);
            }
        };
        let input = match build_snapshot_input_guarded(
            &data.registry,
            &data.assignments,
            now,
            &data.guard,
        ) {
            Ok(input) => input,
            Err(errors) => {
                let reason = format!(
                    "{CODE_BUILD_FAILED}: the snapshot was refused ({}); the previous one stays",
                    errors.codes().join(", ")
                );
                self.set_degraded(Some(reason.clone()));
                return Err(reason);
            }
        };
        let mut current = self.current.write().unwrap_or_else(|p| p.into_inner());
        if current
            .as_ref()
            .is_some_and(|p| p.input == input && p.data.ids == data.ids)
        {
            drop(current);
            self.set_degraded(None);
            return Ok(false);
        }
        let version = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        *current = Some(Arc::new(Published {
            version,
            built_at: now,
            input,
            data,
        }));
        drop(current);
        self.set_degraded(None);
        Ok(true)
    }
}

impl RoleSnapshots for SnapshotPublisher {
    fn current(&self) -> Option<Arc<Published>> {
        self.current
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// The next instant after `now` at which an assignment starts or ends.
pub fn next_boundary(assignments: &[Assignment], now: OffsetDateTime) -> Option<OffsetDateTime> {
    assignments
        .iter()
        .flat_map(|a| [a.valid_from, a.valid_to])
        .flatten()
        .filter(|at| *at > now)
        .min()
}

// ---------------------------------------------------------------------------------------------
// The administration
// ---------------------------------------------------------------------------------------------

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", serde_json::Value::from(5))
}

fn not_found() -> ApiError {
    ApiError::from_code(Code::IdentityNotFound)
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::IdentityInvalid, message)
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<String> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

fn reference_version(role: &Role) -> u64 {
    match &role.origin {
        Origin::Reference { version, .. } => u64::from(version.0),
        Origin::Custom { .. } => 1,
    }
}

/// The contract's `permissions` list of a role: every flattened DocType right
/// (`doctype.right`, `:owner` for the owner-only grant) and every app permission, at most 500.
fn permissions_of(published: &Published, role: &Role) -> Vec<String> {
    let Some(resolved) = published.input.resolved.get(&role.name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for grant in &resolved.grants {
        for right in grant.grant.rights.as_slice() {
            let Some(text) = serde_json::to_value(right)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
            else {
                continue;
            };
            let owner = if grant.grant.if_owner { ":owner" } else { "" };
            out.push(format!("{}.{text}{owner}", grant.grant.doctype));
        }
    }
    out.extend(
        resolved
            .app_permissions
            .iter()
            .map(|p| p.permission.as_str().to_owned()),
    );
    out.truncate(500);
    out
}

fn role_out(published: &Published, role: &Role) -> Option<RoleOut> {
    let id = published.data.ids.get(role.name.as_str())?.clone();
    let customised_from = match &role.origin {
        Origin::Custom {
            copied_from: Some(from),
        } => published.data.ids.get(from.role.as_str()).cloned(),
        _ => None,
    };
    Some(RoleOut {
        id,
        name: role.name.as_str().to_owned(),
        description: None,
        builtin: matches!(role.origin, Origin::Reference { .. }),
        customised_from,
        permissions: permissions_of(published, role),
        version: reference_version(role),
    })
}

/// The identity API's role operations over the published snapshot.
pub struct RoleHost {
    site: String,
    publisher: Arc<SnapshotPublisher>,
    source: Arc<dyn RoleSource>,
    clock: Arc<dyn Clock>,
    wake: Arc<Notify>,
}

impl RoleHost {
    fn snapshot(&self, ctx: &RequestCtx) -> IdResult<Arc<Published>> {
        // A call for another site is the same answer as an id that does not exist.
        if ctx.site.as_str() != self.site {
            return Err(not_found());
        }
        self.publisher.current().ok_or_else(unavailable)
    }

    fn role_of<'p>(published: &'p Published, id: &str) -> IdResult<&'p Role> {
        let name = published
            .data
            .ids
            .iter()
            .find_map(|(name, role_id)| (role_id == id).then_some(name))
            .ok_or_else(not_found)?;
        published
            .data
            .registry
            .role(&RoleName::from(name.as_str()))
            .ok_or_else(not_found)
    }
}

impl RoleAdmin for RoleHost {
    fn role_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: PageIn,
    ) -> BoxFuture<'a, IdResult<Page<RoleOut>>> {
        Box::pin(async move {
            let published = self.snapshot(ctx)?;
            let after = match &page.cursor {
                Some(cursor) => Some(
                    unhex(cursor)
                        .ok_or_else(|| ApiError::new(Code::ApiInvalidRequest, "Bad cursor."))?,
                ),
                None => None,
            };
            let mut roles: Vec<&Role> = published.data.registry.roles().collect();
            roles.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            let mut items = Vec::new();
            let mut more = false;
            for role in roles {
                if after.as_deref().is_some_and(|a| role.name.as_str() <= a) {
                    continue;
                }
                let Some(out) = role_out(&published, role) else {
                    continue;
                };
                if items.len() == page.limit {
                    more = true;
                    break;
                }
                items.push(out);
            }
            let next_cursor = more
                .then(|| items.last().map(|last| hex(&last.name)))
                .flatten();
            Ok(Page { items, next_cursor })
        })
    }

    fn role_get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<RoleOut>> {
        Box::pin(async move {
            let published = self.snapshot(ctx)?;
            let role = Self::role_of(&published, id)?;
            role_out(&published, role).ok_or_else(not_found)
        })
    }

    fn role_customise<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        name: String,
    ) -> BoxFuture<'a, IdResult<RoleOut>> {
        Box::pin(async move {
            let published = self.snapshot(ctx)?;
            let source = Self::role_of(&published, id)?;
            if !matches!(source.origin, Origin::Reference { .. }) {
                return Err(invalid("Only a built-in role can be copied to change it."));
            }
            let new_name = RoleName::from(name.as_str());
            if published.data.registry.role(&new_name).is_some() {
                return Err(invalid("A role with this name already exists."));
            }
            let copy = copy_role(source, &name);
            // The registry refuses a copy that is not valid (a cycle, a name over the limit);
            // the reason names no content.
            published
                .data
                .registry
                .with_role(copy.clone())
                .map_err(|_| invalid("The copy is not a valid role."))?;
            let new_id = self.source.create_role(copy).await.map_err(|_| {
                tracing::error!(
                    code = CODE_SOURCE_UNAVAILABLE,
                    "a customised role was not stored"
                );
                unavailable()
            })?;
            // Read-your-write on this node; a failed rebuild keeps the old snapshot and the
            // refresh loop retries.
            let _ = self
                .publisher
                .refresh(&*self.source, self.clock.now_utc())
                .await;
            self.wake.notify_one();
            let now = self.publisher.current().ok_or_else(unavailable)?;
            let created = Self::role_of(&now, &new_id)?;
            role_out(&now, created).ok_or_else(not_found)
        })
    }

    fn role_upgrade_diff<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<UpgradeDiffOut>> {
        Box::pin(async move {
            let published = self.snapshot(ctx)?;
            let copy = Self::role_of(&published, id)?;
            let Origin::Custom {
                copied_from: Some(from),
            } = &copy.origin
            else {
                return Err(not_found());
            };
            let reference = published
                .data
                .registry
                .role(&from.role)
                .filter(|r| matches!(r.origin, Origin::Reference { .. }))
                .ok_or_else(not_found)?;
            let diff = diff_reference(copy, reference);
            let mut added = Vec::new();
            let mut removed = Vec::new();
            added.extend(
                diff.duties_added
                    .iter()
                    .map(|d| format!("duty:{}", d.as_str())),
            );
            removed.extend(
                diff.duties_removed
                    .iter()
                    .map(|d| format!("duty:{}", d.as_str())),
            );
            let key = |g: &spot_iam_roles::GrantKey| {
                format!("{}{}", g.doctype, if g.if_owner { ":owner" } else { "" })
            };
            added.extend(diff.grants_added.iter().map(key));
            removed.extend(diff.grants_removed.iter().map(key));
            added.extend(diff.app_permissions_added.iter().map(ToString::to_string));
            removed.extend(diff.app_permissions_removed.iter().map(ToString::to_string));
            added.extend(
                diff.parents_added
                    .iter()
                    .map(|r| format!("role:{}", r.as_str())),
            );
            removed.extend(
                diff.parents_removed
                    .iter()
                    .map(|r| format!("role:{}", r.as_str())),
            );
            added.truncate(500);
            removed.truncate(500);
            Ok(UpgradeDiffOut {
                added,
                removed,
                from_version: u64::from(from.version.0),
                to_version: reference_version(reference),
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The service and its registration
// ---------------------------------------------------------------------------------------------

struct Sink(Arc<Notify>);

impl RoleChangeSink for Sink {
    fn changed(&self) {
        self.0.notify_one();
    }
}

struct RolesService {
    publisher: Arc<SnapshotPublisher>,
    source: Arc<dyn RoleSource>,
    clock: Arc<dyn Clock>,
    wake: Arc<Notify>,
    refresh: Duration,
}

impl Service for RolesService {
    fn name(&self) -> &'static str {
        "roles"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            // The first snapshot is built here; a later failure keeps the old one and is
            // reported by the same check.
            self.publisher
                .refresh(&*self.source, self.clock.now_utc())
                .await
                .map(|_| ())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                let now = self.clock.now_utc();
                let mut wait = self.refresh;
                if let Some(current) = self.publisher.current()
                    && let Some(at) = next_boundary(&current.data.assignments, now)
                {
                    // One millisecond past the instant, so the build sees it as passed.
                    wait = wait.min((at - now).unsigned_abs() + Duration::from_millis(1));
                }
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = self.wake.notified() => {}
                    () = tokio::time::sleep(wait) => {}
                }
                if let Err(reason) = self
                    .publisher
                    .refresh(&*self.source, self.clock.now_utc())
                    .await
                {
                    // The previous snapshot stays in force.
                    tracing::warn!(reason = %reason, "role snapshot rebuild failed");
                }
            }
        })
    }
}

/// Registers the role hosting.
///
/// # Errors
///
/// A setting that does not decode or is out of range.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("roles")?;
    if !settings.enabled {
        return Ok(());
    }
    if !(1..=300).contains(&settings.refresh_seconds) {
        return Err(Error(
            "services.roles.refresh_seconds must be from 1 to 300".to_owned(),
        ));
    }
    let Some(source) = ctx.port::<dyn RoleSource>() else {
        if ctx.assembling() {
            ctx.add(Idle::new(
                "roles",
                Level::Optional,
                || -> CheckFuture<'static> {
                    Box::pin(async {
                        Err(format!(
                            "{CODE_NO_SOURCE}: no module of this site provides the stored roles"
                        ))
                    })
                },
            ));
        }
        return Ok(());
    };
    let clock = Arc::clone(ctx.clock());
    let publisher = Arc::new(SnapshotPublisher::default());
    let wake = Arc::new(Notify::new());
    let host = Arc::new(RoleHost {
        site: ctx.label().to_owned(),
        publisher: Arc::clone(&publisher),
        source: Arc::clone(&source),
        clock: Arc::clone(&clock),
        wake: Arc::clone(&wake),
    });
    ctx.provide::<dyn RoleAdmin>(host)?;
    ctx.provide::<dyn RoleSnapshots>(Arc::clone(&publisher) as Arc<dyn RoleSnapshots>)?;
    ctx.provide::<dyn RoleChangeSink>(Arc::new(Sink(Arc::clone(&wake))))?;
    ctx.add(Arc::new(RolesService {
        publisher,
        source,
        clock,
        wake,
        refresh: Duration::from_secs(settings.refresh_seconds),
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::AtomicBool;

    use spot_iam_roles::{Duty, Version};
    use time::macros::datetime;

    use super::*;

    struct Source {
        fail: AtomicBool,
        assignments: Mutex<Vec<Assignment>>,
        roles: Mutex<Vec<Role>>,
    }

    impl Source {
        fn new() -> Arc<Source> {
            Arc::new(Source {
                fail: AtomicBool::new(false),
                assignments: Mutex::new(Vec::new()),
                roles: Mutex::new(vec![reference("Manager"), reference("Viewer")]),
            })
        }
    }

    fn reference(name: &str) -> Role {
        Role {
            origin: Origin::Reference {
                pack: "base".into(),
                version: Version(3),
            },
            ..Role::custom(name)
        }
    }

    impl RoleSource for Source {
        fn load<'a>(&'a self) -> BoxFuture<'a, std::result::Result<RoleData, String>> {
            Box::pin(async move {
                if self.fail.load(Ordering::SeqCst) {
                    return Err("down".into());
                }
                let roles = self.roles.lock().unwrap().clone();
                let ids = roles
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (r.name.as_str().to_owned(), format!("id-{i}")))
                    .collect();
                let registry =
                    Registry::new(Vec::<Duty>::new(), roles).map_err(|e| e.codes().join(","))?;
                Ok(RoleData {
                    registry,
                    assignments: self.assignments.lock().unwrap().clone(),
                    guard: OutsideGuard::none(),
                    ids,
                })
            })
        }

        fn create_role<'a>(
            &'a self,
            role: Role,
        ) -> BoxFuture<'a, std::result::Result<String, String>> {
            Box::pin(async move {
                let mut roles = self.roles.lock().unwrap();
                roles.push(role);
                Ok(format!("id-{}", roles.len() - 1))
            })
        }
    }

    const NOW: OffsetDateTime = datetime!(2026-11-12 10:00:00 UTC);

    #[tokio::test]
    async fn the_first_build_publishes_and_an_unchanged_input_swaps_nothing() {
        let (publisher, source) = (SnapshotPublisher::default(), Source::new());
        assert!(publisher.current().is_none());
        assert!(publisher.refresh(&*source, NOW).await.unwrap());
        assert_eq!(publisher.current().unwrap().version, 1);
        assert!(!publisher.refresh(&*source, NOW).await.unwrap());
        assert_eq!(publisher.current().unwrap().version, 1);
    }

    #[tokio::test]
    async fn a_failing_build_keeps_the_old_snapshot_and_degrades() {
        let (publisher, source) = (SnapshotPublisher::default(), Source::new());
        publisher.refresh(&*source, NOW).await.unwrap();
        let held = publisher.current().unwrap();
        source.fail.store(true, Ordering::SeqCst);
        let reason = publisher.refresh(&*source, NOW).await.unwrap_err();
        assert!(reason.starts_with(CODE_SOURCE_UNAVAILABLE), "{reason}");
        assert_eq!(publisher.degraded(), Some(reason));
        assert_eq!(publisher.current().unwrap().version, held.version);
        // An invalid assignment (an unknown role) is refused whole: nothing partial is published.
        source.fail.store(false, Ordering::SeqCst);
        source
            .assignments
            .lock()
            .unwrap()
            .push(Assignment::new("a1", "u1", "No Such Role"));
        let reason = publisher.refresh(&*source, NOW).await.unwrap_err();
        assert!(reason.starts_with(CODE_BUILD_FAILED), "{reason}");
        assert_eq!(publisher.current().unwrap().version, held.version);
        // Recovery clears the degraded state.
        source.assignments.lock().unwrap().clear();
        publisher.refresh(&*source, NOW).await.unwrap();
        assert_eq!(publisher.degraded(), None);
    }

    #[tokio::test]
    async fn a_request_in_flight_keeps_the_snapshot_it_started_with() {
        let (publisher, source) = (SnapshotPublisher::default(), Source::new());
        publisher.refresh(&*source, NOW).await.unwrap();
        let in_flight = publisher.current().unwrap();
        source
            .assignments
            .lock()
            .unwrap()
            .push(Assignment::new("a1", "u1", "Manager"));
        assert!(publisher.refresh(&*source, NOW).await.unwrap());
        assert!(in_flight.input.assignments.is_empty());
        assert_eq!(
            publisher.current().unwrap().input.assignments["u1"].len(),
            1
        );
        assert_eq!(publisher.current().unwrap().version, in_flight.version + 1);
    }

    #[tokio::test]
    async fn a_time_boxed_assignment_enters_and_leaves_at_its_instant() {
        let (publisher, source) = (SnapshotPublisher::default(), Source::new());
        let mut a = Assignment::new("a1", "u1", "Manager");
        a.valid_from = Some(NOW + time::Duration::seconds(10));
        a.valid_to = Some(NOW + time::Duration::seconds(20));
        source.assignments.lock().unwrap().push(a.clone());
        publisher.refresh(&*source, NOW).await.unwrap();
        assert!(publisher.current().unwrap().input.assignments.is_empty());
        assert_eq!(next_boundary(&[a.clone()], NOW), a.valid_from);
        publisher
            .refresh(&*source, NOW + time::Duration::seconds(10))
            .await
            .unwrap();
        assert_eq!(
            publisher.current().unwrap().input.assignments["u1"].len(),
            1
        );
        assert_eq!(
            next_boundary(&[a.clone()], NOW + time::Duration::seconds(10)),
            a.valid_to
        );
        publisher
            .refresh(&*source, NOW + time::Duration::seconds(20))
            .await
            .unwrap();
        assert!(publisher.current().unwrap().input.assignments.is_empty());
        assert_eq!(next_boundary(&[a], NOW + time::Duration::seconds(20)), None);
    }

    fn host(source: &Arc<Source>, publisher: &Arc<SnapshotPublisher>) -> RoleHost {
        RoleHost {
            site: "acme".into(),
            publisher: Arc::clone(publisher),
            source: source.clone(),
            clock: Arc::new(spot_core::ManualClock::new(NOW)),
            wake: Arc::new(Notify::new()),
        }
    }

    fn ctx(site: &str) -> RequestCtx {
        RequestCtx {
            site: spot_core::SiteId::new(site).unwrap(),
            credential: Some(spot_http::api::service::Credential::new("t")),
            idempotency_key: None,
            language: None,
            request_id: spot_http::request::RequestId::generate(),
            peer: None,
        }
    }

    #[tokio::test]
    async fn the_admin_lists_pages_gets_customises_and_diffs() {
        let (publisher, source) = (Arc::new(SnapshotPublisher::default()), Source::new());
        let host = host(&source, &publisher);
        // Before the first build the answer is unavailable, never an empty list.
        let err = host
            .role_list(
                &ctx("acme"),
                PageIn {
                    cursor: None,
                    limit: 10,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code.as_str(), "server.unavailable");
        publisher.refresh(&*source, NOW).await.unwrap();
        let page = host
            .role_list(
                &ctx("acme"),
                PageIn {
                    cursor: None,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items[0].name, "Manager");
        let cursor = page.next_cursor.unwrap();
        let rest = host
            .role_list(
                &ctx("acme"),
                PageIn {
                    cursor: Some(cursor),
                    limit: 5,
                },
            )
            .await
            .unwrap();
        assert_eq!(rest.items.len(), 1);
        assert_eq!(rest.items[0].name, "Viewer");
        assert!(rest.next_cursor.is_none());
        assert!(page.items[0].builtin);
        // Customise a built-in role: the copy remembers its origin and the original is unchanged.
        let manager = page.items[0].id.clone();
        let copy = host
            .role_customise(&ctx("acme"), &manager, "Manager Ours".into())
            .await
            .unwrap();
        assert!(!copy.builtin);
        assert_eq!(copy.customised_from.as_deref(), Some(manager.as_str()));
        let again = host
            .role_customise(&ctx("acme"), &manager, "Manager Ours".into())
            .await
            .unwrap_err();
        assert_eq!(again.code.as_str(), "identity.invalid");
        let diff = host
            .role_upgrade_diff(&ctx("acme"), &copy.id)
            .await
            .unwrap();
        assert_eq!((diff.from_version, diff.to_version), (3, 3));
        assert!(diff.added.is_empty() && diff.removed.is_empty());
        // A copy cannot be copied, a built-in has no upgrade diff, an unknown id is not found.
        let nested = host
            .role_customise(&ctx("acme"), &copy.id, "x".into())
            .await
            .unwrap_err();
        assert_eq!(nested.code.as_str(), "identity.invalid");
        let nodiff = host
            .role_upgrade_diff(&ctx("acme"), &manager)
            .await
            .unwrap_err();
        assert_eq!(nodiff.code.as_str(), "identity.not_found");
        let missing = host.role_get(&ctx("acme"), "nope").await.unwrap_err();
        assert_eq!(missing.code.as_str(), "identity.not_found");
        // Another site's call answers exactly like a missing id.
        let other = host.role_get(&ctx("beta"), &manager).await.unwrap_err();
        assert_eq!(other.code.as_str(), "identity.not_found");
    }

    #[test]
    fn cursors_round_trip_and_the_settings_refuse_unknown_keys() {
        assert_eq!(unhex(&hex("Manager Ours")).as_deref(), Some("Manager Ours"));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        let ok: std::result::Result<Settings, _> = toml::from_str("refresh_seconds = 9");
        assert_eq!(ok.unwrap().refresh_seconds, 9);
        let bad: std::result::Result<Settings, _> = toml::from_str("refresh = 9");
        assert!(bad.is_err());
    }
}
