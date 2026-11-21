//! The typed-view generator (WP-MET-04.T1; ADR-05, ADR-057).

use std::path::Path;

use crate::parse::parse as parse_definitions;

pub mod error;
pub mod map;
pub mod naming;
pub mod parse;
pub mod view;

pub use error::{GenError, GenErrorKind};
pub use parse::{Model, parse};

/// Generates the typed views for `definitions` as formatted Rust source.
///
/// # Errors
/// One error per invalid name, select without options, table without a child, or collision.
pub fn generate(definitions: &[spot_meta::Definition]) -> Result<String, Vec<GenError>> {
    let model = parse_definitions(definitions)?;
    Ok(prettyplease::unparse(&view::file_of(&model)))
}

/// The build helper a `build.rs` calls.
///
/// Loads every definition in `dir` (`spot_meta::load_dir`), generates and writes them to
/// `out_file` (an app names an `OUT_DIR`-joined path), and prints
/// `cargo:rerun-if-changed=<file>` for every TOML file, so any added, removed, renamed or
/// edited definition file rebuilds the crate.
///
/// # Errors
/// The load errors or every generation error, joined into one [`BuildError`].
pub fn build_views(dir: &Path, out_file: &Path) -> Result<(), BuildError> {
    let definitions = spot_meta::load_dir(dir).map_err(join_displays)?;
    for file in toml_files(dir) {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    let source = generate(&definitions).map_err(join_gen)?;
    if let Some(parent) = out_file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| BuildError(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(out_file, source)
        .map_err(|e| BuildError(format!("cannot write {}: {e}", out_file.display())))?;
    Ok(())
}

/// Joins any displayable errors, one per line, into a [`BuildError`].
fn join_displays<E: std::fmt::Display>(errors: impl IntoIterator<Item = E>) -> BuildError {
    BuildError(
        errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Generation errors as one [`BuildError`].
fn join_gen(errors: Vec<error::GenError>) -> BuildError {
    join_displays(errors)
}

/// Every `*.toml` in `dir`, sorted.
fn toml_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// The build helper's error: the load or generation errors, one per line.
#[derive(Debug)]
pub struct BuildError(pub String);

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BuildError {}
