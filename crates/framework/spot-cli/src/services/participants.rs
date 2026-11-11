//! The transaction participants one site installs, in the one correct order (WP-PRD-01.T11).
//!
//! A document write passes through the engine's participant seam: the audit participant records
//! the write in the same transaction, the owner check refuses a write this node does not own, and
//! edge sync derives the graph edges from the final document. Each of them exists and is tested
//! alone; this module is the **one** place that builds them for a site, so no caller can install
//! them out of order, or install one where the configuration cannot support it.
//!
//! # The order is not decided here
//!
//! [`spot_doc_audit::checked_participants`] fixes it: **audit first**, then the owner checks, then
//! everything else, **edge sync last**; each group keeps its relative order. The reasons are the
//! crate's: the audit must see the write the owner check lets through (a refused write never
//! happened, so it must not be audited as made) and must already have recorded it when a later
//! participant refuses (that refusal rolls the audit event back with the write), and edge sync
//! derives from the final document, after everything that can still refuse.
//!
//! # What refuses to start
//!
//! Every configuration the engine cannot make correct is refused here, before the site serves,
//! with a stable code:
//!
//! - `docaudit.double_registration`: a controller in the site's registry is an
//!   [`AuditStep`](spot_doc_audit::AuditStep) for a DocType the audit participant already audits,
//!   so every write would be audited twice.
//! - `partition.scope_uncovered`: `[partition] doctypes` names a partitioned DocType with no
//!   `[[partition.scope]]` entry. A partitioned DocType nobody configured is unprotected, never
//!   allowed through.
//! - `cli.partition_registry_missing`: the site names a partition node but the host holds no
//!   [`PartitionRegistry`] to check ownership against. A node profile never silently runs
//!   unpartitioned.
//!
//! A single-node site (no `[partition]` section) installs no owner check and readiness says
//! `partition: off`; nothing else changes for it.
//!
//! # The audit masker is per metadata generation
//!
//! [`Audit::new`] takes the [`FieldMasker`] of one metadata generation — in `spot-cli` a
//! [`spot_site_masker::SiteMasker`] over the metadata the site serves, built again in every
//! `build_site`. Nothing here caches it, so a metadata swap rebuilds the site and the masker with
//! it and the mask follows the metadata. Passing it in keeps this module free of the permission
//! engine and metadata snapshot the masker needs.

use std::future::Future;
use std::sync::Arc;

use spot_controllers::ControllerRegistry;
use spot_core::SiteId;
use spot_doc_audit::{
    AuditParticipant, FieldMasker, Placed, Role, checked_participants, is_audit_off,
};
use spot_document::engine::TxParticipant;
use spot_edge_pg::PgEdgeWriters;
use spot_edge_sync::{EdgeParticipant, MetaSource};
use spot_meta::EffectiveMeta;
use spot_partition::{OwnerParticipant, PartitionRegistry, ScopeFields};
use spot_postgres::PgTransaction;
use spot_sync_protocol::NodeId;

use crate::config;
use crate::{Error, Result};

/// The stable name of the audit participant, when the site has an audit chain.
pub const AUDIT: &str = "audit";
/// The stable name of the owner check, when the site runs a partitioned profile.
pub const OWNER: &str = "owner";
/// The stable name of edge sync, always last.
pub const EDGE: &str = "edge";

/// The start-up refusal of a node profile with no ownership registry to check against.
pub const PARTITION_REGISTRY_MISSING: &str = "cli.partition_registry_missing";

/// The audit participant of one metadata generation: the masker it masks through and the DocTypes
/// it audits.
pub struct Audit {
    masker: Arc<dyn FieldMasker>,
    doctypes: Vec<String>,
}

impl Audit {
    /// The audit participant of `meta`, masking through `masker`.
    ///
    /// The DocTypes it audits are the ones `meta` holds, less the audit's own (the crate's
    /// `AUDIT_OFF_DOCTYPES`: chaining its own chain would not terminate).
    #[must_use]
    pub fn new(meta: &EffectiveMeta, masker: Arc<dyn FieldMasker>) -> Self {
        let doctypes = meta
            .doctypes()
            .map(|view| view.name.as_str().to_owned())
            .filter(|name| !is_audit_off(name))
            .collect();
        Self { masker, doctypes }
    }

    /// The masker of this generation.
    #[must_use]
    pub fn masker(&self) -> &Arc<dyn FieldMasker> {
        &self.masker
    }

    /// The DocTypes this generation audits.
    #[must_use]
    pub fn doctypes(&self) -> &[String] {
        &self.doctypes
    }
}

/// What the participants are built from besides the site's own configuration.
pub struct Deps {
    /// The audit participant, when the site has an audit chain to write into. `None` installs no
    /// audit participant, and readiness says `docaudit: off`.
    pub audit: Option<Audit>,
    /// The site's controllers: `checked_participants` looks for an audit step among them.
    pub controllers: Arc<ControllerRegistry>,
    /// The site's ownership registry, when it runs a partitioned profile: the host's
    /// `services::partition` registry, which answers `map_at` from a snapshot refreshed off the
    /// request path (the store itself would cross the blocking bridge inside the write's own
    /// request). A single-node site passes `None`; a unit test may pass any implementation.
    pub partitions: Option<Arc<dyn PartitionRegistry>>,
    /// Where edge sync reads the effective metadata: the same live handle the engine serves.
    pub meta: Arc<dyn MetaSource>,
}

/// The participants a site installs, with their names in the order they run.
pub struct Installed {
    /// The participant list, ready for `Engine::set_participants`.
    pub participants: Vec<Arc<dyn TxParticipant<PgTransaction>>>,
    /// The stable name of each participant, in the same order as `participants`.
    pub names: Vec<&'static str>,
    /// The metadata source edge sync reads, for a caller that wants to prove which one it got.
    pub meta: Arc<dyn MetaSource>,
}

impl std::fmt::Debug for Installed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Installed")
            .field("names", &self.names)
            .finish_non_exhaustive()
    }
}

impl Installed {
    /// The participants' names, in order, as one line: `audit, owner, edge`.
    #[must_use]
    pub fn names_in_order(&self) -> String {
        self.names.join(", ")
    }
}

/// Builds the participants of `site`, in the order the engine requires.
///
/// # Errors
///
/// `docaudit.double_registration`, `partition.scope_uncovered` or
/// `cli.partition_registry_missing`, each with the stable code first and the site's own values
/// (a DocType name, a node id) after it — never a document value.
pub fn build(site: &SiteId, config: &config::Site, deps: &Deps) -> Result<Installed> {
    let mut placed: Vec<(Role, Arc<dyn TxParticipant<PgTransaction>>)> = Vec::new();
    let mut names: Vec<&'static str> = Vec::new();
    if let Some(owner) = owner_check(config, deps)? {
        placed.push((Role::OwnerCheck, owner));
        names.push(OWNER);
    }
    // Edge sync derives the edges from the document every other participant agreed to, so it is
    // the last one to run.
    let edge: Arc<dyn TxParticipant<PgTransaction>> = Arc::new(
        EdgeParticipant::<PgTransaction, PgEdgeWriters>::new(deps.meta.clone(), PgEdgeWriters)
            .for_site(site.clone()),
    );
    placed.push((Role::EdgeSync, edge));
    names.push(EDGE);

    let participants = match &deps.audit {
        Some(audit) => {
            let participant = AuditParticipant::new(audit.masker.clone())
                .with_doctypes(audit.doctypes.iter().cloned());
            names.insert(0, AUDIT);
            checked_participants(
                participant,
                placed
                    .into_iter()
                    .map(|(role, participant)| Placed::new(role, participant))
                    .collect(),
                deps.controllers.as_ref(),
            )
            .map_err(|e| Error(format!("{}: {e}", e.code())))?
        }
        // No audit participant: the rest keep the same relative order, which is what
        // `checked_participants` would give them beside an audit participant.
        None => {
            placed.sort_by_key(|(role, _)| rank(*role));
            placed
                .into_iter()
                .map(|(_, participant)| participant)
                .collect()
        }
    };
    Ok(Installed {
        participants,
        names,
        meta: deps.meta.clone(),
    })
}

/// The owner check of a node profile, or `None` for a single-node site.
fn owner_check(
    config: &config::Site,
    deps: &Deps,
) -> Result<Option<Arc<dyn TxParticipant<PgTransaction>>>> {
    let Some(profile) = config.partition.as_ref() else {
        return Ok(None);
    };
    let Some(registry) = deps.partitions.clone() else {
        return Err(Error(format!(
            "{PARTITION_REGISTRY_MISSING}: site `{}` names partition node `{}` but the host holds \
             no partition registry, so no write could be checked against ownership; never run a \
             node profile unpartitioned",
            config.site, profile.node
        )));
    };
    let node = NodeId::new(&profile.node)
        .map_err(|e| Error(format!("partition.node `{}`: {e}", profile.node)))?;
    let mut fields = ScopeFields::new();
    for entry in &profile.scope {
        fields = fields.with(&entry.doctype, &entry.key, &entry.field);
    }
    let doctypes: Vec<&str> = profile.doctypes.iter().map(String::as_str).collect();
    let participant = OwnerParticipant::checked(node, registry, fields, &doctypes)
        .map_err(|e| Error(format!("{}: {e}", e.code())))?;
    Ok(Some(Arc::new(participant)))
}

/// Where a role sits in the order `checked_participants` enforces.
fn rank(role: Role) -> u8 {
    match role {
        Role::Audit => 0,
        Role::OwnerCheck => 1,
        Role::Other => 2,
        Role::EdgeSync => 3,
    }
}

/// What `/api/health/ready` says about one site's participants: which are installed, and whether
/// the site is partitioned.
///
/// Readiness only reports; a site that cannot check ownership refuses to start rather than serving
/// weaker, so this check is always `ok`. The audit chain's lag is **not** reported here yet: the
/// site has no audit chain to measure (see the task's open questions).
pub struct ParticipantsReady {
    name: String,
    detail: String,
}

impl ParticipantsReady {
    /// The check of `site`, from the partition profile it runs and the participants it installed.
    #[must_use]
    pub fn new(
        site: &str,
        partition: Option<&config::Partition>,
        installed: &[&'static str],
    ) -> Self {
        // Two shapes and no third: a single-node site says `off`, a node says which node it is
        // and how many DocTypes it owns a scope for. Neither carries an assignment.
        let partition = match partition {
            Some(profile) => format!(
                "partition: node {}, {} DocTypes",
                profile.node,
                profile.doctypes.len()
            ),
            None => "partition: off".to_owned(),
        };
        let audit = if installed.contains(&AUDIT) {
            "docaudit: on"
        } else {
            "docaudit: off"
        };
        let participants = if installed.is_empty() {
            "none".to_owned()
        } else {
            installed.join(", ")
        };
        Self {
            name: format!("participants:{site}"),
            detail: format!("{partition}; {audit}; installed: {participants}"),
        }
    }

    /// What the check renders (for tests and for the operator's log).
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl spot_http::health::ReadinessCheck for ParticipantsReady {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = spot_http::health::CheckResult> + Send + '_>> {
        let detail = self.detail.clone();
        Box::pin(async move {
            let mut result = spot_http::health::CheckResult::ok();
            result.detail = detail;
            result
        })
    }
}
