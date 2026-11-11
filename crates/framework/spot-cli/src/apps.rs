//! Which apps a site hosts and the effective metadata they produce together.
use crate::appdir::{self, AppSource, PLATFORM};
use crate::config::{Loaded, Site};
use crate::{Error, Result};
use spot_meta::{Definition, EffectiveMeta, Registry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Whether `name` is usable as an app name in a site file.
pub fn app_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && name.as_bytes()[0].is_ascii_lowercase()
}

/// The directories in `common.toml` `apps` that carry an `app.toml`, validated.
///
/// Directories without an `app.toml` (metadata-only directories hosted by legacy sites) are
/// not part of the catalogue. Every invalid app is reported together.
pub fn catalogue(root: &Path, loaded: &Loaded) -> Result<Vec<AppSource>> {
    let mut apps = Vec::new();
    let mut errors = Vec::new();
    for dir in &loaded.common.apps {
        let dir = root.join(dir);
        if !dir.join("app.toml").is_file() {
            continue;
        }
        match appdir::load(&dir) {
            Ok(app) => {
                if apps.iter().any(|a: &AppSource| a.name == app.name) {
                    errors.push(format!("{}: duplicate app `{}`", dir.display(), app.name));
                } else {
                    apps.push(app);
                }
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
    if errors.is_empty() {
        Ok(apps)
    } else {
        Err(Error(errors.join("\n")))
    }
}

/// Finds an app to install: a manifest name in the catalogue, or a directory that the catalogue
/// already lists. Anything else is refused with the way to fix it.
pub fn resolve(root: &Path, loaded: &Loaded, wanted: &str) -> Result<AppSource> {
    let known = |dir: &Path| {
        loaded
            .common
            .apps
            .iter()
            .any(|a| same_dir(&root.join(a), dir))
    };
    let as_path = [PathBuf::from(wanted), root.join(wanted)]
        .into_iter()
        .find(|p| p.join("app.toml").is_file());
    if let Some(path) = as_path {
        if !known(&path) {
            return Err(Error(format!(
                "{} is not listed in common.toml `apps`; add it there first, then install it by name",
                path.display()
            )));
        }
        return appdir::load(&path);
    }
    if !app_name(wanted) {
        return Err(Error(format!("no app directory `{wanted}`")));
    }
    let found = catalogue(root, loaded)?
        .into_iter()
        .find(|a| a.name == wanted);
    found.ok_or_else(|| {
        Error(format!(
            "app `{wanted}` is not in the catalogue: list its directory in common.toml `apps`"
        ))
    })
}

/// Whether `dir` is one of the directories `common.toml` `apps` lists.
pub fn is_listed(root: &Path, loaded: &Loaded, dir: &Path) -> bool {
    loaded
        .common
        .apps
        .iter()
        .any(|a| same_dir(&root.join(a), dir))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The effective metadata of `site`, for the given installed `apps` (`platform` first).
///
/// The platform app comes from the `common.toml` catalogue when `apps/platform` is listed there
/// (the complete app, including the DocTypes other apps need, such as `file`), and otherwise
/// from the copy embedded in the binary.
pub fn effective(
    root: &Path,
    loaded: &Loaded,
    _site: &Site,
    apps: &[String],
) -> Result<Arc<EffectiveMeta>> {
    if apps.is_empty() {
        return crate::metadata(root, loaded);
    }
    let definitions = installed_definitions_of(root, loaded, apps)?;
    Registry::new(definitions)
        .map(|r| r.effective())
        .map_err(|errors| {
            Error(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
}

/// The definitions of the apps `site` has installed, platform first, exactly as the app
/// directories carry them (runtime customisations are applied on top by the metadata service).
pub fn installed_definitions(root: &Path, loaded: &Loaded, site: &Site) -> Result<Vec<Definition>> {
    if site.apps.is_empty() {
        // A host-wide site hosts every directory in `apps`, with no install list.
        let mut definitions = Vec::new();
        for app in &loaded.common.apps {
            let mut entries: Vec<_> = std::fs::read_dir(root.join(app).join("doctypes"))?
                .collect::<std::io::Result<_>>()?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                definitions.push(
                    Definition::parse(
                        &path.display().to_string(),
                        &std::fs::read_to_string(&path)?,
                    )
                    .map_err(|e| Error(e.to_string()))?,
                );
            }
        }
        return Ok(definitions);
    }
    installed_definitions_of(root, loaded, &site.apps)
}

fn installed_definitions_of(
    root: &Path,
    loaded: &Loaded,
    apps: &[String],
) -> Result<Vec<Definition>> {
    let mut definitions: Vec<Definition> = Vec::new();
    let mut missing = Vec::new();
    let catalogue = catalogue(root, loaded)?;
    for app in apps {
        if app == PLATFORM && !catalogue.iter().any(|a| a.name == PLATFORM) {
            // Not listed in common.toml: the platform app embedded in this binary.
            definitions.extend(appdir::platform_definitions()?);
        } else if let Some(source) = catalogue.iter().find(|a| &a.name == app) {
            definitions.extend(source.definitions.clone());
        } else {
            missing.push(format!(
                "installed app `{app}` is not in common.toml `apps`"
            ));
        }
    }
    if !missing.is_empty() {
        return Err(Error(missing.join("\n")));
    }
    Ok(definitions)
}

/// The metadata `site` serves today.
pub fn serving(root: &Path, loaded: &Loaded, site: &Site) -> Result<Arc<EffectiveMeta>> {
    effective(root, loaded, site, &site.apps)
}

/// The install list a new site starts with: the platform app first, then every app in the
/// catalogue. A bare site starts with the platform only.
pub fn new_site_apps(root: &Path, loaded: &Loaded, bare: bool) -> Result<Vec<String>> {
    let mut apps = vec![PLATFORM.to_owned()];
    if !bare {
        for app in catalogue(root, loaded)? {
            if app.name != PLATFORM && !apps.contains(&app.name) {
                apps.push(app.name);
            }
        }
    }
    Ok(apps)
}

fn read_permissions(dir: &Path) -> spot_permissions::ModelTexts {
    crate::appdir::read_permission_texts(&dir.join("permissions"))
}

/// The permission model of one installed app: the platform's embedded texts when the
/// catalogue does not carry it, the app directory's `permissions/*.toml` otherwise.
fn app_model(root: &Path, loaded: &Loaded, name: &str) -> Result<spot_permissions::Model> {
    let catalogued = catalogue(root, loaded)?
        .into_iter()
        .find(|app| app.name == name);
    let texts = match catalogued {
        Some(app) => read_permissions(&app.dir),
        None if name == PLATFORM => spot_platform_app::permission_texts(),
        None => {
            return Err(Error(format!(
                "installed app `{name}` is not in the catalogue"
            )));
        }
    };
    spot_permissions::load(&texts).map_err(|errors| Error(format!("{errors:?}")))
}

/// Every installed app's permission model merged into the one snapshot the site's engine
/// evaluates: grants, field grants, protection groups and shares are concatenated, then the
/// merged model is checked against the effective metadata by the caller's snapshot build.
pub fn site_model(root: &Path, loaded: &Loaded, site: &Site) -> Result<spot_permissions::Model> {
    if site.apps.is_empty() {
        return Ok(spot_permissions::Model::default());
    }
    let mut merged = spot_permissions::Model::default();
    let mut errors = Vec::new();
    for app in &site.apps {
        match app_model(root, loaded, app) {
            Ok(model) => {
                merged.grants.extend(model.grants);
                merged.field_grants.extend(model.field_grants);
                merged.groups.extend(model.groups);
                merged.shares.extend(model.shares);
            }
            Err(e) => errors.push(format!("site `{}`: {e}", site.site)),
        }
    }
    if errors.is_empty() {
        Ok(merged)
    } else {
        Err(Error(errors.join("\n")))
    }
}
