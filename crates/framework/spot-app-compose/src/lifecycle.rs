//! Uninstall and upgrade safety: a provider with dependents is not removed out from under them,
//! and an upgrade says exactly which dependents it impacts.
//!
//! These functions are pure: they read the installed apps and the proposed new exports and return
//! a plan or a refusal. The install planner ([`spot-installer`](https://docs.rs/spot-installer))
//! consumes the resulting data; this crate does not depend on it.

use std::collections::BTreeSet;

use semver::{Version, VersionReq};

use crate::interface::{AppInterfaces, ExportedInterface};
use crate::problem::{Problem, Problems, codes};

/// The apps that import an interface `app` exports, in name order.
fn dependents_of<'a>(installed: &'a [AppInterfaces], app: &str) -> Vec<&'a AppInterfaces> {
    let Some(target) = installed.iter().find(|candidate| candidate.app == app) else {
        return Vec::new();
    };
    let exported: BTreeSet<&str> = target
        .exports
        .iter()
        .map(|export| export.interface.as_str())
        .collect();
    let mut out: Vec<&AppInterfaces> = installed
        .iter()
        .filter(|candidate| {
            candidate.app != app
                && candidate
                    .imports
                    .iter()
                    .any(|import| exported.contains(import.interface.as_str()))
        })
        .collect();
    out.sort_by(|a, b| a.app.cmp(&b.app));
    out
}

/// What an uninstall would do, when it is allowed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UninstallPlan {
    /// The app being removed.
    pub app: String,
    /// The apps that import an interface it exports, in name order.
    pub dependents: Vec<String>,
    /// The replacement app whose exports satisfy the dependents, when one was given.
    pub replacement: Option<String>,
}

/// Plans the removal of `app`, optionally taking `replacement` in its place.
///
/// # Errors
///
/// `app_compose.unknown_app` when `app` is not installed; `app_compose.uninstall_has_dependents`
/// when it has dependents and no replacement was given; `app_compose.replacement_incomplete` for
/// each dependent import the replacement does not satisfy.
pub fn uninstall(
    installed: &[AppInterfaces],
    app: &str,
    replacement: Option<&AppInterfaces>,
) -> Result<UninstallPlan, Problems> {
    if !installed.iter().any(|candidate| candidate.app == app) {
        return Err(Problems::new(vec![Problem::new(
            codes::UNKNOWN_APP,
            format!("app `{app}` is not installed, so it cannot be uninstalled"),
            &[app],
        )]));
    }
    let dependents = dependents_of(installed, app);
    let names: Vec<String> = dependents.iter().map(|d| d.app.clone()).collect();
    if dependents.is_empty() {
        return Ok(UninstallPlan {
            app: app.to_owned(),
            dependents: names,
            replacement: None,
        });
    }
    let Some(replacement) = replacement else {
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        return Err(Problems::new(vec![Problem::new(
            codes::UNINSTALL_HAS_DEPENDENTS,
            format!(
                "app `{app}` cannot be uninstalled while {} depends on it; remove them first or \
                 name a replacement",
                names.join(", ")
            ),
            &refs,
        )]));
    };

    let mut problems = Vec::new();
    for dependent in &dependents {
        for import in &dependent.imports {
            let satisfied = replacement.exports.iter().any(|export| {
                export.interface == import.interface
                    && import.version_range.matches(&export.version)
            });
            if !satisfied {
                problems.push(Problem::new(
                    codes::REPLACEMENT_INCOMPLETE,
                    format!(
                        "replacement `{}` does not satisfy `{}`'s import of `{}` {}",
                        replacement.app, dependent.app, import.interface, import.version_range
                    ),
                    &[&replacement.app, &dependent.app],
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(UninstallPlan {
            app: app.to_owned(),
            dependents: names,
            replacement: Some(replacement.app.clone()),
        })
    } else {
        Err(Problems::new(problems))
    }
}

/// One dependent an upgrade impacts: an import the new exports no longer satisfy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImpactedDependent {
    /// The dependent app.
    pub app: String,
    /// The interface it imports.
    pub interface: String,
    /// The range it needs.
    pub needed: VersionReq,
    /// The highest version the new exports offer for the interface, if any.
    pub offered: Option<Version>,
}

/// What an upgrade would impact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradePlan {
    /// The app being upgraded.
    pub app: String,
    /// The dependents whose imports the new exports no longer satisfy, sorted.
    pub impacted: Vec<ImpactedDependent>,
    /// Every dependent, sorted, whether impacted or not: the upgrade's blast radius.
    pub blast_radius: Vec<String>,
}

impl UpgradePlan {
    /// Whether the upgrade leaves every dependent satisfied.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.impacted.is_empty()
    }
}

/// Plans an upgrade of `app` to a version exporting `new_exports`.
///
/// # Errors
///
/// `app_compose.unknown_app` when `app` is not installed. An impacted dependent is **reported**,
/// not refused: the installer decides whether to upgrade the dependent too or block.
pub fn upgrade(
    installed: &[AppInterfaces],
    app: &str,
    new_exports: &[ExportedInterface],
) -> Result<UpgradePlan, Problems> {
    if !installed.iter().any(|candidate| candidate.app == app) {
        return Err(Problems::new(vec![Problem::new(
            codes::UNKNOWN_APP,
            format!("app `{app}` is not installed, so it cannot be upgraded"),
            &[app],
        )]));
    }
    let dependents = dependents_of(installed, app);
    let current_exports: BTreeSet<&str> = installed
        .iter()
        .find(|candidate| candidate.app == app)
        .map(|target| {
            target
                .exports
                .iter()
                .map(|export| export.interface.as_str())
                .collect()
        })
        .unwrap_or_default();
    let mut impacted: Vec<ImpactedDependent> = Vec::new();
    for dependent in &dependents {
        for import in &dependent.imports {
            // Only the imports that this app's exports answer are affected.
            if !current_exports.contains(import.interface.as_str()) {
                continue;
            }
            let satisfied = new_exports.iter().any(|export| {
                export.interface == import.interface
                    && import.version_range.matches(&export.version)
            });
            if !satisfied {
                let offered = new_exports
                    .iter()
                    .filter(|export| export.interface == import.interface)
                    .map(|export| export.version.clone())
                    .max();
                impacted.push(ImpactedDependent {
                    app: dependent.app.clone(),
                    interface: import.interface.clone(),
                    needed: import.version_range.clone(),
                    offered,
                });
            }
        }
    }
    impacted.sort_by(|a, b| (&a.app, &a.interface).cmp(&(&b.app, &b.interface)));
    Ok(UpgradePlan {
        app: app.to_owned(),
        impacted,
        blast_radius: dependents.iter().map(|d| d.app.clone()).collect(),
    })
}
