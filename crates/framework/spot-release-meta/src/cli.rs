//! The `release-meta-check` CLI: read a release's declarations, compare them with the patches
//! on disk and print a readable report.
//!
//! The binary is a thin skin over [`main_with`]; CI's `tools/ci/release_meta_check.py` runs it.
//! Exit codes are the contract:
//!
//! - `0` — the release is fully declared;
//! - `1` — [`Problem`](crate::Problem)s were found (the release is refused);
//! - `2` — the check could not run (bad arguments, unreadable or malformed input).

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use spot_migration_plan::PatchDecl as PlanPatchDecl;

use crate::check::{ReleaseContext, check_release_with};
use crate::error::Error;
use crate::patch::PatchDecl;

/// What the check reads, as the CLI's arguments describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckConfig {
    /// The directory holding `patches/*.toml` and `changelog.toml`.
    pub release_meta: PathBuf,
    /// Directories to scan for patch files that exist on disk (app `patches/` directories).
    /// Empty means the declarations are treated as the patches on disk, so only the
    /// declaration-local rules run.
    pub patches: Vec<PathBuf>,
    /// A file of known DocType labels, one per line, for the unknown-DocType check.
    pub doctypes: Option<PathBuf>,
    /// A file of objects this release's code stops using, one per line, for the
    /// destructive-in-the-same-release check (ADR-22). Without it that check is not run.
    pub stopped_using: Option<PathBuf>,
}

impl CheckConfig {
    /// A config for a release-metadata directory, with no external data.
    #[must_use]
    pub fn new(release_meta: impl Into<PathBuf>) -> Self {
        Self {
            release_meta: release_meta.into(),
            patches: Vec::new(),
            doctypes: None,
            stopped_using: None,
        }
    }
}

/// The filesystem the check reads, so the whole CLI is testable without a real tree.
pub trait ReleaseFs {
    /// Reads a file as UTF-8.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// The immediate children of a directory, sorted. A missing directory is an error.
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    /// Whether a path is a directory.
    fn is_dir(&self, path: &Path) -> bool;
}

/// The real filesystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFs;

impl ReleaseFs for StdFs {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            out.push(entry?.path());
        }
        out.sort();
        Ok(out)
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

/// The facts the command line supplies: the known DocTypes (when `--doctypes` is given; with
/// none, every DocType is accepted) and the objects the release stops using.
struct CliContext {
    known: Option<BTreeSet<String>>,
    stopped: Vec<String>,
}

impl ReleaseContext for CliContext {
    fn knows_doctype(&self, doctype: &str) -> bool {
        self.known
            .as_ref()
            .is_none_or(|known| known.contains(doctype))
    }

    fn stopped_using(&self) -> Vec<String> {
        self.stopped.clone()
    }
}

const USAGE: &str = "\
release-meta-check: check that a release declares every patch and behaviour change

Usage: release-meta-check [--release-meta DIR] [--patches DIR]... [--doctypes FILE]
                          [--stopped-using FILE]

  --release-meta DIR  the release-metadata directory (default: release-meta) holding
                      patches/*.toml and an optional changelog.toml
  --patches DIR       a directory of patch files on disk, scanned recursively; repeat it for
                      each app. Without it, the declarations are treated as the patches on disk
  --doctypes FILE     a file of known DocType labels, one per line, for the unknown-DocType check
                      (without it the check is skipped; CI must pass it)
  --stopped-using FILE  a file of objects this release's code stops using, one per line, for the
                      destructive-in-the-same-release check (without it the check is skipped)
  -h, --help          print this help";

/// Runs the CLI over any [`ReleaseFs`]. Returns the exit code.
pub fn main_with<I>(args: I, fs: &dyn ReleaseFs, out: &mut dyn Write, err: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let config = match parse_args(&args) {
        Ok(Parse::Help) => {
            let _ = writeln!(out, "{USAGE}");
            return 0;
        }
        Ok(Parse::Config(config)) => config,
        Err(message) => {
            let _ = writeln!(err, "release-meta-check: {message}");
            let _ = writeln!(err, "{USAGE}");
            return 2;
        }
    };
    run(&config, fs, out, err)
}

/// Parses the arguments.
enum Parse {
    Config(CheckConfig),
    Help,
}

fn parse_args(args: &[String]) -> Result<Parse, String> {
    let mut config = CheckConfig::new("release-meta");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok(Parse::Help),
            "--release-meta" => config.release_meta = PathBuf::from(value(args, &mut i)?),
            "--patches" => config.patches.push(PathBuf::from(value(args, &mut i)?)),
            "--doctypes" => config.doctypes = Some(PathBuf::from(value(args, &mut i)?)),
            "--stopped-using" => {
                config.stopped_using = Some(PathBuf::from(value(args, &mut i)?));
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
        i += 1;
    }
    Ok(Parse::Config(config))
}

/// The value of an option that takes one, advancing past it.
fn value(args: &[String], i: &mut usize) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("`{}` needs a value", args[*i - 1]))
}

/// Runs the check and writes the report. See the module docs for the exit codes.
pub fn run(
    config: &CheckConfig,
    fs: &dyn ReleaseFs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let (declarations, changelog_entries) = match read_release_meta(config, fs) {
        Ok(read) => read,
        Err(problem) => {
            let _ = writeln!(err, "release-meta-check: {problem}");
            return 2;
        }
    };

    let on_disk: Vec<PlanPatchDecl> = if config.patches.is_empty() {
        declarations.iter().map(PatchDecl::to_plan_decl).collect()
    } else {
        match scan_patches(fs, &config.patches, &declarations) {
            Ok(patches) => patches,
            Err(problem) => {
                let _ = writeln!(err, "release-meta-check: {problem}");
                return 2;
            }
        }
    };

    let known = match &config.doctypes {
        Some(path) => match read_label_list(fs, path) {
            Ok(known) => Some(known),
            Err(problem) => {
                let _ = writeln!(err, "release-meta-check: {problem}");
                return 2;
            }
        },
        None => None,
    };
    let stopped = match &config.stopped_using {
        Some(path) => match read_label_list(fs, path) {
            Ok(stopped) => stopped.into_iter().collect(),
            Err(problem) => {
                let _ = writeln!(err, "release-meta-check: {problem}");
                return 2;
            }
        },
        None => Vec::new(),
    };
    if config.doctypes.is_none() {
        let _ = writeln!(
            err,
            "release-meta-check: warning: no --doctypes file, so the unknown-DocType check was skipped"
        );
    }
    if config.stopped_using.is_none() {
        let _ = writeln!(
            err,
            "release-meta-check: warning: no --stopped-using file, so the destructive-in-the-same-release check was skipped"
        );
    }
    let context = CliContext { known, stopped };

    let problems = check_release_with(&on_disk, &declarations, &context);

    let _ = writeln!(
        out,
        "release-meta-check: release `{}`",
        config.release_meta.display()
    );
    let _ = writeln!(
        out,
        "release-meta-check: {} declaration(s), {} patch(es) on disk, {} behaviour entry(ies)",
        declarations.len(),
        on_disk.len(),
        changelog_entries
    );
    if problems.is_empty() {
        let _ = writeln!(out, "release-meta-check: OK");
        return 0;
    }
    let _ = writeln!(out, "release-meta-check: {} problem(s)", problems.len());
    for problem in &problems {
        let _ = writeln!(out, "  - {problem}");
    }
    1
}

/// Reads the declarations and the changelog entry count.
fn read_release_meta(
    config: &CheckConfig,
    fs: &dyn ReleaseFs,
) -> Result<(Vec<PatchDecl>, usize), String> {
    let declarations = read_declarations(fs, &config.release_meta.join("patches"))?;
    let changelog = config.release_meta.join("changelog.toml");
    let entries = match fs.read_to_string(&changelog) {
        Ok(text) => parse_changelog(&changelog, &text)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(format!("cannot read {}: {e}", changelog.display())),
    };
    Ok((declarations, entries))
}

/// The declarations in a `patches/` directory, sorted by file name.
fn read_declarations(fs: &dyn ReleaseFs, dir: &Path) -> Result<Vec<PatchDecl>, String> {
    let entries = match fs.list(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    let mut files: Vec<PathBuf> = entries
        .into_iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    files.sort();
    let mut declarations = Vec::with_capacity(files.len());
    for file in files {
        let name = file.display().to_string();
        let text = fs
            .read_to_string(&file)
            .map_err(|e| format!("cannot read {name}: {e}"))?;
        declarations.push(PatchDecl::from_file(&name, &text).map_err(|e| e.to_string())?);
    }
    Ok(declarations)
}

/// The behaviour entry count, or a message when the changelog is malformed.
fn parse_changelog(path: &Path, text: &str) -> Result<usize, String> {
    crate::behaviour::BehaviourChangelog::from_toml(text)
        .map(|c| c.entries.len())
        .map_err(|e: Error| format!("{}: {e}", path.display()))
}

/// Scans directories for `*.toml` patch files and returns one plan declaration per patch id.
///
/// A file name carries only the id, so the `reversible` and `idempotent` flags are taken from
/// the declaration of the same id (a patch with none gets `false`, and is reported as
/// undeclared anyway). A directory that does not exist is an error, never an empty scan: a
/// mistyped `--patches` path must not let every undeclared patch through.
fn scan_patches(
    fs: &dyn ReleaseFs,
    dirs: &[PathBuf],
    declarations: &[PatchDecl],
) -> Result<Vec<PlanPatchDecl>, String> {
    let mut files = Vec::new();
    for dir in dirs {
        if !fs.is_dir(dir) {
            return Err(format!("--patches {} is not a directory", dir.display()));
        }
        collect(fs, dir, &mut files);
    }
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for file in files {
        if let Some(stem) = file.file_stem().and_then(|s| s.to_str()) {
            ids.insert(stem.to_owned());
        }
    }
    Ok(ids
        .into_iter()
        .map(|id| {
            let declared = declarations.iter().find(|d| d.id == id);
            PlanPatchDecl {
                intent: None,
                reversible: declared.is_some_and(|d| d.reversible),
                idempotent: declared.is_some_and(|d| d.idempotent),
                deps: Vec::new(),
                id,
            }
        })
        .collect())
}

/// Collects `*.toml` files under `dir`, skipping dot-directories and `target`.
fn collect(fs: &dyn ReleaseFs, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs.list(dir) else {
        return;
    };
    for entry in entries {
        let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fs.is_dir(&entry) {
            if name.starts_with('.') || name == "target" {
                continue;
            }
            collect(fs, &entry, out);
        } else if entry.extension().and_then(|e| e.to_str()) == Some("toml") {
            out.push(entry);
        }
    }
}

/// The labels in a file, one per line; `#` starts a comment.
fn read_label_list(fs: &dyn ReleaseFs, path: &Path) -> Result<BTreeSet<String>, String> {
    let text = fs
        .read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut labels = BTreeSet::new();
    for line in text.lines() {
        let label = line.split('#').next().unwrap_or("").trim();
        if !label.is_empty() {
            labels.insert(label.to_owned());
        }
    }
    Ok(labels)
}
