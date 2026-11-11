//! The manifest contract: samples parse to typed values, every error code fires with its
//! position, all errors come out at once, and hook-point names round-trip with the contract.

use std::str::FromStr;
use std::time::Duration;

use spot_app_api::prelude::Name;
use spot_app_api::{HookPoint, Operation, OverrideKind, Phase, StepName, Target};
use spot_manifest::{
    AppDependency, AppManifest, AppMeta, Capabilities, EventSub, FileAccess, ManifestError,
    PatchIntent, Queue, SESSION_EVENT_NAMES, SeamKind, SessionEvent, parse, parse_hook_point,
    validate,
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

/// Parses a sample, failing with its errors.
#[track_caller]
fn parsed(rel: &str) -> AppManifest {
    match parse(&sample(rel), rel) {
        Ok(manifest) => manifest,
        Err(errs) => panic!("{rel} must parse, got: {}", render(&errs)),
    }
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

/// Reads a sample from `tests/samples`.
fn sample(rel: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/samples/");
    match std::fs::read_to_string(format!("{path}{rel}")) {
        Ok(text) => text,
        Err(err) => panic!("sample {path}{rel} must be readable: {err}"),
    }
}

/// Asserts a file reports exactly one error: this code, at this position.
#[track_caller]
fn one_error(rel: &str, code: &str, line: u32, col: u32) {
    let errs = match parse(&sample(rel), rel) {
        Ok(manifest) => panic!("{rel} must not parse: {manifest:?}"),
        Err(errs) => errs,
    };
    let [err] = errs.as_slice() else {
        panic!("{rel}: expected exactly one error, got:\n{}", render(&errs));
    };
    assert_eq!(err.code, code, "{rel}: {err}");
    assert_eq!((err.line, err.col), (Some(line), Some(col)), "{rel}: {err}");
}

#[test]
fn minimal_sample_parses() {
    let manifest = parsed("minimal.toml");
    assert_eq!(manifest.app.name.as_str(), APP);
    assert_eq!(manifest.app.version.to_string(), "0.1.0");
    assert_eq!(manifest.app.title, "");
    assert_eq!(manifest.app.requires_host, None);
    assert!(manifest.app.depends.is_empty());
    assert!(manifest.events.is_empty() && manifest.schedules.is_empty());
    assert_eq!(manifest.capabilities, Capabilities::default());
    assert!(validate(&manifest).is_empty());
}

#[test]
fn full_sample_parses_to_typed_values() {
    let manifest = parsed("full.toml");
    assert_eq!(got(Name::new(APP)), manifest.app.name);
    assert_eq!(manifest.app.version.to_string(), "0.3.1");
    assert_eq!(
        manifest.app.requires_host,
        Some(got(semver::VersionReq::parse(">=0.1, <0.2")))
    );
    assert_eq!(
        manifest.app.depends,
        [AppDependency {
            app: got(Name::new("spot_foundation")),
            version: got(semver::VersionReq::parse("^0.2")),
        }]
    );

    assert_eq!(manifest.events.len(), 3);
    assert_eq!(manifest.events[0].doctype, "Sales Invoice");
    assert_eq!(
        manifest.events[0].event,
        HookPoint::of(Phase::Validate, Target::Save)
    );
    assert_eq!(manifest.events[0].priority, 500);
    assert_eq!(manifest.events[0].budget, Duration::from_millis(200));
    assert_eq!(
        manifest.events[1].event,
        HookPoint::of(Phase::Committed, Target::Op(Operation::Post))
    );
    assert_eq!(
        manifest.events[2].event,
        HookPoint::FieldChanged("credit_limit".to_owned())
    );
    assert_eq!(manifest.events[2].priority, 0, "priority defaults to 0");
    assert_eq!(
        manifest.events[2].budget,
        Duration::from_millis(200),
        "budget defaults to 200 ms"
    );

    assert_eq!(manifest.session_events.len(), 1);
    assert_eq!(manifest.session_events[0].event, SessionEvent::Login);
    assert_eq!(
        manifest.session_events[0].budget,
        Duration::from_millis(100)
    );

    assert_eq!(manifest.boot.len(), 1);
    assert_eq!(
        manifest.boot[0].budget, None,
        "boot states no budget by default"
    );

    assert_eq!(manifest.schedules.len(), 2);
    assert_eq!(manifest.schedules[0].cron.as_str(), "0 */15 * * * *");
    assert_eq!(manifest.schedules[0].queue, Queue::Long);
    assert_eq!(manifest.schedules[0].budget, Duration::from_millis(600_000));
    assert_eq!(
        manifest.schedules[1].queue,
        Queue::Default,
        "queue defaults to default"
    );
    assert_eq!(manifest.schedules[1].budget, Duration::from_millis(600_000));

    assert_eq!(manifest.seams.len(), 2);
    assert_eq!(manifest.seams[0].kind, SeamKind::Provide);
    assert_eq!(manifest.seams[0].name, "tax.calculate");
    assert_eq!(manifest.seams[0].version, 1);
    assert_eq!(manifest.seams[0].reason, None);
    assert_eq!(manifest.seams[1].kind, SeamKind::Replace);
    assert_eq!(manifest.seams[1].name, "erp_accounts:report_render");
    assert_eq!(manifest.seams[1].version, 2);
    assert_eq!(manifest.seams[1].reason.as_deref(), Some("Commission rows"));

    assert_eq!(manifest.methods.len(), 1);
    assert_eq!(manifest.methods[0].name, got(Name::new("make_payment")));
    assert!(manifest.methods[0].mutates);

    assert_eq!(manifest.functions.len(), 1);
    assert_eq!(manifest.functions[0].module, got(Name::new("pricing")));
    assert_eq!(
        manifest.functions[0].http,
        Some(vec![
            spot_manifest::HttpVerb::Get,
            spot_manifest::HttpVerb::Post
        ])
    );
    assert!(!manifest.functions[0].guest);

    assert_eq!(manifest.fixtures[0].file.as_str(), "fixtures/roles.json");
    assert_eq!(manifest.fixtures[0].step, None);

    let ids: Vec<&str> = manifest.patches.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "2026_09_rename_price_list_field",
            "2026_10_add_commission_column"
        ]
    );
    assert!(manifest.patches[0].idempotent);
    assert!(!manifest.patches[0].reversible);
    assert_eq!(manifest.patches[1].intent, PatchIntent::Schema);
    assert!(!manifest.patches[1].idempotent, "flags default to false");

    assert_eq!(manifest.routes[0].path.as_str(), "/portal/orders");
    assert_eq!(manifest.assets[0].path.as_str(), "assets/selling.css");

    assert_eq!(manifest.capabilities.network[0].host, "api.fbr.gov.pk");
    assert_eq!(manifest.capabilities.network[0].port, 443);
    assert!(manifest.capabilities.email);
    assert_eq!(manifest.capabilities.files, FileAccess::ReadWrite);
    assert_eq!(manifest.capabilities.doctypes.read, ["Customer"]);
    assert_eq!(manifest.capabilities.doctypes.write, ["Sales Invoice"]);
}

#[test]
fn step_decls_feed_the_seam_registry() {
    let manifest = parsed("full.toml");
    let decls = manifest.step_decls();
    assert_eq!(decls.len(), 3);
    assert_eq!(decls[0].0, "Sales Invoice");
    assert_eq!(
        decls[0].1.name,
        got(StepName::new("erp_selling:check_credit_limit"))
    );
    assert_eq!(
        decls[0].1.point,
        HookPoint::of(Phase::Validate, Target::Save)
    );
    assert_eq!(decls[0].1.priority, 500);
    assert_eq!(decls[0].1.budget, Duration::from_millis(200));
    assert_eq!(decls[2].0, "Customer");
    assert_eq!(
        decls[2].1.name,
        got(StepName::new("erp_selling:recheck_limit"))
    );
}

#[test]
fn overrides_are_contract_step_overrides() {
    let manifest = parsed("full.toml");
    let overrides = manifest.overrides();
    assert_eq!(overrides.len(), 2);
    assert_eq!(
        overrides[0].target,
        got(StepName::new("erp_accounts:post_gl"))
    );
    assert_eq!(overrides[0].kind, OverrideKind::Wrap);
    assert_eq!(
        overrides[0].by,
        Some(got(StepName::new("erp_selling:post_gl_with_commission")))
    );
    assert_eq!(overrides[0].reason, "Commission accrual on posting");
    assert_eq!(overrides[1].kind, OverrideKind::Disable);
    assert_eq!(overrides[1].by, None, "a disable runs no step");
}

// One test per error code: the code, the line and the column, as the sample declares them.

#[test]
fn err_invalid_toml() {
    one_error("errors/invalid_toml.toml", "manifest.invalid_toml", 3, 10);
}
#[test]
fn err_unknown_key() {
    one_error("errors/unknown_key.toml", "manifest.unknown_key", 5, 1);
}
#[test]
fn err_missing_key() {
    one_error("errors/missing_key.toml", "manifest.missing_key", 1, 1);
}
#[test]
fn err_invalid_type() {
    one_error("errors/invalid_type.toml", "manifest.invalid_type", 9, 13);
}
#[test]
fn err_invalid_name() {
    one_error("errors/invalid_name.toml", "manifest.invalid_name", 2, 8);
}
#[test]
fn err_invalid_version() {
    one_error(
        "errors/invalid_version.toml",
        "manifest.invalid_version",
        3,
        11,
    );
}
#[test]
fn err_invalid_event() {
    one_error("errors/invalid_event.toml", "manifest.invalid_event", 7, 9);
}
#[test]
fn err_phase_rule() {
    one_error("errors/phase_rule.toml", "manifest.phase_rule", 7, 9);
}
#[test]
fn err_invalid_session_event() {
    one_error(
        "errors/invalid_session_event.toml",
        "manifest.invalid_session_event",
        6,
        9,
    );
}
#[test]
fn err_invalid_step() {
    one_error("errors/invalid_step.toml", "manifest.invalid_step", 8, 8);
}
#[test]
fn err_foreign_step() {
    one_error("errors/foreign_step.toml", "manifest.foreign_step", 8, 8);
}
#[test]
fn err_duplicate_step() {
    one_error(
        "errors/duplicate_step.toml",
        "manifest.duplicate_step",
        13,
        8,
    );
}
#[test]
fn err_budget_out_of_range() {
    one_error(
        "errors/budget_out_of_range.toml",
        "manifest.budget_out_of_range",
        9,
        13,
    );
}
#[test]
fn err_duplicate_schedule() {
    one_error(
        "errors/duplicate_schedule.toml",
        "manifest.duplicate_schedule",
        13,
        8,
    );
}
#[test]
fn err_invalid_cron() {
    one_error("errors/invalid_cron.toml", "manifest.invalid_cron", 7, 8);
}
#[test]
fn err_invalid_queue() {
    one_error("errors/invalid_queue.toml", "manifest.invalid_queue", 9, 9);
}
#[test]
fn err_invalid_intent() {
    one_error(
        "errors/invalid_intent.toml",
        "manifest.invalid_intent",
        8,
        10,
    );
}
#[test]
fn err_duplicate_patch() {
    one_error(
        "errors/duplicate_patch.toml",
        "manifest.duplicate_patch",
        11,
        6,
    );
}
#[test]
fn err_patch_order() {
    one_error("errors/patch_order.toml", "manifest.patch_order", 11, 6);
}
#[test]
fn err_override_missing_by() {
    one_error(
        "errors/override_missing_by.toml",
        "manifest.override_missing_by",
        6,
        10,
    );
}
#[test]
fn err_override_with_by() {
    one_error(
        "errors/override_with_by.toml",
        "manifest.override_with_by",
        6,
        10,
    );
}
#[test]
fn err_invalid_override() {
    one_error(
        "errors/invalid_override.toml",
        "manifest.invalid_override",
        7,
        8,
    );
}
#[test]
fn err_invalid_seam() {
    one_error("errors/invalid_seam.toml", "manifest.invalid_seam", 6, 20);
}
#[test]
fn err_invalid_path() {
    one_error("errors/invalid_path.toml", "manifest.invalid_path", 7, 8);
}
#[test]
fn err_invalid_route() {
    one_error("errors/invalid_route.toml", "manifest.invalid_route", 6, 8);
}
#[test]
fn err_invalid_method() {
    one_error(
        "errors/invalid_method.toml",
        "manifest.invalid_method",
        8,
        9,
    );
}
#[test]
fn err_invalid_capability() {
    one_error(
        "errors/invalid_capability.toml",
        "manifest.invalid_capability",
        6,
        12,
    );
}

#[test]
fn five_errors_all_report_at_once() {
    let source = format!(
        "[app]\nname = \"{APP}\"\nversion = \"0.1.0\"\n\n[oops]\n\n[[events]]\ndoctype = \"X 1\"\nevent = \"nope\"\nstep = \"{APP}:a\"\n\n[[events]]\ndoctype = \"X 1\"\nevent = \"load\"\nstep = \"other:b\"\nbudget_ms = 0\n\n[[schedules]]\nname = \"s\"\ncron = \"bad\"\nstep = \"{APP}:s\"\n\n[[overrides]]\ntarget = \"other:t\"\nkind = \"wrap\"\nreason = \"x\"\n"
    );
    let errs = match parse(&source, "five.toml") {
        Ok(manifest) => panic!("must not parse: {manifest:?}"),
        Err(errs) => errs,
    };
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    for expected in [
        "manifest.unknown_key",
        "manifest.invalid_event",
        "manifest.foreign_step",
        "manifest.budget_out_of_range",
        "manifest.invalid_cron",
        "manifest.override_missing_by",
    ] {
        assert!(codes.contains(&expected), "missing {expected} in {codes:?}");
    }
    assert_eq!(codes.len(), 6, "{codes:?}");
    assert!(
        errs.iter().all(|e| e.line.is_some() && e.col.is_some()),
        "every error has a position:\n{}",
        render(&errs)
    );
}

#[test]
fn hook_point_names_round_trip() {
    let mut points = vec![
        HookPoint::Load,
        HookPoint::FieldChanged("price_list".to_owned()),
    ];
    for phase in [
        Phase::Before,
        Phase::Validate,
        Phase::After,
        Phase::Committed,
    ] {
        for target in [
            Target::Save,
            Target::Op(Operation::Create),
            Target::Op(Operation::Update),
            Target::Op(Operation::Post),
            Target::Op(Operation::Cancel),
            Target::Op(Operation::Delete),
            Target::Op(Operation::Rename),
            Target::Op(Operation::UpdatePosted),
        ] {
            points.push(HookPoint::of(phase, target));
        }
    }
    assert_eq!(points.len(), 34, "every point the contract can produce");
    for point in points {
        let name = point.name();
        let parsed = match parse_hook_point(&name) {
            Ok(parsed) => parsed,
            Err(err) => panic!("{name}: {err}"),
        };
        assert_eq!(parsed, point, "{name} must round-trip");
        assert_eq!(parsed.name(), name);
    }
}

#[test]
fn session_event_names_are_the_three() {
    for (raw, expected) in [
        ("login", SessionEvent::Login),
        ("logout", SessionEvent::Logout),
        ("session_created", SessionEvent::SessionCreated),
    ] {
        assert!(SESSION_EVENT_NAMES.contains(&raw), "{raw} must be listed");
        let source = format!(
            "[app]\nname = \"{APP}\"\nversion = \"0.1.0\"\n\n[[session_events]]\nevent = \"{raw}\"\nstep = \"{APP}:s\"\n"
        );
        let manifest = match parse(&source, "session.toml") {
            Ok(manifest) => manifest,
            Err(errs) => panic!("{raw}: {}", render(&errs)),
        };
        assert_eq!(manifest.session_events[0].event, expected);
    }
}

#[test]
fn validate_checks_code_built_manifests_too() {
    let manifest = AppManifest {
        app: AppMeta {
            name: got(Name::new("erp_selling")),
            version: got(semver::Version::parse("0.1.0")),
            title: String::new(),
            requires_host: None,
            depends: Vec::new(),
        },
        events: vec![EventSub {
            doctype: "Sales Invoice".to_owned(),
            event: HookPoint::Load,
            step: got(StepName::new("erp_accounts:x")),
            priority: 0,
            budget: Duration::ZERO,
        }],
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
        permissions: Vec::new(),
    };
    let errs = validate(&manifest);
    let codes: Vec<&str> = errs.iter().map(|e| e.code).collect();
    assert!(codes.contains(&"manifest.foreign_step"), "{codes:?}");
    assert!(codes.contains(&"manifest.budget_out_of_range"), "{codes:?}");
    assert!(
        errs.iter().all(|e| e.line.is_none()),
        "a code-built manifest has no positions"
    );
}

#[test]
fn cron_accepts_the_scheduler_grammar() {
    assert!(spot_manifest::Cron::from_str("0 */15 * * * *").is_ok());
    assert!(spot_manifest::Cron::from_str("0 61 * * * *").is_err());
    assert!(spot_manifest::Cron::from_str("0 0 * * *").is_err());
    assert!(
        spot_manifest::Cron::from_str("0 0 * * * 7").is_ok(),
        "7 is Sunday"
    );
}
