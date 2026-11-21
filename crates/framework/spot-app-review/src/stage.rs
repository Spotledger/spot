//! Opening the bundle and the static checks: everything `Bundle::open` and
//! `check_privileges` report, plus manifest hygiene.

use spot_bundle::{Bundle, BundleError, Policy};
use spot_manifest::PatchIntent;

use crate::finding::{Finding, Severity};
use crate::rules;

/// Opens a bundle for the review.
///
/// A bundle that does not open is **not** an error: the `bundle.*` problems come back as
/// `Blocker` findings and the review continues with an empty bundle, so a report is always
/// produced.
pub fn open_bundle(
    bytes: &[u8],
    config: &crate::config::ReviewConfig,
) -> (Option<Bundle>, Vec<Finding>) {
    match Bundle::open(bytes, &config.bundle.to_limits(), &config.host_info()) {
        Ok(bundle) => (Some(bundle), Vec::new()),
        Err(errors) => (None, errors.into_iter().map(bundle_finding).collect()),
    }
}

/// One `bundle.*` problem as a `Blocker` finding. The rule is the bundle code itself.
fn bundle_finding(error: BundleError) -> Finding {
    Finding::new(
        error.code,
        Severity::Blocker,
        error.path,
        error.message,
        bundle_risk(error.code),
    )
}

/// The risk a `bundle.*` code maps to.
fn bundle_risk(code: &str) -> &'static str {
    match code {
        "bundle.over_privileged" | "bundle.unknown_import" | "bundle.import_without_capability" => {
            rules::RISK_OVER_PRIVILEGE
        }
        _ => rules::RISK_PUBLISHER,
    }
}

/// The static checks over an already-open bundle.
///
/// Everything [`Bundle::check_privileges`] reports comes through here: unknown imports and
/// imports without a declared capability, declarations beyond the policy, and the declared
/// capabilities nothing in the component exercises.
pub fn static_findings(bundle: &Bundle) -> Vec<Finding> {
    let mut findings = Vec::new();
    let report = bundle.check_privileges(&Policy::new());
    for error in report.errors {
        findings.push(Finding::new(
            error.code,
            Severity::High,
            error.path,
            error.message,
            bundle_risk(error.code),
        ));
    }
    for unused in report.unused_capabilities {
        findings.push(Finding::new(
            rules::STATIC_UNUSED_CAPABILITY,
            Severity::Low,
            None,
            unused.note,
            rules::RISK_OVER_PRIVILEGE,
        ));
    }
    hygiene_findings(bundle, &mut findings);
    findings
}

/// Manifest hygiene: a missing title, undeclared or irreversible patches, an open-ended host
/// requirement.
fn hygiene_findings(bundle: &Bundle, findings: &mut Vec<Finding>) {
    if bundle.manifest().app.title.trim().is_empty() {
        findings.push(Finding::new(
            rules::STATIC_TITLE_MISSING,
            Severity::Low,
            Some("manifest: app.title".to_owned()),
            "the manifest has no human title, so an administrator sees only the storage name",
            rules::RISK_OVER_PRIVILEGE,
        ));
    }
    for patch in &bundle.manifest().patches {
        if !patch.reversible
            && !patch.idempotent
            && matches!(patch.intent, PatchIntent::Data | PatchIntent::Schema)
        {
            findings.push(Finding::new(
                rules::STATIC_PATCH_IRREVERSIBLE,
                Severity::High,
                Some(format!("patches: {}", patch.id)),
                format!(
                    "the {} patch is neither reversible nor idempotent, so a failed migration \
                     cannot be undone and a retry cannot be assumed safe",
                    intent_word(patch.intent)
                ),
                rules::RISK_PUBLISHER,
            ));
        }
    }
    let Some(raw) = raw_manifest(bundle) else {
        return;
    };
    patch_declaration_findings(&raw, findings);
    if let Some(text) = raw
        .get("app")
        .and_then(|app| app.get("requires_host"))
        .and_then(toml::Value::as_str)
        && requires_host_is_open(text)
    {
        findings.push(Finding::new(
            rules::STATIC_REQUIRES_HOST_OPEN,
            Severity::Medium,
            Some("app.requires_host".to_owned()),
            format!(
                "the requirement `{text}` sets no upper bound, so the app claims every future \
                 host version"
            ),
            rules::RISK_PUBLISHER,
        ));
    }
}

/// A patch that omits a `reversible` or `idempotent` declaration, read from the raw
/// `manifest.toml` (the typed model defaults a missing declaration to `false`, so only the
/// raw text tells absence from an explicit `false`).
fn patch_declaration_findings(raw: &toml::Value, findings: &mut Vec<Finding>) {
    let Some(rows) = raw.get("patches").and_then(toml::Value::as_array) else {
        return;
    };
    for row in rows {
        let id = row
            .get("id")
            .and_then(toml::Value::as_str)
            .unwrap_or("<unknown>");
        let mut missing: Vec<&str> = Vec::new();
        if row.get("reversible").is_none() {
            missing.push("reversible");
        }
        if row.get("idempotent").is_none() {
            missing.push("idempotent");
        }
        if !missing.is_empty() {
            findings.push(Finding::new(
                rules::STATIC_PATCH_UNDECLARED,
                Severity::Medium,
                Some(format!("patches: {id}")),
                format!(
                    "the patch does not declare {}, so a reviewer cannot tell whether it can be \
                     undone or repeated",
                    missing.join(" or ")
                ),
                rules::RISK_PUBLISHER,
            ));
        }
    }
}

/// The manifest's raw TOML, when it is valid UTF-8 and parses.
fn raw_manifest(bundle: &Bundle) -> Option<toml::Value> {
    let text = std::str::from_utf8(bundle.manifest_bytes()).ok()?;
    toml::from_str(text).ok()
}

/// Whether a `requires_host` requirement string sets no upper bound.
///
/// Read from the raw text because the typed `semver::VersionReq` would need this crate to
/// name `semver`, which the task's dependency list does not allow. The rule: a stated upper
/// bound (`<`, `^`, `~`, `=`) pins the range; a lower-only bound (`>`, `>=`) or the bare
/// wildcard `*` leaves it open.
fn requires_host_is_open(raw: &str) -> bool {
    let text = raw.trim();
    if text.is_empty() {
        return false;
    }
    if text.contains('<') {
        return false;
    }
    if text.contains('>') {
        return true;
    }
    if text.contains(['^', '~', '=']) {
        return false;
    }
    text.starts_with('*')
}

/// The manifest's word for a patch intent.
fn intent_word(intent: PatchIntent) -> &'static str {
    match intent {
        PatchIntent::Schema => "schema",
        PatchIntent::Data => "data",
        PatchIntent::Metadata => "metadata",
    }
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::{bundle, plain_component};

    const GOOD: &str = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"
"#;

    #[test]
    fn a_clean_manifest_has_no_static_finding() {
        let found = static_findings(&bundle(GOOD, &plain_component()));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_missing_title_is_low() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
"#;
        let found = static_findings(&bundle(manifest, &plain_component()));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule, rules::STATIC_TITLE_MISSING);
        assert_eq!(found[0].severity, Severity::Low);
    }

    #[test]
    fn an_undeclared_patch_is_medium_and_an_irreversible_data_patch_is_high() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[[patches]]
id = "0001"
file = "patches/0001.toml"
intent = "data"
"#;
        let found = static_findings(&bundle(manifest, &plain_component()));
        let rules: Vec<&str> = found.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&rules::STATIC_PATCH_UNDECLARED));
        assert!(rules.contains(&rules::STATIC_PATCH_IRREVERSIBLE));
    }

    #[test]
    fn an_explicitly_irreversible_patch_does_not_also_report_undeclared() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[[patches]]
id = "0001"
file = "patches/0001.toml"
intent = "schema"
reversible = false
idempotent = false
"#;
        let found = static_findings(&bundle(manifest, &plain_component()));
        let rules: Vec<&str> = found.iter().map(|f| f.rule.as_str()).collect();
        assert_eq!(rules, vec![rules::STATIC_PATCH_IRREVERSIBLE]);
    }

    #[test]
    fn a_declared_reversible_patch_is_clean() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[[patches]]
id = "0001"
file = "patches/0001.toml"
intent = "data"
reversible = true
idempotent = false
"#;
        let found = static_findings(&bundle(manifest, &plain_component()));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_open_host_requirement_is_medium() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"
requires_host = ">=0.1"
"#;
        let found = static_findings(&bundle(manifest, &plain_component()));
        let rules: Vec<&str> = found.iter().map(|f| f.rule.as_str()).collect();
        assert_eq!(rules, vec![rules::STATIC_REQUIRES_HOST_OPEN]);
    }

    #[test]
    fn a_bounded_host_requirement_is_clean() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"
requires_host = ">=0.1, <1.0"
"#;
        let found = static_findings(&bundle(manifest, &plain_component()));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn the_open_ended_rule_covers_the_obvious_shapes() {
        for open in ["*", ">0.1", ">=0.1", ">= 0.1 , <2"] {
            // The last one has an upper bound and is therefore *not* open; it is listed
            // here to prove the caller does not get a false positive below.
            if open.contains('<') {
                assert!(!requires_host_is_open(open), "{open}");
            } else {
                assert!(requires_host_is_open(open), "{open}");
            }
        }
        for bounded in ["0.1.0", "^0.1", "~0.1", "=0.1.0", "1.*", "0.1"] {
            assert!(!requires_host_is_open(bounded), "{bounded}");
        }
    }

    #[test]
    fn unused_capabilities_are_low() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[capabilities]
doctypes = { read = ["Customer"] }
"#;
        let found = static_findings(&bundle(manifest, &plain_component()));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule, rules::STATIC_UNUSED_CAPABILITY);
        assert_eq!(found[0].severity, Severity::Low);
    }

    #[test]
    fn an_unknown_import_is_high() {
        let found = static_findings(&bundle(
            GOOD,
            &crate::test_support::component_importing("make-coffee"),
        ));
        let unknown: Vec<&Finding> = found
            .iter()
            .filter(|finding| finding.rule == "bundle.unknown_import")
            .collect();
        assert_eq!(unknown.len(), 1, "{found:?}");
        assert_eq!(unknown[0].severity, Severity::High);
    }

    #[test]
    fn declaring_beyond_the_policy_is_high() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[capabilities]
email = true
"#;
        let found = static_findings(&bundle(manifest, &plain_component()));
        assert!(
            found
                .iter()
                .any(|finding| finding.rule == "bundle.over_privileged"
                    && finding.severity == Severity::High),
            "{found:?}"
        );
    }

    #[test]
    fn open_bundle_turns_bad_bytes_into_blockers_not_an_error() {
        let (bundle, findings) = open_bundle(b"not a zip", &crate::config::ReviewConfig::default());
        assert!(bundle.is_none());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, "bundle.bad_archive");
        assert_eq!(findings[0].severity, Severity::Blocker);
    }
}
