//! `help-coverage`: the completeness gate for offline error help.
//!
//! Checks every user-visible code in the error catalogue against the bundled help, the
//! `learn_more` documents and the style rules, and exits 1 on any finding. With
//! `--write-skeletons` it first writes derived entries for the missing codes into the crate's
//! `help/` folder so an author only has to review the text; rebuild afterwards to bundle them.
//!
//! Options: `--write-skeletons`, `--docs <dir>` (default: the repository's `docs/`) and
//! `--help-dir <dir>` (default: this crate's `help/`).

use std::path::PathBuf;
use std::process::ExitCode;

use spot_errors::Catalogue;
use spot_unattended_support::coverage::{check, user_visible, write_skeletons};
use spot_unattended_support::help::HelpIndex;

fn main() -> ExitCode {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut docs = manifest.join("../../../docs");
    let mut help_dir = manifest.join("help");
    let mut skeletons = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--write-skeletons" => skeletons = true,
            "--docs" => match args.next() {
                Some(v) => docs = PathBuf::from(v),
                None => return usage(),
            },
            "--help-dir" => match args.next() {
                Some(v) => help_dir = PathBuf::from(v),
                None => return usage(),
            },
            _ => return usage(),
        }
    }

    let catalogue = Catalogue::load();
    if let Some(problem) = catalogue.load_error() {
        eprintln!("the error catalogue does not load: {problem}");
        return ExitCode::FAILURE;
    }
    let index = HelpIndex::load();
    let findings = check(catalogue, index, Some(&docs));
    if skeletons && !findings.missing.is_empty() {
        match write_skeletons(catalogue, &findings.missing, &help_dir) {
            Ok(files) => {
                for file in files {
                    println!("wrote skeletons into {}", file.display());
                }
                println!("rebuild to bundle them, then review the text");
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    for error in &findings.errors {
        eprintln!("{error}");
    }
    println!(
        "{} user-visible codes, {} with help, {} problems",
        user_visible(catalogue).len(),
        user_visible(catalogue).len() - findings.missing.len(),
        findings.errors.len()
    );
    if findings.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: help-coverage [--write-skeletons] [--docs <dir>] [--help-dir <dir>]");
    ExitCode::from(2)
}
