//! The release's usage manifest, generated from the compiled catalogue and checked against it
//! (WP-STO-07.T9, ADR-22).
//!
//! `spot-expand-contract` decides whether a destructive (contract) schema step may ship, by
//! comparing the usage manifests of two releases. This binary produces and checks the manifest of
//! one release **from the compiled catalogue**, never by hand: the platform and app manifests are
//! compiled exactly as `spot migrate` compiles them (no database is contacted), the derived
//! traversal indexes are added, and `spot_expand_contract::UsageManifest::from_catalog` lists every
//! table, column and index the release can touch.
//!
//! ```text
//! usage_manifest generate --root <dir> --release <id> --out <file>
//! usage_manifest check    --root <dir> --manifest <file> [--release <id>]
//! ```
//!
//! `--root` is the platform root (default: the current directory), as for `spot`.
//!
//! # Exit codes
//!
//! * `0`: ok (`generate` wrote the file; `check` found the manifest byte-identical to a fresh one);
//! * `1`: `check` found problems (a stale, missing or unreadable manifest), one line each;
//! * `2`: the command could not run (bad arguments, unreadable root, catalogue does not compile).
//!
//! # Regenerating a checked-in manifest
//!
//! The checked-in `release-meta/usage.toml` is an artefact. Never edit it by hand: run
//! `python tools/ci/usage_manifest.py generate --release <id>` and commit the result in the
//! release PR. `check` is generate-and-diff: it regenerates the manifest text from the catalogue (with
//! the release label read from the committed file, or `--release` when the file is missing or
//! unreadable) and fails whenever the committed file is not byte-for-byte that text, so a new,
//! removed or renamed table, column or index is always caught, including a new column of a table
//! the manifest already lists (which `UsageManifest::uses` alone would accept). The structural
//! `check_manifest` problems are printed first, then the lines that differ.
//!
//! # The derived traversal indexes
//!
//! The reverse index of every traversable Link field is part of the catalogue. The list comes from
//! `spot_migration_plan::traversal::wanted`, over the traversal state
//! `spot_migration_plan::state_from_model` derives from the graph model, the same way the plan
//! does (WP-STO-07.T5, T8). The host does not apply that list at `spot migrate` yet, so this
//! binary applies it itself: that is why a derived index is always listed and cannot ship unlisted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use spot_cli::{apps, config};
use spot_expand_contract::{ReleaseId, UsageManifest, check_manifest};
use spot_graph_model::GraphModel;
use spot_meta::ReverseIndex;
use spot_migration_plan::traversal::wanted;
use spot_migration_plan::{DocTypeDescription, SystemDescription, state_from_model};
use spot_storage_contract::{Catalog, Collation};

/// The exit code for a command that ran and found problems.
const PROBLEMS: u8 = 1;
/// The exit code for a command that could not run.
const CANNOT_RUN: u8 = 2;

#[derive(Parser)]
#[command(
    name = "usage_manifest",
    about = "Generate or check a release's usage manifest from the compiled catalogue. \
             Regenerate a checked-in manifest with `python tools/ci/usage_manifest.py generate \
             --release <id>`; never edit it by hand."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the manifest the compiled catalogue implies.
    Generate {
        /// The platform root (default: the current directory).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// The release label the manifest describes.
        #[arg(long)]
        release: String,
        /// The file to write.
        #[arg(long)]
        out: PathBuf,
    },
    /// Check a manifest file against the compiled catalogue.
    Check {
        /// The platform root (default: the current directory).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// The manifest file to check.
        #[arg(long)]
        manifest: PathBuf,
        /// The release label, used only in the regenerate hint when the file is missing or
        /// unreadable (a readable file carries its own).
        #[arg(long)]
        release: Option<String>,
    },
}

/// The catalogue of the platform at `root`: every app of the catalogue installed, compiled with the
/// reverse indexes of the traversable Link fields. No database is contacted.
fn compile_catalogue(root: &Path) -> Result<Catalog, String> {
    let loaded = config::load(root, false).map_err(|e| e.to_string())?;
    let installed = apps::new_site_apps(root, &loaded, false).map_err(|e| e.to_string())?;
    let site = config::Site::named("usage-manifest", "usage_manifest", "und", installed.clone());
    let meta = apps::effective(root, &loaded, &site, &installed).map_err(|e| e.to_string())?;
    let system = SystemDescription {
        doctypes: meta
            .definitions()
            .iter()
            .map(|definition| DocTypeDescription {
                definition: definition.clone(),
                permissions: Vec::new(),
                views: BTreeMap::new(),
            })
            .collect(),
        ..SystemDescription::new()
    };
    let model = GraphModel::derive(&meta);
    let state = state_from_model(&model, &[], &[]);
    let reverse: Vec<ReverseIndex> = wanted(&system, &state)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|index| ReverseIndex {
            table: index.table,
            column: index.column,
            name: index.name,
        })
        .collect();
    let collation = Collation::new("und").map_err(|e| e.to_string())?;
    meta.catalog_with_reverse_indexes(collation, &reverse)
        .map_err(|e| e.to_string())
}

/// The manifest text of `release` for `catalog`: deterministic, ending in one newline.
fn render(release: &str, catalog: &Catalog) -> Result<String, String> {
    let release = ReleaseId::new(release).map_err(|e| e.to_string())?;
    let mut text = UsageManifest::from_catalog(release, catalog).to_toml();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// `generate`: compile the catalogue and write the manifest. Returns the one line to print.
fn generate(root: &Path, release: &str, out: &Path) -> Result<String, String> {
    let catalog = compile_catalogue(root)?;
    let text = render(release, &catalog)?;
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(out, &text).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    Ok(format!(
        "usage manifest of release {release} written to {} ({} lines)",
        out.display(),
        text.lines().count()
    ))
}

/// The line that tells a developer how to fix a stale manifest. It never suggests editing the file.
/// `release` is the real label when known; otherwise the line says to pass the release label.
fn regenerate_hint(release: Option<&str>) -> String {
    let flag = match release {
        Some(release) => format!("--release {release}"),
        None => "with the release's label as --release".to_owned(),
    };
    format!(
        "the manifest is generated, never edited: regenerate with \
         `python tools/ci/usage_manifest.py generate` ({flag})"
    )
}

/// The `release = "..."` label of a manifest text that does not parse, when it can be read.
fn scrape_release(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.trim().strip_prefix("release")?.trim_start();
        let value = value.strip_prefix('=')?.trim();
        let value = value.strip_prefix('"')?.strip_suffix('"')?;
        Some(value.to_owned())
    })
}

/// The lines present in only one of the two texts: `+` for generated-only, `-` for committed-only.
fn diff_lines(committed: &str, generated: &str) -> Vec<String> {
    let have: std::collections::BTreeSet<&str> = committed.lines().collect();
    let want: std::collections::BTreeSet<&str> = generated.lines().collect();
    let mut out: Vec<String> = generated
        .lines()
        .filter(|l| !have.contains(l))
        .map(|l| format!("  + {l}"))
        .collect();
    out.extend(
        committed
            .lines()
            .filter(|l| !want.contains(l))
            .map(|l| format!("  - {l}")),
    );
    out
}

/// `check`: the problem lines of `manifest` against the compiled catalogue (empty when current).
/// The manifest must equal the freshly generated text byte for byte.
fn check(root: &Path, manifest: &Path, release: Option<&str>) -> Result<Vec<String>, String> {
    let catalog = compile_catalogue(root)?;
    let text = match std::fs::read_to_string(manifest) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(vec![
                format!(
                    "missing-manifest `{}`: the file does not exist",
                    manifest.display()
                ),
                regenerate_hint(release),
            ]);
        }
        Err(e) => return Err(format!("cannot read {}: {e}", manifest.display())),
    };
    let parsed = match UsageManifest::from_toml(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            let scraped = scrape_release(&text);
            return Ok(vec![
                format!("unreadable-manifest `{}`: {e}", manifest.display()),
                regenerate_hint(release.or(scraped.as_deref())),
            ]);
        }
    };
    let label = parsed.release().as_str().to_owned();
    let generated = render(&label, &catalog)?;
    let mut lines: Vec<String> = check_manifest(&parsed, &catalog)
        .iter()
        .map(ToString::to_string)
        .collect();
    if text == generated && lines.is_empty() {
        return Ok(Vec::new());
    }
    if text != generated {
        lines.push(format!(
            "stale-manifest `{}`: differs from the manifest the catalogue generates",
            manifest.display()
        ));
        lines.extend(diff_lines(&text, &generated));
    }
    lines.push(regenerate_hint(Some(&label)));
    Ok(lines)
}

fn run(cli: Cli) -> Result<u8, String> {
    match cli.command {
        Command::Generate { root, release, out } => {
            println!("{}", generate(&root, &release, &out)?);
            Ok(0)
        }
        Command::Check {
            root,
            manifest,
            release,
        } => {
            let lines = check(&root, &manifest, release.as_deref())?;
            if lines.is_empty() {
                println!("usage manifest {} is current", manifest.display());
                return Ok(0);
            }
            for line in lines {
                println!("{line}");
            }
            Ok(PROBLEMS)
        }
    }
}

fn main() -> ExitCode {
    // `clap` exits with 2 on a usage error and 0 on `--help`, which matches the exit codes above.
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("usage_manifest: {message}");
            ExitCode::from(CANNOT_RUN)
        }
    }
}
