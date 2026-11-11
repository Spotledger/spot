//! The seeded corpus table test: each app's exact set of rule ids and its verdict.
//!
//! Every app is built at test time (see `corpus/`); no binary is committed. `clean` must be
//! `LooksClean` with no finding above `Info`, and every malicious or resource-hungry app must
//! be flagged.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::vec_init_then_push
)]

mod corpus;

use std::collections::BTreeSet;

use corpus::{CHATTY, ECHO, GROW_FOREVER, HUGE_OUTPUT, SPIN, bundle_bytes, component, core_module};
use spot_app_review::{
    BundleLimits, CallLimits, NullProbe, Probe, ProbeFinding, ReviewConfig, Severity, Stopwatch,
    Verdict, review_at, review_with_stopwatch,
};

/// One seeded app: its bytes, the configuration to review it with, the exact rule ids it must
/// produce and the verdict.
struct Case {
    name: &'static str,
    bytes: Vec<u8>,
    config: ReviewConfig,
    rules: &'static [&'static str],
    verdict: Verdict,
}

/// The clean manifest: a title, no capabilities, no patches.
const CLEAN: &str = r#"
[app]
name = "clean_app"
version = "1.0.0"
title = "Clean App"
"#;

fn config_with(limits: CallLimits) -> ReviewConfig {
    ReviewConfig {
        limits,
        ..ReviewConfig::default()
    }
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();

    // --- clean -----------------------------------------------------------------------------
    cases.push(Case {
        name: "clean",
        bytes: bundle_bytes(CLEAN, &component(&ECHO), &[]),
        config: ReviewConfig::default(),
        rules: &[],
        verdict: Verdict::LooksClean,
    });

    // --- the security scan -----------------------------------------------------------------
    cases.push(Case {
        name: "private_endpoint",
        bytes: bundle_bytes(
            r#"
[app]
name = "private_app"
version = "1.0.0"
title = "Private App"

[capabilities]
network = ["10.0.0.5:443"]
"#,
            &component(&ECHO),
            &[],
        ),
        config: ReviewConfig::default(),
        rules: &[
            "app_review.scan.endpoint.private",
            "app_review.static.unused_capability",
        ],
        verdict: Verdict::NeedsHumanReview,
    });
    cases.push(Case {
        name: "metadata_endpoint",
        bytes: bundle_bytes(
            r#"
[app]
name = "metadata_app"
version = "1.0.0"
title = "Metadata App"

[capabilities]
network = ["169.254.169.254:80"]
"#,
            &component(&ECHO),
            &[],
        ),
        config: ReviewConfig::default(),
        rules: &[
            "app_review.scan.endpoint.metadata",
            "app_review.static.unused_capability",
        ],
        verdict: Verdict::NeedsHumanReview,
    });
    cases.push(Case {
        name: "platform_doctype_write",
        bytes: bundle_bytes(
            r#"
[app]
name = "platform_write_app"
version = "1.0.0"
title = "Platform Write App"

[capabilities]
doctypes = { write = ["User"] }
"#,
            &component(&ECHO),
            &[],
        ),
        config: ReviewConfig::default(),
        rules: &[
            "app_review.scan.doctype.platform_write",
            "app_review.static.unused_capability",
        ],
        verdict: Verdict::NeedsHumanReview,
    });
    cases.push(Case {
        name: "leaked_key_in_asset",
        bytes: bundle_bytes(
            r#"
[app]
name = "leaky_app"
version = "1.0.0"
title = "Leaky App"

[[assets]]
path = "assets/notes.txt"
"#,
            &component(&ECHO),
            &[(
                "assets/notes.txt",
                b"-----BEGIN RSA PRIVATE KEY-----\nMIIEexample\n",
            )],
        ),
        config: ReviewConfig::default(),
        rules: &["app_review.scan.secret.embedded"],
        verdict: Verdict::NeedsHumanReview,
    });
    cases.push(Case {
        name: "remote_script_asset",
        bytes: bundle_bytes(
            r#"
[app]
name = "remote_script_app"
version = "1.0.0"
title = "Remote Script App"

[[assets]]
path = "assets/page.html"
"#,
            &component(&ECHO),
            &[(
                "assets/page.html",
                b"<html><script src=\"https://evil.example/x.js\"></script></html>",
            )],
        ),
        config: ReviewConfig::default(),
        rules: &["app_review.scan.asset.active_content"],
        verdict: Verdict::NeedsHumanReview,
    });

    // --- the resource profile --------------------------------------------------------------
    cases.push(Case {
        name: "infinite_loop",
        bytes: bundle_bytes(CLEAN, &component(&SPIN), &[]),
        config: config_with(CallLimits {
            fuel: 2_000_000,
            wall_clock_ms: 10_000,
            ..CallLimits::default()
        }),
        rules: &["app_review.resource.fuel"],
        verdict: Verdict::NeedsHumanReview,
    });
    cases.push(Case {
        name: "memory_bomb",
        bytes: bundle_bytes(CLEAN, &component(&GROW_FOREVER), &[]),
        config: config_with(CallLimits {
            fuel: 2_000_000_000,
            wall_clock_ms: 10_000,
            memory_bytes: 4 * 1024 * 1024,
            ..CallLimits::default()
        }),
        rules: &["app_review.resource.memory"],
        verdict: Verdict::NeedsHumanReview,
    });
    cases.push(Case {
        name: "host_call_flood",
        bytes: bundle_bytes(CLEAN, &component(&CHATTY), &[]),
        config: config_with(CallLimits {
            fuel: 2_000_000_000,
            wall_clock_ms: 10_000,
            host_calls: 8,
            ..CallLimits::default()
        }),
        rules: &["app_review.resource.host_calls"],
        verdict: Verdict::NeedsHumanReview,
    });
    cases.push(Case {
        name: "huge_output",
        bytes: bundle_bytes(CLEAN, &component(&HUGE_OUTPUT), &[]),
        config: config_with(CallLimits {
            fuel: 2_000_000_000,
            wall_clock_ms: 10_000,
            memory_bytes: 32 * 1024 * 1024,
            output_bytes: 64 * 1024,
            ..CallLimits::default()
        }),
        rules: &["app_review.resource.output_limit"],
        verdict: Verdict::NeedsHumanReview,
    });

    // --- static checks ---------------------------------------------------------------------
    cases.push(Case {
        name: "irreversible_data_patch",
        bytes: bundle_bytes(
            r#"
[app]
name = "patch_app"
version = "1.0.0"
title = "Patch App"

[[patches]]
id = "0001"
file = "patches/0001.toml"
intent = "data"
reversible = false
idempotent = false
"#,
            &component(&ECHO),
            &[],
        ),
        config: ReviewConfig::default(),
        rules: &["app_review.static.patch_irreversible"],
        verdict: Verdict::NeedsHumanReview,
    });
    cases.push(Case {
        name: "unused_capabilities",
        bytes: bundle_bytes(
            r#"
[app]
name = "unused_app"
version = "1.0.0"
title = "Unused App"

[capabilities]
network = ["api.example.com:443"]
files = "read"
doctypes = { read = ["Customer"] }
"#,
            &component(&ECHO),
            &[],
        ),
        config: ReviewConfig::default(),
        rules: &["app_review.static.unused_capability"],
        verdict: Verdict::LooksClean,
    });

    // --- malformed bundles -----------------------------------------------------------------
    cases.push(Case {
        name: "corrupt_zip",
        bytes: b"this is not a zip archive at all".to_vec(),
        config: ReviewConfig::default(),
        rules: &["bundle.bad_archive"],
        verdict: Verdict::Rejected,
    });
    let big = vec![0_u8; 200_000];
    cases.push(Case {
        name: "zip_bomb",
        bytes: bundle_bytes(CLEAN, &component(&ECHO), &[("assets/big.bin", &big)]),
        config: ReviewConfig {
            bundle: BundleLimits {
                max_total_bytes: 16 * 1024,
                max_file_bytes: 4 * 1024,
                ..BundleLimits::default()
            },
            ..ReviewConfig::default()
        },
        rules: &["bundle.too_large"],
        verdict: Verdict::Rejected,
    });
    cases.push(Case {
        name: "core_module_not_component",
        bytes: bundle_bytes(CLEAN, &core_module(), &[]),
        config: ReviewConfig::default(),
        rules: &["bundle.bad_component"],
        verdict: Verdict::Rejected,
    });

    cases
}

/// A stopwatch that never advances, so a resource profile is byte-identical run to run.
struct FrozenStopwatch;

impl Stopwatch for FrozenStopwatch {
    fn mark(&self) -> u128 {
        0
    }
}

#[test]
fn each_seeded_app_produces_its_exact_rule_set_and_verdict() {
    for case in cases() {
        let report = review_at(
            &case.bytes,
            &case.config,
            &NullProbe,
            "2026-01-01T00:00:00Z",
        );
        let actual: BTreeSet<&str> = report.findings.iter().map(|f| f.rule.as_str()).collect();
        let expected: BTreeSet<&str> = case.rules.iter().copied().collect();
        assert_eq!(
            actual, expected,
            "case `{}`: {:?}",
            case.name, report.findings
        );
        assert_eq!(report.verdict, case.verdict, "case `{}`", case.name);
    }
}

#[test]
fn clean_is_looks_clean_with_no_finding_above_info() {
    let case = cases()
        .into_iter()
        .find(|case| case.name == "clean")
        .expect("the corpus has a clean case");
    let report = review_at(&case.bytes, &case.config, &NullProbe, "t");
    assert_eq!(report.verdict, Verdict::LooksClean);
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.severity == Severity::Info),
        "{:?}",
        report.findings
    );
    // The clean app's one profiled call still ran and returned.
    assert_eq!(report.resources.len(), 1);
    assert_eq!(
        report.resources[0].outcome,
        spot_app_review::CallOutcome::Ok
    );
}

#[test]
fn two_runs_are_byte_identical_apart_from_generated_at() {
    let case = cases()
        .into_iter()
        .find(|case| case.name == "private_endpoint")
        .expect("the corpus has the case");
    let first = review_with_stopwatch(
        &case.bytes,
        &case.config,
        &NullProbe,
        &FrozenStopwatch,
        "2026-01-01T00:00:00Z",
    );
    let second = review_with_stopwatch(
        &case.bytes,
        &case.config,
        &NullProbe,
        &FrozenStopwatch,
        "2026-01-01T00:00:00Z",
    );
    assert_eq!(first.to_json(), second.to_json());
    // A different generated_at changes only that field.
    let third = review_with_stopwatch(
        &case.bytes,
        &case.config,
        &NullProbe,
        &FrozenStopwatch,
        "2027-01-01T00:00:00Z",
    );
    assert_eq!(first.to_json().replace("2026", "2027"), third.to_json());
}

#[test]
fn a_hostile_bundle_does_not_affect_the_next_review() {
    let corpus = cases();
    let clean = corpus
        .iter()
        .find(|case| case.name == "clean")
        .expect("the corpus has the clean case");

    // Every hostile app, one after another, then the clean one: a killed call must not leave
    // the process or the review in a bad state.
    for name in [
        "infinite_loop",
        "memory_bomb",
        "host_call_flood",
        "huge_output",
    ] {
        let hostile = corpus
            .iter()
            .find(|case| case.name == name)
            .unwrap_or_else(|| panic!("the corpus has the {name} case"));
        let report = review_at(&hostile.bytes, &hostile.config, &NullProbe, "t");
        assert_eq!(report.verdict, Verdict::NeedsHumanReview, "{name}");
    }

    let second = review_at(&clean.bytes, &clean.config, &NullProbe, "t");
    assert_eq!(second.verdict, Verdict::LooksClean);
    assert!(second.findings.is_empty());
}

/// A probe that always reports one finding, to prove the review surfaces probe results.
struct AlwaysFails;

impl Probe for AlwaysFails {
    fn probe(&self, _bundle: &spot_bundle::Bundle) -> Vec<ProbeFinding> {
        vec![ProbeFinding {
            rule: "app_review.probe.contract".to_owned(),
            where_: Some("doctypes/customer.toml".to_owned()),
            message: "the metadata does not load".to_owned(),
        }]
    }
}

#[test]
fn a_failing_probe_is_a_high_finding() {
    let case = cases()
        .into_iter()
        .find(|case| case.name == "clean")
        .expect("the corpus has a clean case");
    let report = review_at(&case.bytes, &case.config, &AlwaysFails, "t");
    assert_eq!(report.verdict, Verdict::NeedsHumanReview);
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.rule == "app_review.probe.contract")
        .expect("the probe finding is present");
    assert_eq!(finding.severity, Severity::High);
    assert_eq!(finding.risk_id, "R-025");
}

#[test]
fn the_spot_pack_probe_runs_the_author_checks() {
    // A directory with no manifest.toml: `spot-pack`'s check reports `pack.manifest_missing`,
    // which the probe surfaces under its own rule.
    let dir = std::env::temp_dir().join(format!("spot-app-review-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the temp dir is created");
    let probe = spot_app_review::SpotPackProbe::new(&dir);
    let case = cases()
        .into_iter()
        .find(|case| case.name == "clean")
        .expect("the corpus has a clean case");
    let report = review_at(&case.bytes, &case.config, &probe, "t");
    let _ = std::fs::remove_dir_all(&dir);
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.rule == "app_review.probe.contract")
        .expect("the probe finding is present");
    assert_eq!(finding.severity, Severity::High);
    assert!(
        finding.message.contains("pack.manifest_missing"),
        "{}",
        finding.message
    );
}

/// The manifest the two profile tests use: one declared asset, the author's `profile.toml`.
const PROFILED: &str = r#"
[app]
name = "profiled_app"
version = "1.0.0"
title = "Profiled App"

[[assets]]
path = "assets/profile.toml"
"#;

#[test]
fn an_author_profile_names_the_calls() {
    let bytes = bundle_bytes(
        PROFILED,
        &component(&ECHO),
        &[(
            "assets/profile.toml",
            b"[[call]]\nentry = \"on-event\"\nargs = [\"hello\"]\n",
        )],
    );
    let report = review_at(&bytes, &ReviewConfig::default(), &NullProbe, "t");
    assert_eq!(report.verdict, Verdict::LooksClean);
    assert_eq!(report.resources.len(), 1);
    assert_eq!(report.resources[0].entry, "on-event");
    assert_eq!(
        report.resources[0].outcome,
        spot_app_review::CallOutcome::Ok
    );
}

#[test]
fn a_malformed_profile_falls_back_and_is_medium() {
    let bytes = bundle_bytes(
        PROFILED,
        &component(&ECHO),
        &[("assets/profile.toml", b"this is not toml =")],
    );
    let report = review_at(&bytes, &ReviewConfig::default(), &NullProbe, "t");
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.rule == "app_review.resource.profile_invalid")
        .expect("the malformed profile is reported");
    assert_eq!(finding.severity, Severity::Medium);
    // The review still profiled the default entry.
    assert_eq!(report.resources.len(), 1);
    assert_eq!(report.resources[0].entry, "on-event");
    assert_eq!(report.verdict, Verdict::LooksClean);
}
