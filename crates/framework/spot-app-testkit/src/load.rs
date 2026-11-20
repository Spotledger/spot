//! Loading an app directory the way the server would: the app check first (every problem at
//! once), then the DocTypes, then the permission texts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_manifest::AppManifest;
use spot_meta::{Definition, EffectiveMeta, Registry};
use spot_permissions::{ModelTexts, PermissionEngine, Snapshot};

use crate::error::{Result, TestkitError};

/// One app directory, read.
#[derive(Clone, Debug)]
pub(crate) struct LoadedApp {
    /// The directory's own name (what `run_script` addresses an app by).
    pub(crate) name: String,
    /// The app directory.
    pub(crate) dir: PathBuf,
    /// The manifest, when the directory has one that parses (the app check has judged it).
    pub(crate) manifest: Option<AppManifest>,
    pub(crate) definitions: Vec<Definition>,
    pub(crate) texts: ModelTexts,
}

fn read_plain(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

/// The permission texts of an app's `permissions/` directory: `grants.toml` followed by every
/// other `*grants.toml` (not `field_grants.toml`) sorted by name, plus `field_grants.toml` and
/// `groups.toml`. The same rule the server's app loader uses.
fn permission_texts(dir: &Path) -> ModelTexts {
    let mut extra: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.ends_with("grants.toml") && name != "grants.toml" && name != "field_grants.toml"
        })
        .collect();
    extra.sort();
    let mut grants = vec![read_plain(dir, "grants.toml")];
    grants.extend(extra.iter().map(|name| read_plain(dir, name)));
    ModelTexts {
        grants: grants.join("\n"),
        field_grants: read_plain(dir, "field_grants.toml"),
        groups: read_plain(dir, "groups.toml"),
        shares: String::new(),
    }
}

/// The platform DocTypes an app's links may name: the platform app's own, plus the file record
/// (`file`), which the files service owns and the platform app's list does not carry.
fn platform_definitions() -> Vec<Definition> {
    let mut definitions = spot_platform_app::definitions().unwrap_or_default();
    if let Ok(file) = Definition::parse(
        "file.toml",
        include_str!("../../../../apps/platform/doctypes/file.toml"),
    ) {
        definitions.push(file);
    }
    definitions
}

/// Read and select on the platform's `user` and `file` DocTypes for every role the apps' grants
/// name. A link is only valid to a record the writer may read (WP-DOC-06.T2), and the platform's
/// own grants give those two DocTypes to its administrators only, so without this an app role
/// could not set an `owner` or attach a file in a test.
fn lookup_grants(grants: &str) -> String {
    let mut roles: Vec<&str> = grants
        .lines()
        .filter_map(|line| line.trim().strip_prefix("role"))
        .filter_map(|rest| rest.trim_start().strip_prefix('='))
        .filter_map(|value| value.trim().strip_prefix('"'))
        .filter_map(|value| value.split('"').next())
        .collect();
    roles.sort_unstable();
    roles.dedup();
    let mut out = String::new();
    for role in roles {
        for doctype in ["user", "file"] {
            out.push_str(&format!(
                "[[entries]]\ndoctype = \"{doctype}\"\nrole = \"{role}\"\nrights = [\"read\", \"select\"]\n\n"
            ));
        }
    }
    out
}

/// The app check of `dir`, or `None` for a directory in the server's own layout.
///
/// The packaging layout names the manifest `manifest.toml` and allows a fixed set of files; a
/// server app directory (the sample app: `app.toml`, a README, reports, a seed file) is not a
/// pack and would fail its layout rules. That directory is not pack-checked: its DocTypes and
/// permission files are loaded below and fail there with their own reports.
fn check_dir(dir: &Path) -> Option<spot_pack::CheckReport> {
    if !dir.join("manifest.toml").exists() && dir.join("app.toml").exists() {
        return None;
    }
    Some(spot_pack::check_app_with(
        dir,
        None,
        &platform_definitions(),
    ))
}

/// Checks and reads the app at `dir`.
pub(crate) fn load_app(dir: &Path) -> Result<LoadedApp> {
    if let Some(report) = check_dir(dir)
        && !report.is_ok()
    {
        let problems = report
            .problems
            .iter()
            .map(|p| match &p.path {
                Some(path) => format!("{} {path}: {}", p.code, p.message),
                None => format!("{} {}", p.code, p.message),
            })
            .collect();
        return Err(TestkitError::invalid(
            &format!("the app at {} failed its checks", dir.display()),
            problems,
        ));
    }
    let definitions = spot_meta::load_dir(&dir.join("doctypes")).map_err(|errors| {
        TestkitError::invalid(
            "the app's DocTypes do not load",
            errors.iter().map(ToString::to_string).collect(),
        )
    })?;
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app")
        .to_owned();
    let manifest = ["manifest.toml", "app.toml"].iter().find_map(|file| {
        let text = std::fs::read_to_string(dir.join(file)).ok()?;
        spot_manifest::parse(&text, file).ok()
    });
    Ok(LoadedApp {
        name,
        dir: dir.to_path_buf(),
        manifest,
        definitions,
        texts: permission_texts(&dir.join("permissions")),
    })
}

/// The effective metadata and the permission engine of the platform DocTypes plus `apps`.
///
/// `extra_grants` is grants TOML appended after the apps' own (the roles the harness made for
/// `with_permissions`); empty for none.
pub(crate) fn assemble(
    apps: &[LoadedApp],
    extra_grants: &str,
) -> Result<(Arc<EffectiveMeta>, Arc<PermissionEngine>)> {
    let mut definitions = spot_platform_app::definitions().map_err(|e| {
        TestkitError::invalid("the platform DocTypes do not load", vec![e.to_string()])
    })?;
    definitions.extend(
        platform_definitions()
            .into_iter()
            .filter(|d| d.name == "file"),
    );
    for app in apps {
        definitions.extend(app.definitions.iter().cloned());
    }
    let meta = Registry::new(definitions)
        .map_err(|errors| {
            TestkitError::invalid(
                "the DocTypes of the loaded apps do not build together",
                errors.iter().map(ToString::to_string).collect(),
            )
        })?
        .effective();
    let join = |pick: fn(&ModelTexts) -> &String| -> String {
        apps.iter()
            .map(|a| pick(&a.texts).as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut grants = join(|t| &t.grants);
    grants.push('\n');
    grants.push_str(&lookup_grants(&grants));
    if !extra_grants.is_empty() {
        grants.push('\n');
        grants.push_str(extra_grants);
    }
    let texts = ModelTexts {
        grants,
        field_grants: join(|t| &t.field_grants),
        groups: join(|t| &t.groups),
        shares: String::new(),
    };
    let mut model = spot_permissions::load(&texts).map_err(|e| {
        TestkitError::invalid("the permission files do not load", vec![format!("{e:?}")])
    })?;
    let build_error = |e: &dyn std::fmt::Debug| {
        TestkitError::invalid(
            "the permission snapshot does not build",
            vec![format!("{e:?}")],
        )
    };
    // A DocType may declare a protection group that no permission file names (the platform's
    // `credential`): the group is still closed, so declare it as the server does.
    for protection in meta.field_protection() {
        for group in &protection.groups {
            if !model
                .groups
                .iter()
                .any(|declared| &declared.name.0 == group)
            {
                model.groups.push(spot_permissions::ProtectionGroup {
                    name: spot_permissions::GroupName(group.clone()),
                    trace: false,
                });
            }
        }
    }
    let mut snapshot = Snapshot::build(&meta, &model).map_err(|e| build_error(&e))?;
    // The protection facts the metadata declares, per DocType, as the server stamps them.
    for view in meta.doctypes() {
        let name = view.name.as_str();
        let prefix = format!("{name}.");
        let facts: Vec<spot_permissions::FieldFacts> = meta
            .field_protection()
            .iter()
            .filter_map(|p| protection_facts(p, &prefix))
            .collect();
        snapshot = snapshot
            .with_field_facts(name, facts)
            .map_err(|e| build_error(&e))?;
    }
    Ok((meta, Arc::new(PermissionEngine::new(Arc::new(snapshot)))))
}

/// The protection facts of one field, when `protection` belongs to the DocType `prefix` names.
fn protection_facts(
    protection: &spot_meta::FieldProtection,
    prefix: &str,
) -> Option<spot_permissions::FieldFacts> {
    let path = protection.path.strip_prefix(prefix)?;
    Some(spot_permissions::FieldFacts {
        path: path.to_owned(),
        groups: protection
            .groups
            .iter()
            .cloned()
            .map(spot_permissions::GroupName)
            .collect(),
        tags: protection
            .tags
            .iter()
            .cloned()
            .map(spot_permissions::Tag)
            .collect(),
        mask: protection.mask.as_deref().map(|text| {
            text.strip_prefix("last_chars(")
                .and_then(|rest| rest.strip_suffix(')'))
                .and_then(|n| n.parse::<u8>().ok())
                .map_or(
                    spot_permissions::MaskFormat::Placeholder,
                    spot_permissions::MaskFormat::LastChars,
                )
        }),
    })
}

/// `dir` as given, for the builder.
pub(crate) fn to_path(dir: impl AsRef<Path>) -> PathBuf {
    dir.as_ref().to_path_buf()
}
