//! The native controllers of a site's installed apps, and the named permissions they declare.
//!
//! # Controllers
//!
//! Every installed app whose manifest names `[app] controller = "native:<name>"` registers its
//! controllers through [`spot_controllers::ControllerRegistry`], one per DocType (the main one
//! and the companions `native:<app>.<suffix>` the table of [`crate::natives`] holds). The set
//! installs into one [`spot_seams::Registry`] for the engine's pipeline steps, and the same
//! controllers answer whitelisted method calls. A conflict (two apps ordering one step the same
//! way, a method two apps declare) refuses the site at build time and names every one of them.
//!
//! # App permissions
//!
//! [`app_permissions`] builds the site's [`AppPerms`] from the installed apps: the catalogue
//! from every manifest's `[[permissions]]` (a name declared twice fails the second install, so
//! the site is refused) and the roles' grants from `permissions/app_permissions.toml` of each
//! app. Both go to every permission snapshot of the site. [`degradation`] words the other
//! direction at assembly: a method that requires a permission no installed app declares is
//! listed, the site degrades and readiness says `perm.app.unknown` with the method and the
//! name (every build of the site records it again, so a reload that fixes the manifest clears it). Such a method still runs for the system principal and for nobody else, because the
//! decision core denies an unknown name.

use std::path::Path;
use std::sync::Arc;

use spot_app_api::{Controller, MethodRight};
use spot_controllers::{ControllerFactory, ControllerRef, ControllerRegistry};
use spot_meta::EffectiveMeta;
use spot_permissions::{AppPermissionName, AppReason};

use crate::appdir::{self, AppSource};
use crate::config::{Loaded, Site};
use crate::permissions::AppPerms;
use crate::{Error, Result, apps, natives};

/// The file of an app's directory that grants named permissions to roles.
pub const GRANTS_FILE: &str = "permissions/app_permissions.toml";

/// The installed apps of `site` that the catalogue carries, in install order. The embedded
/// platform app has no manifest and is not among them.
pub fn installed(root: &Path, loaded: &Loaded, site: &Site) -> Result<Vec<AppSource>> {
    let catalogue = apps::catalogue(root, loaded)?;
    if site.apps.is_empty() {
        return Ok(catalogue);
    }
    let mut found = Vec::new();
    for name in &site.apps {
        if let Some(position) = catalogue.iter().position(|source| &source.name == name) {
            found.push(position);
        }
    }
    let mut catalogue: Vec<Option<AppSource>> = catalogue.into_iter().map(Some).collect();
    Ok(found
        .into_iter()
        .filter_map(|position| catalogue.get_mut(position).and_then(Option::take))
        .collect())
}

/// The app id the platform declares its own permissions under.
pub const PLATFORM_APP: &str = appdir::PLATFORM;

/// Why a declaration was not taken into the catalogue.
#[derive(Debug)]
pub enum DeclareError {
    /// The name is not a valid app permission name.
    Invalid(spot_permissions::AppNameError),
    /// The name is already declared; the message names the permission and both apps.
    Duplicate(String),
}

/// Declares one app permission into `catalogue` for `site`: the single place where a manifest's
/// name is mapped and a duplicate (an app against another app or against the platform) is refused.
///
/// # Errors
///
/// [`DeclareError`]; the catalogue is unchanged.
pub fn declare_app_permission(
    catalogue: &mut spot_permissions::AppPermissionCatalogue,
    site: &str,
    name: &str,
    decl: spot_permissions::AppPermissionDecl,
) -> std::result::Result<(), DeclareError> {
    let parsed = AppPermissionName::parse(name).map_err(DeclareError::Invalid)?;
    catalogue
        .insert(parsed, decl)
        .map_err(|e| DeclareError::Duplicate(format!("site `{site}`: {e}")))
}

/// The permissions the platform itself declares: process publishing, reading versions,
/// migration and supervision, and publishing a version with a service-principal Document node
/// (the platform permission `process.publish_service_principal`; the named-permission grammar
/// has no dot, see `spot_bpm_documents::engine_permission_name`). Each is declared by the
/// platform app. No role holds any of them by default: a grant is a site decision.
#[must_use]
pub fn platform_permissions() -> Vec<(AppPermissionName, spot_permissions::AppPermissionDecl)> {
    const PERMISSIONS: [(&str, &str, &str); 5] = [
        (
            "process_publish",
            "Publish processes",
            "Publish a new version of a process.",
        ),
        (
            "process_read_versions",
            "Read process versions",
            "Read the published versions of a process.",
        ),
        (
            "process_migrate",
            "Migrate process instances",
            "Move running process instances to another version.",
        ),
        (
            "process_supervise",
            "Supervise processes",
            "See and intervene in running process instances.",
        ),
        (
            "process_publish_service_principal",
            "Publish processes acting as a service principal",
            "Publish a process version that has a Document node run as a service principal.",
        ),
    ];
    PERMISSIONS
        .iter()
        .filter_map(|(name, title, description)| {
            AppPermissionName::parse(name).ok().map(|name| {
                (
                    name,
                    spot_permissions::AppPermissionDecl {
                        app: PLATFORM_APP.to_owned(),
                        title: (*title).to_owned(),
                        description: (*description).to_owned(),
                    },
                )
            })
        })
        .collect()
}

/// Declares [`platform_permissions`] into an empty-or-not `catalogue`, before any app's.
pub fn declare_platform_permissions(catalogue: &mut spot_permissions::AppPermissionCatalogue) {
    for (name, decl) in platform_permissions() {
        // A duplicate here would mean the catalogue already holds a platform name; it is kept.
        let _ = catalogue.insert(name, decl);
    }
}

/// The site's app permission catalogue and the grants of it, from the installed apps.
///
/// # Errors
///
/// Every problem at once: a permission two apps declare, an unreadable or invalid grants file.
pub fn app_permissions(root: &Path, loaded: &Loaded, site: &Site) -> Result<AppPerms> {
    let mut perms = AppPerms::default();
    let mut errors = Vec::new();
    // The platform's own permissions go in first: an installed app that declares one of the
    // names is then refused as a duplicate and can never shadow it.
    declare_platform_permissions(&mut perms.catalogue);
    for source in installed(root, loaded, site)? {
        match appdir::manifest(&source.dir) {
            Ok(manifest) => {
                for (name, decl) in manifest.app_permissions() {
                    // The manifest crate validates a name by the catalogue's own rules but
                    // may not depend on it, so the host maps it one to one.
                    match declare_app_permission(
                        &mut perms.catalogue,
                        &site.site,
                        name.as_str(),
                        spot_permissions::AppPermissionDecl {
                            app: decl.app,
                            title: decl.title,
                            description: decl.description,
                        },
                    ) {
                        Ok(()) => {}
                        Err(e) => errors.push(match e {
                            DeclareError::Invalid(e) => format!("app `{}`: {e}", source.name),
                            DeclareError::Duplicate(e) => e,
                        }),
                    }
                }
            }
            Err(e) => errors.push(e.to_string()),
        }
        let path = source.dir.join(GRANTS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => match spot_permissions::load_app_grants(&text) {
                Ok(grants) => perms.grants.extend(grants),
                Err(list) => errors.extend(list.iter().map(|e| format!("{}: {e}", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    if errors.is_empty() {
        perms.grants.sort();
        perms.grants.dedup();
        Ok(perms)
    } else {
        Err(Error(errors.join("\n")))
    }
}

/// A method that requires a permission no installed app declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unknown {
    /// The DocType the method belongs to.
    pub doctype: String,
    /// The method.
    pub method: String,
    /// The permission it names.
    pub permission: String,
}

/// The controllers of one site, installed.
pub struct Installed {
    /// The pipeline steps of every controller, ready for the engine.
    pub seams: Arc<spot_seams::Registry>,
    /// The controllers that answer whitelisted method calls.
    pub controllers: Vec<Arc<dyn Controller>>,
    /// Methods whose required permission no installed app declares.
    pub unknown: Vec<Unknown>,
    /// The registry the steps and methods were installed from. The participant installer reads
    /// it to refuse an audit step beside the audit participant (WP-PRD-01.T11).
    pub registry: ControllerRegistry,
}

/// Installs `set` into a strict platform registry ([`crate::platform_registry_builder`]), so a
/// step that declares an unknown effect class refuses the start-up with `effects.unknown_class`
/// instead of silently running without the effect checks.
///
/// # Errors
///
/// Every conflict the registry build finds, at once, with its code.
pub fn install_strict(set: &spot_controllers::ControllerSet) -> Result<Arc<spot_seams::Registry>> {
    let mut builder = crate::platform_registry_builder()?;
    set.install_into(&mut builder);
    builder
        .build()
        .map(Arc::new)
        .map_err(|conflicts| Error(format!("conflict: {conflicts:?}")))
}

/// Registers the native controllers of the installed apps and installs them.
///
/// # Errors
///
/// A controller the binary does not contain, two controllers for one DocType, a step or method
/// conflict: all of them, so one start-up reports the whole problem.
pub fn build(
    root: &Path,
    loaded: &Loaded,
    site: &Site,
    meta: &EffectiveMeta,
    perms: &AppPerms,
) -> Result<Installed> {
    let factory = natives::table();
    let mut registry = ControllerRegistry::new();
    let mut errors = Vec::new();
    for source in installed(root, loaded, site)? {
        let reference = match appdir::controller_reference(&source.dir) {
            Ok(Some(reference)) => reference,
            Ok(None) => continue,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        let registered = (|| -> std::result::Result<(), String> {
            let manifest = appdir::manifest(&source.dir).map_err(|e| e.to_string())?;
            let mut references = vec![ControllerRef::parse(&reference).map_err(|e| e.to_string())?];
            references.extend(factory.companions(&source.name));
            registry
                .register_from_manifest_refs(&manifest, &references, &factory)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })();
        if let Err(e) = registered {
            errors.push(format!("app `{}`: {e}", source.name));
        }
    }
    if !errors.is_empty() {
        return Err(Error(errors.join("\n")));
    }
    let set = registry.set();
    let seams = install_strict(&set)
        .map_err(|e| Error(format!("site `{}`: controllers: {}", site.site, e.0)))?;
    let mut controllers: Vec<Arc<dyn Controller>> = Vec::new();
    let mut unknown = Vec::new();
    for view in meta.doctypes() {
        for registered in set.controllers_for(view.name.as_str()) {
            for decl in registered.controller.methods() {
                if let MethodRight::Permission(name) = &decl.right {
                    let declared = AppPermissionName::parse(name.as_str())
                        .is_ok_and(|parsed| perms.catalogue.contains(&parsed));
                    if !declared {
                        unknown.push(Unknown {
                            doctype: view.name.as_str().to_owned(),
                            method: decl.name.as_str().to_owned(),
                            permission: name.as_str().to_owned(),
                        });
                    }
                }
            }
            controllers.push(Arc::clone(&registered.controller));
        }
    }
    Ok(Installed {
        seams,
        controllers,
        unknown,
        registry,
    })
}

/// What readiness says about methods whose permission nobody declares: one sentence each,
/// naming the code, the method and the permission. Empty when every name is declared.
pub fn degradation(unknown: &[Unknown]) -> Vec<String> {
    unknown
        .iter()
        .map(|u| {
            format!(
                "{}: method `{}` of `{}` requires permission `{}`, which no installed app declares",
                AppReason::Unknown.code(),
                u.method,
                u.doctype,
                u.permission
            )
        })
        .collect()
}
