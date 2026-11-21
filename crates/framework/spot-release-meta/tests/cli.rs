#![allow(missing_docs)]

//! The `release-meta-check` CLI over a fake filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use spot_release_meta::cli::{ReleaseFs, main_with};

/// A filesystem in memory: files only, with directories implied by their paths.
struct FakeFs {
    files: BTreeMap<PathBuf, String>,
}

impl FakeFs {
    fn new(files: &[(&str, &str)]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(path, text)| (PathBuf::from(path), (*text).to_owned()))
                .collect(),
        }
    }
}

impl ReleaseFs for FakeFs {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.files.get(path).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} does not exist", path.display()),
            )
        })
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut children = BTreeSet::new();
        for path in self.files.keys() {
            if let Ok(rel) = path.strip_prefix(dir)
                && let Some(first) = rel.components().next()
            {
                children.insert(dir.join(first.as_os_str()));
            }
        }
        if children.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such directory"));
        }
        Ok(children.into_iter().collect())
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.files.keys().any(|p| p != path && p.starts_with(path))
    }
}

const P1: &str = "id = \"p1\"\nintent = \"add a field\"\nclass = \"additive\"\nreversible = true\nidempotent = true\n";

fn run(files: &[(&str, &str)], args: &[&str]) -> (i32, String) {
    let fs = FakeFs::new(files);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = main_with(
        args.iter().map(|s| (*s).to_owned()),
        &fs,
        &mut out,
        &mut err,
    );
    let mut text = String::from_utf8_lossy(&out).into_owned();
    text.push_str(&String::from_utf8_lossy(&err));
    (code, text)
}

#[test]
fn a_declared_release_passes() {
    let (code, text) = run(
        &[("release-meta/patches/p1.toml", P1)],
        &["--release-meta", "release-meta"],
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("OK"), "{text}");
}

#[test]
fn an_undeclared_patch_fails() {
    let (code, text) = run(
        &[
            ("release-meta/patches/p1.toml", P1),
            ("apps/acme/patches/q1.toml", "id = \"q1\"\n"),
        ],
        &[
            "--release-meta",
            "release-meta",
            "--patches",
            "apps/acme/patches",
        ],
    );
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("missing-declaration"), "{text}");
    assert!(text.contains("dangling-declaration"), "{text}");
}

#[test]
fn an_unknown_doctype_fails_with_a_list() {
    let with_type = "id = \"p1\"\nintent = \"x\"\ndoctypes = [\"Ghost\"]\nclass = \"additive\"\nreversible = true\nidempotent = true\n";
    let (code, text) = run(
        &[
            ("release-meta/patches/p1.toml", with_type),
            ("types.txt", "Sales Invoice\n# a comment\n"),
        ],
        &["--release-meta", "release-meta", "--doctypes", "types.txt"],
    );
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("unknown-doctype"), "{text}");
    assert!(text.contains("Ghost"), "{text}");
}

#[test]
fn a_known_doctype_passes() {
    let with_type = "id = \"p1\"\nintent = \"x\"\ndoctypes = [\"Sales Invoice\"]\nclass = \"additive\"\nreversible = true\nidempotent = true\n";
    let (code, text) = run(
        &[
            ("release-meta/patches/p1.toml", with_type),
            ("types.txt", "Sales Invoice\n"),
        ],
        &["--release-meta", "release-meta", "--doctypes", "types.txt"],
    );
    assert_eq!(code, 0, "{text}");
}

#[test]
fn a_malformed_declaration_cannot_run() {
    let (code, text) = run(
        &[("release-meta/patches/p1.toml", "id = \"p1\"\n")],
        &["--release-meta", "release-meta"],
    );
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("release_meta.bad_patch"), "{text}");
}

#[test]
fn bad_arguments_cannot_run() {
    let (code, text) = run(&[], &["--nonsense"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("unexpected argument"), "{text}");
}

#[test]
fn help_is_success() {
    let (code, text) = run(&[], &["--help"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Usage:"), "{text}");
}

#[test]
fn recursion_skips_target_and_dot_directories() {
    let (code, text) = run(
        &[
            (
                "release-meta/patches/y.toml",
                "id = \"y\"\nintent = \"x\"\nclass = \"additive\"\nreversible = true\nidempotent = true\n",
            ),
            ("apps/a/patches/y.toml", "id = \"y\"\n"),
            ("apps/target/patches/z.toml", "id = \"z\"\n"),
            ("apps/.git/patches/w.toml", "id = \"w\"\n"),
        ],
        &["--release-meta", "release-meta", "--patches", "apps"],
    );
    assert_eq!(code, 0, "{text}");
}

#[test]
fn a_missing_changelog_is_not_an_error() {
    let (code, text) = run(
        &[("release-meta/patches/p1.toml", P1)],
        &["--release-meta", "release-meta"],
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("0 behaviour entry"), "{text}");
}

#[test]
fn a_patches_directory_that_does_not_exist_cannot_run() {
    let (code, text) = run(
        &[("release-meta/patches/p1.toml", P1)],
        &[
            "--release-meta",
            "release-meta",
            "--patches",
            "apps/typo/patches",
        ],
    );
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("is not a directory"), "{text}");
}

#[test]
fn a_destructive_patch_on_an_object_the_release_stops_using_is_refused() {
    let destructive = "id = \"d1\"\nintent = \"drop a column\"\nclass = \"destructive\"\ndoctypes = [\"Invoice\"]\nreversible = true\nidempotent = true\n";
    let (code, text) = run(
        &[
            ("release-meta/patches/d1.toml", destructive),
            ("stopped.txt", "Invoice\n"),
        ],
        &[
            "--release-meta",
            "release-meta",
            "--stopped-using",
            "stopped.txt",
        ],
    );
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("destructive-while-stop-is-same-release"),
        "{text}"
    );
}

#[test]
fn skipping_the_doctype_and_stopped_using_checks_is_said_aloud() {
    let (code, text) = run(
        &[("release-meta/patches/p1.toml", P1)],
        &["--release-meta", "release-meta"],
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("unknown-DocType check was skipped"), "{text}");
    assert!(
        text.contains("destructive-in-the-same-release check was skipped"),
        "{text}"
    );
}
