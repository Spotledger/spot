//! The `[[permissions]]` section of the app manifest (WP-PLT-03.T4): an app declares the
//! named permissions its whitelisted methods use, the parser and shared checks report every
//! problem at once with a stable code, and `app_permissions()` is the one function the host
//! calls per installed app.
//!
//! Declaring a permission grants it to nobody: grants are role data (`spot-permissions`).

use spot_app_api::prelude::Name;
use spot_manifest::{
    AppManifest, AppMeta, AppPermissionName, Capabilities, MAX_PERMISSION_TITLE_CHARS,
    ManifestError, PermissionSub, parse, validate,
};

/// The app the samples belong to.
const APP: &str = "erp_selling";

/// Unwrap for tests without tripping the workspace's `unwrap_used` lint.
#[track_caller]
fn got<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => panic!("expected Ok, got {err:?}"),
    }
}

/// A manifest with the given tail after `[app]`.
fn text(tail: &str) -> String {
    format!("[app]\nname = \"{APP}\"\nversion = \"0.1.0\"\n\n{tail}")
}

fn render(errs: &[ManifestError]) -> String {
    errs.iter()
        .map(|e| {
            format!(
                "{} (line {:?}, col {:?}): {}",
                e.code, e.line, e.col, e.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses a manifest, failing with its errors.
#[track_caller]
fn parsed(tail: &str) -> AppManifest {
    match parse(&text(tail), "spot-app.toml") {
        Ok(manifest) => manifest,
        Err(errs) => panic!("must parse, got:\n{}", render(&errs)),
    }
}

/// The errors of a manifest that must not parse.
#[track_caller]
fn errors(tail: &str) -> Vec<ManifestError> {
    match parse(&text(tail), "spot-app.toml") {
        Ok(manifest) => panic!("must not parse: {manifest:?}"),
        Err(errs) => errs,
    }
}

/// Asserts one error: this code, at this line and column.
#[track_caller]
fn one_error(tail: &str, code: &str, line: u32, col: u32) {
    let errs = errors(tail);
    let [err] = errs.as_slice() else {
        panic!("expected exactly one error, got:\n{}", render(&errs));
    };
    assert_eq!(err.code, code, "{err}");
    assert_eq!((err.line, err.col), (Some(line), Some(col)), "{err}");
}

/// A code-built manifest, the way a native app assembles one.
fn built(permissions: Vec<PermissionSub>) -> AppManifest {
    AppManifest {
        app: AppMeta {
            name: got(Name::new(APP)),
            version: got(semver::Version::parse("0.1.0")),
            title: String::new(),
            requires_host: None,
            depends: Vec::new(),
        },
        events: Vec::new(),
        session_events: Vec::new(),
        boot: Vec::new(),
        schedules: Vec::new(),
        overrides: Vec::new(),
        seams: Vec::new(),
        methods: Vec::new(),
        functions: Vec::new(),
        fixtures: Vec::new(),
        patches: Vec::new(),
        routes: Vec::new(),
        assets: Vec::new(),
        capabilities: Capabilities::default(),
        permissions,
    }
}

fn permission(name: &str, title: &str) -> PermissionSub {
    PermissionSub {
        name: got(AppPermissionName::parse(name)),
        title: title.to_owned(),
        description: None,
    }
}

#[test]
fn permissions_parse_into_the_model() {
    let manifest = parsed(
        "[[permissions]]\n\
         name = \"approve_note\"\n\
         title = \"Approve notes\"\n\
         description = \"Lets a user approve a note.\"\n\
         \n\
         [[permissions]]\n\
         name = \"reject_note_2\"\n\
         title = \"Reject notes\"\n",
    );
    assert_eq!(manifest.permissions.len(), 2);
    assert_eq!(manifest.permissions[0].name.as_str(), "approve_note");
    assert_eq!(manifest.permissions[0].title, "Approve notes");
    assert_eq!(
        manifest.permissions[0].description.as_deref(),
        Some("Lets a user approve a note.")
    );
    assert_eq!(manifest.permissions[1].name.as_str(), "reject_note_2");
    assert_eq!(manifest.permissions[1].description, None);
}

#[test]
fn app_permissions_carries_the_app_name_in_declaration_order() {
    let manifest = parsed(
        "[[permissions]]\n\
         name = \"zeta_note\"\n\
         title = \"Zeta notes\"\n\
         \n\
         [[permissions]]\n\
         name = \"alpha_note\"\n\
         title = \"Alpha notes\"\n\
         description = \"Lets a user approve an alpha note.\"\n",
    );
    let pairs = manifest.app_permissions();
    let names: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        ["zeta_note", "alpha_note"],
        "order is the file's, not sorted"
    );
    assert!(pairs.iter().all(|(_, decl)| decl.app == APP));
    assert_eq!(pairs[0].1.title, "Zeta notes");
    assert_eq!(pairs[0].1.description, "", "a missing description is empty");
    assert_eq!(pairs[1].1.description, "Lets a user approve an alpha note.");
}

#[test]
fn a_manifest_without_permissions_is_valid() {
    let manifest = parsed("");
    assert!(manifest.permissions.is_empty());
    assert!(manifest.app_permissions().is_empty());
}

#[test]
fn code_built_permissions_round_trip() {
    let mut declared = permission("approve_note", "Approve notes");
    declared.description = Some("Lets a user approve a note.".to_owned());
    let manifest = built(vec![
        declared.clone(),
        permission("reject_note", "Reject notes"),
    ]);
    assert_eq!(validate(&manifest), Vec::new());
    let pairs = manifest.app_permissions();
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0].0.as_str(), "approve_note");
    assert_eq!(pairs[0].1.app, APP);
    assert_eq!(pairs[0].1.title, declared.title);
    assert_eq!(pairs[1].0.as_str(), "reject_note");
}

#[test]
fn err_permission_name_not_lowercase() {
    one_error(
        "[[permissions]]\nname = \"Approve\"\ntitle = \"Approve notes\"\n",
        "manifest.permission_name",
        6,
        8,
    );
}

#[test]
fn err_permission_name_empty() {
    one_error(
        "[[permissions]]\nname = \"\"\ntitle = \"Approve notes\"\n",
        "manifest.permission_name",
        6,
        8,
    );
}

#[test]
fn err_permission_name_holds_a_dash() {
    one_error(
        "[[permissions]]\nname = \"approve-note\"\ntitle = \"Approve notes\"\n",
        "manifest.permission_name",
        6,
        8,
    );
}

#[test]
fn err_permission_title_empty() {
    one_error(
        "[[permissions]]\nname = \"approve_note\"\ntitle = \"\"\n",
        "manifest.permission_title",
        7,
        9,
    );
}

#[test]
fn err_permission_title_whitespace_only() {
    one_error(
        "[[permissions]]\nname = \"approve_note\"\ntitle = \"   \"\n",
        "manifest.permission_title",
        7,
        9,
    );
}

#[test]
fn a_120_character_title_is_accepted() {
    let title = "n".repeat(MAX_PERMISSION_TITLE_CHARS);
    let manifest = parsed(&format!(
        "[[permissions]]\nname = \"approve_note\"\ntitle = \"{title}\"\n"
    ));
    assert_eq!(manifest.permissions[0].title.chars().count(), 120);
}

#[test]
fn err_permission_title_over_120_characters() {
    let title = "n".repeat(MAX_PERMISSION_TITLE_CHARS + 1);
    let errs = errors(&format!(
        "[[permissions]]\nname = \"approve_note\"\ntitle = \"{title}\"\n"
    ));
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    assert_eq!(codes, ["manifest.permission_title"], "{}", render(&errs));
    assert_eq!(
        (errs[0].line, errs[0].col),
        (Some(7), Some(9)),
        "{}",
        errs[0]
    );
}

#[test]
fn err_permission_duplicate() {
    one_error(
        "[[permissions]]\n\
         name = \"approve_note\"\n\
         title = \"Approve notes\"\n\
         \n\
         [[permissions]]\n\
         name = \"approve_note\"\n\
         title = \"Approve notes again\"\n",
        "manifest.permission_duplicate",
        10,
        8,
    );
}

#[test]
fn two_problems_come_out_together() {
    // One row the parser refuses (its name), one row the shared checks refuse (its title):
    // the author sees both in one pass.
    let errs = errors(
        "[[permissions]]\n\
         name = \"9approve\"\n\
         title = \"Approve notes\"\n\
         \n\
         [[permissions]]\n\
         name = \"reject_note\"\n\
         title = \"\"\n",
    );
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    assert_eq!(
        codes,
        ["manifest.permission_name", "manifest.permission_title"],
        "{}",
        render(&errs)
    );
}

#[test]
fn a_duplicate_and_an_empty_title_come_out_together() {
    // Both are shared checks, so they are found in the same pass over the same manifest.
    let errs = errors(
        "[[permissions]]\n\
         name = \"approve_note\"\n\
         title = \"\"\n\
         \n\
         [[permissions]]\n\
         name = \"approve_note\"\n\
         title = \"Approve notes\"\n",
    );
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    assert_eq!(
        codes,
        ["manifest.permission_title", "manifest.permission_duplicate"],
        "{}",
        render(&errs)
    );
}

#[test]
fn err_permission_unknown_key() {
    let errs = errors(
        "[[permissions]]\n\
         name = \"approve_note\"\n\
         title = \"Approve notes\"\n\
         scope = \"company\"\n",
    );
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    assert_eq!(codes, ["manifest.unknown_key"], "{}", render(&errs));
    assert_eq!(
        (errs[0].line, errs[0].col),
        (Some(8), Some(9)),
        "{}",
        errs[0]
    );
}

#[test]
fn err_permission_missing_title() {
    let errs = errors("[[permissions]]\nname = \"approve_note\"\n");
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    assert_eq!(codes, ["manifest.missing_key"], "{}", render(&errs));
}

#[test]
fn err_permission_title_wrong_type() {
    let errs = errors("[[permissions]]\nname = \"approve_note\"\ntitle = 3\n");
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    assert_eq!(codes, ["manifest.invalid_type"], "{}", render(&errs));
}

#[test]
fn validate_checks_code_built_permissions_too() {
    let manifest = built(vec![
        permission("approve_note", "  "),
        permission("approve_note", ""),
    ]);
    let errs = validate(&manifest);
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    assert_eq!(
        codes,
        [
            "manifest.permission_title",
            "manifest.permission_title",
            "manifest.permission_duplicate"
        ],
        "{}",
        render(&errs)
    );
    assert!(
        errs.iter().all(|e| e.line.is_none()),
        "a code-built manifest has no positions"
    );
}

#[test]
fn a_permission_name_is_never_trimmed_or_folded() {
    assert!(AppPermissionName::parse(" approve_note").is_err());
    assert!(AppPermissionName::parse("approve_note ").is_err());
    assert_ne!(
        got(AppPermissionName::parse("approve_note")),
        got(AppPermissionName::parse("approve_notes"))
    );
    assert!(AppPermissionName::parse(&"n".repeat(65)).is_err());
    let longest = got(AppPermissionName::parse(&format!("n{}", "n".repeat(63))));
    assert_eq!(longest.as_str().len(), 64);
}
