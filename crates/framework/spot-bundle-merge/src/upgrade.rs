//! Checking a bundle against a platform upgrade, reusing `spot-migration-conflicts` (WP-MIG-03).
//!
//! The release plan (WP-MIG-01) says what an upgrade changes; `spot-migration-conflicts` finds
//! what those changes break in a site's customisations. This module turns a bundle's customisations
//! back into that crate's [`ReleaseContext`] and reports its items as this crate's
//! [`ConflictKind::Upgrade`] conflicts, so one report carries both a bundle-versus-bundle merge and
//! a bundle-versus-release review.
//!
//! `new_meta` is the new **platform** metadata the release publishes (the app definitions and the
//! metadata version), before this site's customisations are merged over it: that is what
//! `spot-migration-conflicts` wants as its `new_standard` — a custom field the release's own
//! metadata now declares is the collision it reports. The bundle supplies the site's side.
//!
//! The check is behind the [`UpgradeCheck`] trait so a caller — or a test — can supply its own
//! implementation; [`MigrationConflicts`] is the production one that calls
//! `spot_migration_conflicts::detect`.

use spot_custom_bundle::{Bundle, ScriptRecord, SectionRegistry};
use spot_meta::{Definition, EffectiveMeta};
use spot_migration_conflicts::{
    ArtefactRef, ConflictItem, ReleaseContext, SiteFixture, SiteRule, SiteScript,
};
use spot_migration_plan::{ReleasePlan, Risk};

use crate::error::BundleMergeError;
use crate::items::bundle_site;
use crate::model::{Conflict, ConflictKind, ItemVersion, UpgradeRef, Variants};

/// A check of a bundle against a platform upgrade.
pub trait UpgradeCheck {
    /// The upgrade conflicts a bundle has, in a deterministic order.
    fn conflicts(
        &self,
        bundle: &Bundle,
        plan: &ReleasePlan,
        new_meta: &EffectiveMeta,
    ) -> Result<Vec<Conflict>, BundleMergeError>;
}

/// The production [`UpgradeCheck`]: `spot-migration-conflicts` (WP-MIG-03).
pub struct MigrationConflicts;

impl UpgradeCheck for MigrationConflicts {
    fn conflicts(
        &self,
        bundle: &Bundle,
        plan: &ReleasePlan,
        new_meta: &EffectiveMeta,
    ) -> Result<Vec<Conflict>, BundleMergeError> {
        let registry = SectionRegistry::new();
        let site = bundle_site(bundle, &registry)?;
        let customisations = site.customisations().clone();
        let new_standard: &[Definition] = new_meta.definitions();
        let scripts: Vec<SiteScript> = site.scripts().iter().map(site_script).collect();
        let rules: Vec<SiteRule> = site
            .rules()
            .iter()
            .map(|rule| SiteRule {
                name: rule.name.clone(),
                doctype: None,
                expression: rule.expression.clone(),
            })
            .collect();
        let fixtures: Vec<SiteFixture> = site
            .fixtures()
            .iter()
            .map(|set| SiteFixture {
                doctype: set.doctype.clone(),
                documents: set.documents.clone(),
            })
            .collect();
        let context = ReleaseContext::new(plan, new_standard, new_meta, &customisations)
            .with_views(site.layers(), &[])
            .with_scripts(&scripts)
            .with_rules(&rules)
            .with_fixtures(&fixtures);
        let report = spot_migration_conflicts::detect(&context);
        Ok(report.items.iter().map(upgrade_conflict).collect())
    }
}

/// Checks a bundle against a platform upgrade with the production [`MigrationConflicts`] checker.
///
/// This is the brief's `check_against_upgrade(bundle, release_plan, new_meta)`: it reuses
/// `spot-migration-conflicts` (MIG-03) through [`UpgradeCheck`] and returns its items as
/// [`ConflictKind::Upgrade`] conflicts.
pub fn check_against_upgrade(
    bundle: &Bundle,
    plan: &ReleasePlan,
    new_meta: &EffectiveMeta,
) -> Result<Vec<Conflict>, BundleMergeError> {
    MigrationConflicts.conflicts(bundle, plan, new_meta)
}

/// A bundle script as `spot-migration-conflicts` reads it.
///
/// The bundle records a script's hook point as `doctype.point` (the convention
/// `spot-custom-bundle`'s `FakeSite` and capture use); the DocType is the part before the first
/// dot, and the manifest hook point the rest.
fn site_script(record: &ScriptRecord) -> SiteScript {
    let (doctype, hook_point) = match record.hook_point.split_once('.') {
        Some((doctype, point)) => (Some(doctype.to_owned()), point.to_owned()),
        None => (None, record.hook_point.clone()),
    };
    SiteScript {
        name: record.name.clone(),
        app: record.app.clone(),
        doctype,
        hook_point,
        source: record.source.clone(),
    }
}

/// A `spot-migration-conflicts` item as this crate's upgrade conflict.
fn upgrade_conflict(item: &ConflictItem) -> Conflict {
    let pointer = ArtefactRef::pointer;
    let primary_key = item
        .artefacts
        .first()
        .map(pointer)
        .unwrap_or_else(|| item.cause.release_item.clone());
    let upgrade_version = |key: String| ItemVersion {
        section: "upgrade".to_owned(),
        key,
        base: None,
        ours: None,
        theirs: None,
    };
    let related = item
        .artefacts
        .iter()
        .skip(1)
        .map(|artefact| upgrade_version(artefact.pointer()))
        .collect();
    Conflict {
        id: item.id.clone(),
        kind: ConflictKind::Upgrade,
        primary: upgrade_version(primary_key),
        related,
        explanation: format!(
            "{} — {} ({})",
            item.cause.effect, item.resolution, item.code
        ),
        variants: Variants::default(),
        upgrade: Some(UpgradeRef {
            code: item.code.to_owned(),
            category: item.category.slug().to_owned(),
            risk: risk_name(item.risk).to_owned(),
            release_item: item.cause.release_item.clone(),
            artefacts: item.artefacts.iter().map(pointer).collect(),
        }),
    }
}

/// The lower-case name of a plan risk.
fn risk_name(risk: Risk) -> &'static str {
    match risk {
        Risk::Additive => "additive",
        Risk::NeedsReview => "needs_review",
        Risk::Destructive => "destructive",
    }
}
