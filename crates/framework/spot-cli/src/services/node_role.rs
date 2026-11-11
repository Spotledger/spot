//! The edge profile of the single binary: one node role model for `spot serve` (WP-RTB-03.T1).
//!
//! Spotledger ships one executable. What a process *is* — a standalone desktop, a head-office
//! hub, or a branch edge that keeps working offline — is configuration, never a build flag
//! (decision of 26 Oct 2026, the decision log). This module is
//! where that configuration is read, validated against the host's own role set, and turned into
//! the one thing this task owns: a stable refusal when the combination is wrong, and an
//! offline-safe readiness line when it is not.
//!
//! # The table this task owns
//!
//! `[services.node_role]` of `common.toml` (see [`Settings`]):
//!
//! - `role`: `"standalone"` (default), `"hub"` or `"edge"`.
//! - `node`: this node's identity; required for `edge`, and must equal `[partition] node`.
//! - `hub_url`, `hub_fingerprint`, `enrol_code_file`: edge-only.
//! - `listen`: hub-only, the address `services.sync_hub` (WP-RTB-02) is handed.
//!
//! An edge never carries a hub setting and a hub or standalone never carries an edge setting:
//! [`decide`] refuses the mix with [`codes::ROLE_CONFLICT`] before anything is built, which is
//! this task's proof that "an edge must not carry hub powers" is a configuration property, not
//! a runtime promise.
//!
//! ## Why `[services.node_role]`, not the brief's `[sync]`
//!
//! `crates/framework/spot-cli/src/config.rs` is not an owned path of this task, and its `Site`
//! and `Common` structs both carry `#[serde(deny_unknown_fields)]` with no `[sync]` field or
//! catch-all: adding a bespoke top-level `[sync]` table is not reachable without editing that
//! file. The registry built by WP-PRD-01.T26 gives every self-registering module exactly one
//! configuration seam instead — `[services.<module file stem>]`, decoded through
//! [`crate::services::SiteCtx::settings_or_default`] — and this module's file is
//! `node_role.rs`, so its table is `[services.node_role]`. See `## Open questions` in this
//! task's report for the exact trade-off and how a future task can rename it once `config.rs`'s
//! owner widens the paths.
//!
//! Because that table lives in `Common` (`common.toml`), not `Site`, it is the same for every
//! site this process hosts: one `spot serve` process is one role. A host that must serve
//! several sites in different roles needs several processes (or `config.rs`'s eventual
//! per-site `[sync]`), which is out of this task's scope.
//!
//! # The host's own role set
//!
//! Item 1 of the brief requires this module's role to agree with the host's
//! `runtime.node.roles` (`spot-agent-runtime::roles::RoleSet`). `spot-cli` has no dependency on
//! `spot-agent-runtime` and no reachable settings store (`spot-agent-runtime`'s `RoleSet` is
//! read from a [`spot_settings::ValueStore`] that belongs to a different process, the host
//! agent, not `spot serve`). Adding that dependency only to read one setting is not named in
//! this task's brief, so [`HostRoles`] mirrors the setting through the documented escape
//! clause ("the host config key that mirrors it if the setting store is not reachable from
//! spot-cli: say which"): `[services.node_role] host_roles` (a list) or, when that is absent,
//! the `SPOT_HOST_ROLES` environment variable (comma-separated), both using the role names
//! `spot-agent-runtime::roles::NodeRole` uses (`web`, `jobs`, `realtime`, `database`,
//! `branch-edge`). **Fail-closed:** when neither is given the host role set is *unknown*; a hub
//! or standalone (nothing to contradict) is accepted, but an `edge` is refused with
//! [`codes::ROLE_CONFLICT`], because an edge must positively know it runs as `branch-edge`
//! without `database`. See `## Open questions`.
//!
//! # What never happens on a refusal
//!
//! [`register`] runs at [`ORDER`], after the framework's required services already added
//! themselves to the assembly (the registry's own convention: new modules use 1000 and above).
//! A refusal here still stops the **whole site build** before [`crate::services::Services::
//! start`] is ever called — no service's `check()` runs, and no service's `run()` (no listener
//! binds, no job loop starts, nothing is written) — because [`crate::services::register_all`]
//! returns the error at once and the host never reaches `start()`. "Starts nothing" is this
//! property, not a claim that an earlier module's own `register()` touched no byte.
//!
//! # Offline-safe readiness
//!
//! The registered service is [`crate::services::Level::Optional`]: a stopped or degraded sync
//! link never fails `/api/health/ready`, only the readiness detail reports it (`sync_node:
//! offline …`). A hub or standalone node is unaffected (the detail says `off` or `hub`). The
//! actual hub-reachability probe belongs to `services.sync_node` (WP-RTB-02, out of scope); this
//! module's own check never performs network I/O and never fails.
//!
//! # The small table
//!
//! `spot_node_role_state` (one row per site: `site`, `role`, `hub_fingerprint`, `updated_at`) is
//! how [`codes::ROLE_CHANGED_UNSAFELY`] is enforced: the role recorded on a site's first start
//! stays until an explicit operation changes it. The operation itself (`spot sync change-role`)
//! is WP-RTB-04.T2's, out of scope here; until it exists the only way to move a site from one
//! role to another is to stop it and delete its row (see the edge profile notes), which
//! is deliberately not something this module automates.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services (100 through 900); the registry's own
/// convention reserves 1000 and above for modules that register themselves.
pub const ORDER: u32 = 1005;

/// The module name and the `[services.<name>]` table it reads.
pub const NAME: &str = "node_role";

/// The environment variable that mirrors the host's `runtime.node.roles` setting (see the
/// module docs' "The host's own role set"). Comma-separated role names; absent means every
/// role.
pub const HOST_ROLES_ENV: &str = "SPOT_HOST_ROLES";

/// The stable refusal codes of this module (see `catalogue/node_role.toml`). `partition.
/// scope_uncovered` stays `participants.rs`'s own code; this module never redeclares it.
pub mod codes {
    /// `[services.node_role] role` names something other than `standalone`, `hub` or `edge`.
    pub const UNKNOWN_ROLE: &str = "node_role.unknown_role";
    /// An edge names no `[partition]` profile.
    pub const EDGE_WITHOUT_PARTITION: &str = "node_role.edge_without_partition";
    /// An edge names no `hub_url`.
    pub const EDGE_WITHOUT_HUB: &str = "node_role.edge_without_hub";
    /// An edge is not yet enrolled and names no readable, non-empty `enrol_code_file`.
    pub const EDGE_NOT_ENROLLED_AND_NO_CODE: &str = "node_role.edge_not_enrolled_and_no_code";
    /// A hub names no `listen` address.
    pub const HUB_WITHOUT_LISTENER: &str = "node_role.hub_without_listener";
    /// The configured role disagrees with the host's own role set, or an edge carries a hub
    /// setting, or a hub/standalone carries an edge setting.
    pub const ROLE_CONFLICT: &str = "node_role.role_conflict";
    /// `[services.node_role] node` is absent or does not equal `[partition] node`.
    pub const NODE_ID_MISMATCH: &str = "node_role.node_id_mismatch";
    /// The role on this site's first start differs from the role `spot_node_role_state` already
    /// recorded, and no explicit role-change operation ran (WP-RTB-04.T2, out of scope).
    pub const ROLE_CHANGED_UNSAFELY: &str = "node_role.role_changed_unsafely";
    /// `spot_node_role_state` does not exist yet.
    pub const TABLE_MISSING: &str = "node_role.table_missing";
}

/// One `[services.node_role]` table. Every field is optional (role-specific requirements are
/// [`decide`]'s job, not serde's), so a malformed combination is refused with a stable code
/// instead of a generic "missing field".
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `"standalone"` (the default), `"hub"` or `"edge"`.
    pub role: Option<String>,
    /// This node's identity; required for `edge`, equal to `[partition] node`.
    pub node: Option<String>,
    /// The hub's URL; edge only.
    pub hub_url: Option<String>,
    /// The hub's certificate fingerprint; edge only.
    pub hub_fingerprint: Option<String>,
    /// The path of a one-time enrolment code; edge only.
    pub enrol_code_file: Option<String>,
    /// The address `services.sync_hub` listens on; hub only.
    pub listen: Option<String>,
    /// The host's role set (`web`, `jobs`, `realtime`, `database`, `branch-edge`), the host
    /// config key that mirrors `runtime.node.roles`. Takes precedence over [`HOST_ROLES_ENV`].
    pub host_roles: Option<Vec<String>>,
}

/// One Spotledger node role, as `[services.node_role] role` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// No sync service starts and nothing dials out: today's behaviour.
    Standalone,
    /// The side edges enrol against and sync with.
    Hub,
    /// Syncs through one hub; keeps working while the hub is unreachable.
    Edge,
}

impl Role {
    /// The wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Hub => "hub",
            Self::Edge => "edge",
        }
    }

    /// The role named `text`.
    ///
    /// # Errors
    /// [`NodeRoleError`] with [`codes::UNKNOWN_ROLE`] for anything else.
    pub fn parse(text: &str) -> std::result::Result<Self, NodeRoleError> {
        match text {
            "standalone" => Ok(Self::Standalone),
            "hub" => Ok(Self::Hub),
            "edge" => Ok(Self::Edge),
            other => Err(NodeRoleError {
                code: codes::UNKNOWN_ROLE,
                message: format!(
                    "services.node_role.role must be standalone, hub or edge, not `{other}`"
                ),
            }),
        }
    }
}

/// A start-up refusal: a stable code and an operator-facing message. Never a secret, a key, a
/// code or a fingerprint (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRoleError {
    /// The stable dotted code (see [`codes`]).
    pub code: &'static str,
    /// The message, safe to print and to log.
    pub message: String,
}

impl std::fmt::Display for NodeRoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for NodeRoleError {}

/// The host's own role set, mirrored from [`HOST_ROLES_ENV`] (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostRoles {
    roles: BTreeSet<String>,
    /// No host role set is known (neither `host_roles` nor `SPOT_HOST_ROLES` given): a hub or
    /// standalone is accepted (nothing to contradict) but an edge is refused (fail-closed).
    unknown: bool,
}

impl HostRoles {
    /// No role set is known: [`Self::agrees`] accepts a hub or standalone and refuses an edge.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            roles: BTreeSet::new(),
            unknown: true,
        }
    }

    /// Parses a comma-separated list of role names (`web`, `jobs`, `realtime`, `database`,
    /// `branch-edge`); blank entries are ignored. An unknown name is kept as-is (this module only
    /// asks whether `database` or `branch-edge` is present; it does not validate the host's
    /// whole role set, `spot-agent-runtime` already does).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let roles: BTreeSet<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        Self {
            roles,
            unknown: false,
        }
    }

    /// The host role set: the configured `host_roles` list when given, else [`HOST_ROLES_ENV`],
    /// else [`Self::unknown`] (an empty value counts as not given).
    #[must_use]
    pub fn resolve(configured: Option<&[String]>) -> Self {
        if let Some(list) = configured.filter(|l| l.iter().any(|r| !r.trim().is_empty())) {
            return Self::parse(&list.join(","));
        }
        match std::env::var(HOST_ROLES_ENV) {
            Ok(raw) if !raw.trim().is_empty() => Self::parse(&raw),
            _ => Self::unknown(),
        }
    }

    /// Whether the (known) set literally names `role`. `false` for an unknown set: there is
    /// nothing to hold it. [`Self::agrees`] is the check [`decide`] actually uses.
    #[must_use]
    pub fn contains(&self, role: &str) -> bool {
        self.roles.contains(role)
    }

    /// Whether this host's role set agrees with running `role` on this site: an edge needs
    /// `branch-edge` and refuses `database` (so an unknown set refuses an edge), and a hub or
    /// standalone refuses `branch-edge` (item 1 of the brief; an unknown set contradicts
    /// neither).
    #[must_use]
    pub fn agrees(&self, role: Role) -> bool {
        if self.unknown {
            return role != Role::Edge;
        }
        match role {
            Role::Edge => self.contains("branch-edge") && !self.contains("database"),
            Role::Hub | Role::Standalone => !self.contains("branch-edge"),
        }
    }
}

/// The site's existing role record, read from `spot_node_role_state` (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRole {
    /// The role recorded on this site's first start.
    pub role: Role,
    /// The hub fingerprint recorded with it, if the site ever enrolled.
    pub hub_fingerprint: Option<String>,
}

/// What this task's validation needs besides [`Settings`]: the site's own `[partition] node`
/// (absent for a single-node site), the host's role set, and the site's previously recorded
/// role, if any.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// `[partition] node`, when the site has a partition profile.
    pub partition_node: Option<String>,
    /// The host's own role set.
    pub host_roles: HostRoles,
    /// The role this site previously recorded, if it ever started before.
    pub stored: Option<StoredRole>,
    /// Whether `enrol_code_file` names a readable, non-empty file (checked once, by the
    /// caller, so this module never guesses whether a path a test gives it is real).
    pub enrol_code_present: bool,
}

/// What this site's services registration should do, once [`decide`] accepts the configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The validated role.
    pub role: Role,
    /// The node id this role runs as (`[services.node_role] node`, equal to `[partition]
    /// node` for an edge; `None` for a hub or standalone that named none).
    pub node: Option<String>,
    /// The hub fingerprint to record (edge only; `None` otherwise).
    pub hub_fingerprint: Option<String>,
    /// The readiness detail an operator sees (never a secret, a key or a fingerprint).
    pub detail: String,
}

/// Validates `settings` against `ctx` and decides the role's plan. Pure: no I/O, no clock, no
/// random source — every input is an argument, so this is the whole decision this task makes,
/// tested without a database or a process.
///
/// # Errors
/// A [`NodeRoleError`] naming one of [`codes`]; the caller stops the site build on it.
pub fn decide(settings: &Settings, ctx: &Context) -> std::result::Result<Plan, NodeRoleError> {
    let role = Role::parse(settings.role.as_deref().unwrap_or("standalone"))?;

    // An edge must carry no hub setting, and a hub or standalone must carry no edge setting:
    // the tenancy property of "an edge must not carry hub powers" is a configuration refusal,
    // not a runtime promise this task has no service to keep yet.
    let edge_only_set = settings.hub_url.is_some()
        || settings.hub_fingerprint.is_some()
        || settings.enrol_code_file.is_some();
    match role {
        Role::Edge if settings.listen.is_some() => {
            return Err(NodeRoleError {
                code: codes::ROLE_CONFLICT,
                message: "services.node_role.listen: hub-only setting; an edge must not bind a hub \
                          listener"
                    .to_owned(),
            });
        }
        Role::Hub | Role::Standalone if edge_only_set => {
            return Err(NodeRoleError {
                code: codes::ROLE_CONFLICT,
                message: "services.node_role.hub_url, hub_fingerprint and enrol_code_file are \
                          edge-only"
                    .to_owned(),
            });
        }
        _ => {}
    }

    // The host's own role set must agree (item 1 of the brief).
    if !ctx.host_roles.agrees(role) {
        return Err(NodeRoleError {
            code: codes::ROLE_CONFLICT,
            message: format!(
                "services.node_role.role is {}, which the host role set ({} or \
                 services.node_role.host_roles) does not agree with or does not state: edge \
                 requires branch-edge, refuses database and is refused when the set is \
                 unknown; hub and standalone refuse branch-edge",
                role.name(),
                HOST_ROLES_ENV
            ),
        });
    }

    let (node, hub_fingerprint, detail) = match role {
        Role::Standalone => (None, None, "sync: off (standalone)".to_owned()),
        Role::Hub => {
            let Some(listen) = settings.listen.as_deref().filter(|s| !s.is_empty()) else {
                return Err(NodeRoleError {
                    code: codes::HUB_WITHOUT_LISTENER,
                    message: "services.node_role.role is hub, which requires listen".to_owned(),
                });
            };
            (None, None, format!("sync: hub (listening on {listen})"))
        }
        Role::Edge => {
            let Some(partition_node) = ctx.partition_node.as_deref() else {
                return Err(NodeRoleError {
                    code: codes::EDGE_WITHOUT_PARTITION,
                    message: "services.node_role.role is edge, which requires a [partition] \
                              profile"
                        .to_owned(),
                });
            };
            let node = settings.node.as_deref().unwrap_or_default();
            if node.is_empty() || node != partition_node {
                return Err(NodeRoleError {
                    code: codes::NODE_ID_MISMATCH,
                    message: format!(
                        "services.node_role.node must equal [partition] node (`{partition_node}`)"
                    ),
                });
            }
            if settings.hub_url.as_deref().is_none_or(str::is_empty) {
                return Err(NodeRoleError {
                    code: codes::EDGE_WITHOUT_HUB,
                    message: "services.node_role.role is edge, which requires hub_url".to_owned(),
                });
            }
            let already_enrolled = ctx
                .stored
                .as_ref()
                .is_some_and(|s| s.hub_fingerprint.is_some());
            if !already_enrolled && !ctx.enrol_code_present {
                return Err(NodeRoleError {
                    code: codes::EDGE_NOT_ENROLLED_AND_NO_CODE,
                    message: "services.node_role.role is edge, is not yet enrolled, and names \
                              no readable enrol_code_file"
                        .to_owned(),
                });
            }
            let fingerprint = settings.hub_fingerprint.clone().or_else(|| {
                ctx.stored
                    .as_ref()
                    .and_then(|s| s.hub_fingerprint.clone())
            });
            (
                Some(node.to_owned()),
                fingerprint,
                format!("sync: edge (node={node}, offline-safe)"),
            )
        }
    };

    // A site that already recorded a role never silently becomes another one: the operation
    // that may change it is WP-RTB-04.T2's, out of scope here.
    if let Some(stored) = &ctx.stored
        && stored.role != role
    {
        return Err(NodeRoleError {
            code: codes::ROLE_CHANGED_UNSAFELY,
            message: format!(
                "this site last started as {}; services.node_role.role now names {} — stop the \
                 site and clear spot_node_role_state, or run the role-change operation, before \
                 changing it",
                stored.role.name(),
                role.name()
            ),
        });
    }

    Ok(Plan {
        role,
        node,
        hub_fingerprint,
        detail,
    })
}

/// Whether `path` names a readable file with at least one byte (an enrolment code is a secret;
/// this never reads or returns its content).
fn code_file_present(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

/// Maps a store error to the module's refusal: a missing table is `node_role.table_missing`.
fn store_error(action: &str, e: &spot_node_role_pg::NodeRoleStoreError) -> Error {
    if e.code() == spot_node_role_pg::TABLE_MISSING {
        Error(format!(
            "{}: spot_node_role_state does not exist; run `spot migrate` before serving",
            codes::TABLE_MISSING
        ))
    } else {
        Error(format!("node_role: could not {action} the role record ({})", e.code()))
    }
}

/// Reads the site's previously recorded role, if any. `None` when the store has no row for this
/// site; the row and the table's absence are distinguished by [`table_exists`].
///
/// # Errors
/// [`Error`] when the read fails or the stored role name is not one [`Role::parse`] accepts.
async fn read_stored(
    store: &spot_node_role_pg::PgNodeRoleStore,
    site: &str,
) -> Result<Option<StoredRole>> {
    let Some(row) = store.read(site).await.map_err(|e| store_error("read", &e))? else {
        return Ok(None);
    };
    let role = Role::parse(&row.role)
        .map_err(|e| Error(format!("node_role: stored role is unreadable: {e}")))?;
    Ok(Some(StoredRole {
        role,
        hub_fingerprint: row.hub_fingerprint,
    }))
}

/// Records `plan`'s role for `site` (insert on first start, update when the role is unchanged;
/// [`decide`] already refused any other case).
///
/// # Errors
/// [`Error`] when the write fails.
async fn store_role(
    store: &spot_node_role_pg::PgNodeRoleStore,
    site: &str,
    plan: &Plan,
    now: OffsetDateTime,
) -> Result<()> {
    store
        .upsert(
            site,
            plan.role.name(),
            plan.hub_fingerprint.as_deref(),
            now.unix_timestamp(),
        )
        .await
        .map_err(|e| store_error("write", &e))
}

/// Whether `spot_node_role_state` exists on `backend`'s database.
///
/// # Errors
/// [`Error`] when the schema cannot be read.
async fn table_exists(backend: &spot_postgres::PgBackend) -> Result<bool> {
    let live = spot_pg_migrate::LiveSchema::read(backend)
        .await
        .map_err(|e| Error(e.to_string()))?;
    Ok(live.tables.contains_key("spot_node_role_state"))
}

/// The readiness service: always optional, so this task's own contribution can never fail
/// `/api/health/ready` for a reason that belongs to the hub link. No network I/O: the real
/// hub-reachability probe, and the detail an operator reads as `sync_node: offline 3h`, belong
/// to `services.sync_node` (WP-RTB-02, out of scope) — this module only guarantees the
/// structural property that a stopped or never-built hub link cannot block readiness, which
/// `sync_node`'s own [`Level::Optional`] registration will inherit once it exists.
struct NodeRoleService;

impl Service for NodeRoleService {
    fn name(&self) -> &'static str {
        "node_role"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: tokio::sync::watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

/// Registers this site's node role: decides the plan, refuses an invalid or unsafely changed
/// one before anything later starts, records the role on first start, and adds the offline-safe
/// readiness service.
///
/// # Errors
/// The [`NodeRoleError`] [`decide`] returns, or an [`Error`] from the schema or state read/write.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    let host_roles = HostRoles::resolve(settings.host_roles.as_deref());
    let env = ctx.env()?;
    let site_name = env.site.site.clone();
    let partition_node = env.site.partition.as_ref().map(|p| p.node.clone());
    let backend = env.backend.clone();
    let clock = ctx.clock().clone();

    if !table_exists(&backend).await? {
        return Err(Error(format!(
            "{}: spot_node_role_state does not exist; run `spot migrate` before serving",
            codes::TABLE_MISSING
        )));
    }
    let store = spot_node_role_pg::PgNodeRoleStore::new(backend.pool());
    let stored = read_stored(&store, &site_name).await?;
    let enrol_code_present = settings
        .enrol_code_file
        .as_deref()
        .is_some_and(code_file_present);

    let decision_ctx = Context {
        partition_node,
        host_roles,
        stored,
        enrol_code_present,
    };
    let plan = decide(&settings, &decision_ctx)
        .map_err(|e| Error(format!("{}: {}", e.code, e.message)))?;

    store_role(&store, &site_name, &plan, clock.now_utc()).await?;

    tracing::info!(
        site = %site_name,
        role = plan.role.name(),
        detail = %plan.detail,
        "node_role: site role validated"
    );
    ctx.add(std::sync::Arc::new(NodeRoleService));
    Ok(())
}

/// The site's `[partition] node`, for callers that build a [`Context`] from a loaded site
/// without going through [`register`] (a command, or a test).
#[must_use]
pub fn partition_node_of(site: &crate::config::Site) -> Option<String> {
    site.partition.as_ref().map(|p| p.node.clone())
}

/// A readable, non-empty file check exposed for commands and tests that must judge an enrolment
/// code path the same way [`register`] does, without duplicating the rule.
#[must_use]
pub fn enrol_code_present(path: &Path) -> bool {
    path.to_str().is_some_and(code_file_present)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn settings(role: &str) -> Settings {
        Settings {
            role: Some(role.to_owned()),
            ..Settings::default()
        }
    }

    fn ctx() -> Context {
        Context {
            partition_node: None,
            host_roles: HostRoles::unknown(),
            stored: None,
            enrol_code_present: false,
        }
    }

    #[test]
    fn standalone_is_the_default_and_needs_nothing() {
        let plan = decide(&Settings::default(), &ctx()).unwrap();
        assert_eq!(plan.role, Role::Standalone);
        assert_eq!(plan.detail, "sync: off (standalone)");
    }

    #[test]
    fn an_unknown_role_is_refused() {
        let err = decide(&settings("branch"), &ctx()).unwrap_err();
        assert_eq!(err.code, codes::UNKNOWN_ROLE);
    }

    #[test]
    fn a_hub_without_listen_is_refused() {
        let err = decide(&settings("hub"), &ctx()).unwrap_err();
        assert_eq!(err.code, codes::HUB_WITHOUT_LISTENER);
    }

    #[test]
    fn a_hub_with_listen_starts() {
        let settings = Settings {
            listen: Some("0.0.0.0:7443".to_owned()),
            ..settings("hub")
        };
        let plan = decide(&settings, &ctx()).unwrap();
        assert_eq!(plan.role, Role::Hub);
        assert!(plan.detail.contains("hub"));
    }

    #[test]
    fn a_hub_cannot_carry_an_edge_setting() {
        let settings = Settings {
            listen: Some("0.0.0.0:7443".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            ..settings("hub")
        };
        let err = decide(&settings, &ctx()).unwrap_err();
        assert_eq!(err.code, codes::ROLE_CONFLICT);
    }

    #[test]
    fn an_edge_without_partition_is_refused() {
        let mut c = ctx();
        c.host_roles = HostRoles::parse("branch-edge");
        let err = decide(&settings("edge"), &c).unwrap_err();
        assert_eq!(err.code, codes::EDGE_WITHOUT_PARTITION);
    }

    #[test]
    fn an_edge_cannot_carry_a_hub_listener() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        let settings = Settings {
            node: Some("W1".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            listen: Some("0.0.0.0:7443".to_owned()),
            ..settings("edge")
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::ROLE_CONFLICT);
    }

    #[test]
    fn an_edge_node_id_must_equal_the_partition_node() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        let settings = Settings {
            node: Some("W9".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            ..settings("edge")
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::NODE_ID_MISMATCH);
    }

    #[test]
    fn an_edge_without_a_hub_url_is_refused() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        let settings = Settings {
            node: Some("W1".to_owned()),
            ..settings("edge")
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::EDGE_WITHOUT_HUB);
    }

    #[test]
    fn an_unenrolled_edge_with_no_code_is_refused() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        let settings = Settings {
            node: Some("W1".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            ..settings("edge")
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::EDGE_NOT_ENROLLED_AND_NO_CODE);
    }

    #[test]
    fn an_unenrolled_edge_with_a_code_file_starts() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        c.enrol_code_present = true;
        let settings = Settings {
            node: Some("W1".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            enrol_code_file: Some("/etc/spot/enrol.code".to_owned()),
            ..settings("edge")
        };
        let plan = decide(&settings, &c).unwrap();
        assert_eq!(plan.role, Role::Edge);
        assert!(plan.detail.contains("offline-safe"));
    }

    #[test]
    fn an_already_enrolled_edge_needs_no_code_file() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        c.stored = Some(StoredRole {
            role: Role::Edge,
            hub_fingerprint: Some("sha256:abc".to_owned()),
        });
        let settings = Settings {
            node: Some("W1".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            ..settings("edge")
        };
        let plan = decide(&settings, &c).unwrap();
        assert_eq!(plan.hub_fingerprint.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn the_host_role_set_must_agree_for_an_edge() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        c.enrol_code_present = true;
        c.host_roles = HostRoles::parse("web,jobs");
        let settings = Settings {
            node: Some("W1".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            enrol_code_file: Some("/etc/spot/enrol.code".to_owned()),
            ..settings("edge")
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::ROLE_CONFLICT);
    }

    #[test]
    fn the_host_role_set_refuses_branch_edge_for_a_hub() {
        let mut c = ctx();
        c.host_roles = HostRoles::parse("branch-edge");
        let settings = Settings {
            listen: Some("0.0.0.0:7443".to_owned()),
            ..settings("hub")
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::ROLE_CONFLICT);
    }

    #[test]
    fn a_host_declaring_database_refuses_an_edge_even_with_branch_edge() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        c.enrol_code_present = true;
        c.host_roles = HostRoles::parse("branch-edge,database");
        let settings = Settings {
            node: Some("W1".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            enrol_code_file: Some("/etc/spot/enrol.code".to_owned()),
            ..settings("edge")
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::ROLE_CONFLICT);
    }

    #[test]
    fn a_role_change_without_the_operation_is_refused() {
        let mut c = ctx();
        c.stored = Some(StoredRole {
            role: Role::Standalone,
            hub_fingerprint: None,
        });
        let settings = settings("hub");
        let settings = Settings {
            listen: Some("0.0.0.0:7443".to_owned()),
            ..settings
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::ROLE_CHANGED_UNSAFELY);
    }

    #[test]
    fn repeating_the_same_recorded_role_is_fine() {
        let mut c = ctx();
        c.stored = Some(StoredRole {
            role: Role::Standalone,
            hub_fingerprint: None,
        });
        let plan = decide(&Settings::default(), &c).unwrap();
        assert_eq!(plan.role, Role::Standalone);
    }

    #[test]
    fn host_roles_parses_and_ignores_blanks() {
        let roles = HostRoles::parse("web, jobs,,realtime");
        assert!(roles.contains("web"));
        assert!(roles.contains("jobs"));
        assert!(roles.contains("realtime"));
        assert!(!roles.contains("database"));
    }

    #[test]
    fn an_unknown_host_role_set_refuses_an_edge_and_accepts_the_rest() {
        let roles = HostRoles::unknown();
        assert!(roles.agrees(Role::Standalone));
        assert!(roles.agrees(Role::Hub));
        assert!(!roles.agrees(Role::Edge), "fail-closed for an edge");
        assert!(!roles.contains("branch-edge"), "unknown holds no literal set");
    }

    #[test]
    fn an_edge_with_an_unknown_host_role_set_is_refused() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.enrol_code_present = true;
        let settings = Settings {
            node: Some("W1".to_owned()),
            hub_url: Some("https://hub.example".to_owned()),
            enrol_code_file: Some("/etc/spot/enrol.code".to_owned()),
            ..settings("edge")
        };
        let err = decide(&settings, &c).unwrap_err();
        assert_eq!(err.code, codes::ROLE_CONFLICT);
    }

    #[test]
    fn the_readiness_detail_never_echoes_the_hub_url() {
        let mut c = ctx();
        c.partition_node = Some("W1".to_owned());
        c.host_roles = HostRoles::parse("branch-edge");
        c.enrol_code_present = true;
        let settings = Settings {
            node: Some("W1".to_owned()),
            hub_url: Some("https://user:token@hub.example/?k=secret".to_owned()),
            enrol_code_file: Some("/etc/spot/enrol.code".to_owned()),
            ..settings("edge")
        };
        let plan = decide(&settings, &c).unwrap();
        assert!(!plan.detail.contains("token") && !plan.detail.contains("hub.example"));
    }

    #[test]
    fn a_configured_host_roles_list_wins_over_the_environment() {
        let roles = HostRoles::resolve(Some(&["branch-edge".to_owned()]));
        assert!(roles.agrees(Role::Edge));
        assert!(!roles.agrees(Role::Hub));
    }

    #[test]
    fn host_roles_agrees_checks_the_combination() {
        let edge_host = HostRoles::parse("branch-edge");
        assert!(edge_host.agrees(Role::Edge));
        assert!(!edge_host.agrees(Role::Hub));
        assert!(!edge_host.agrees(Role::Standalone));
        let desktop_host = HostRoles::parse("web,jobs,realtime,database");
        assert!(!desktop_host.agrees(Role::Edge));
        assert!(desktop_host.agrees(Role::Hub));
        assert!(desktop_host.agrees(Role::Standalone));
    }

    #[test]
    fn role_names_round_trip() {
        for role in [Role::Standalone, Role::Hub, Role::Edge] {
            assert_eq!(Role::parse(role.name()).unwrap(), role);
        }
    }

    #[test]
    fn code_file_present_rejects_a_missing_or_empty_file() {
        let dir = std::env::temp_dir().join(format!("spot_node_role_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.code");
        assert!(!code_file_present(missing.to_str().unwrap()));
        let empty = dir.join("empty.code");
        std::fs::write(&empty, b"").unwrap();
        assert!(!code_file_present(empty.to_str().unwrap()));
        let present = dir.join("present.code");
        std::fs::write(&present, b"abc").unwrap();
        assert!(code_file_present(present.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
