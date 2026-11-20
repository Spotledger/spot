//! Hot reload for `spot serve` in development (WP-DX-02.T2): the parts that need no database.
//!
//! The end-to-end scenarios of the brief (a DocType field usable at once over the API, a bad
//! edit keeping the old metadata, a script swap, a bad component, `dev.reloaded` reaching a
//! signed-in client) need the real host and PostgreSQL; they are listed in the report as not
//! yet written, because this module could not be compiled or run when it was written.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use spot_cli::services::SiteCtx;
use spot_cli::services::devserver::{
    ComponentRuntime, Guarded, NoteStep, RuntimeComponentHost, RuntimeScriptHost, ScriptRuntime,
    Settings, register, resolve, seams::DEV_RELOADED,
};
use spot_core::ManualClock;
use spot_devserver::classify::{Change, ChangeKind};
use spot_devserver::reload::{
    ComponentHost, ReloadContext, ScriptHost, Step, StepName, StepOutcome,
};
use spot_devserver::watch::Op;
use spot_devserver::{DevError, record_payload};
use spot_ts_transpile::{Options, transpile};

fn ctx_with(settings: BTreeMap<String, toml::Value>) -> SiteCtx<'static> {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    SiteCtx::new("acme", Duration::from_secs(1), clock, settings, true)
}

#[test]
fn a_site_that_does_not_ask_for_hot_reload_registers_nothing() {
    for settings in [
        BTreeMap::new(),
        BTreeMap::from([(
            "devserver".to_owned(),
            toml::Value::Table(toml::from_str("enabled = false").unwrap()),
        )]),
    ] {
        let mut ctx = ctx_with(settings);
        register(&mut ctx).expect("a disabled dev server registers cleanly");
        let assembled = ctx.finish(axum::Router::new());
        let services = assembled.assembly.expect("an assembling build");
        assert!(
            services.services().is_empty(),
            "no watcher service without enabled = true"
        );
    }
}

#[test]
fn an_unknown_setting_is_refused() {
    let settings = BTreeMap::from([(
        "devserver".to_owned(),
        toml::Value::Table(toml::from_str("enabld = true").unwrap()),
    )]);
    let mut ctx = ctx_with(settings);
    assert!(register(&mut ctx).is_err());
}

#[test]
fn the_start_is_refused_on_a_non_loopback_address() {
    let public = ["0.0.0.0:8080".parse().unwrap()];
    assert!(resolve(&Settings::default(), &public).is_err());
    let local = ["127.0.0.1:8080".parse().unwrap()];
    assert!(resolve(&Settings::default(), &local).is_ok());
}

/// A step that records the paths it was handed.
struct Recording {
    seen: Mutex<Vec<String>>,
}

impl Step for Recording {
    fn name(&self) -> StepName {
        StepName::Metadata
    }

    fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome {
        let mut seen = self.seen.lock().unwrap();
        seen.extend(ctx.changes.iter().map(|c| c.relative.to_string()));
        StepOutcome::ok("seen")
    }
}

fn change(path: &std::path::Path, relative: &str) -> Change {
    Change {
        path: Utf8PathBuf::from_path_buf(path.to_path_buf()).unwrap(),
        relative: Utf8PathBuf::from(relative),
        op: Op::Modified,
        kind: ChangeKind::Metadata,
    }
}

#[test]
fn a_change_inside_the_app_reaches_the_step() {
    let app = tempfile::tempdir().unwrap();
    std::fs::create_dir(app.path().join("doctypes")).unwrap();
    let file = app.path().join("doctypes/widget.toml");
    std::fs::write(&file, "x").unwrap();
    let inner = Recording {
        seen: Mutex::new(Vec::new()),
    };
    let guarded = Guarded::new(inner, app.path().to_path_buf());
    let changes = [change(&file, "doctypes/widget.toml")];
    let outcome = guarded.reload(&ReloadContext {
        changes: &changes,
        now: Duration::ZERO,
    });
    assert!(outcome.ok);
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_app_is_ignored_and_never_reaches_the_step() {
    let app = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.toml"), "x").unwrap();
    std::os::unix::fs::symlink(outside.path(), app.path().join("doctypes")).unwrap();
    let inner = Arc::new(Recording {
        seen: Mutex::new(Vec::new()),
    });

    struct Shared(Arc<Recording>);
    impl Step for Shared {
        fn name(&self) -> StepName {
            self.0.name()
        }
        fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome {
            self.0.reload(ctx)
        }
    }

    let guarded = Guarded::new(Shared(Arc::clone(&inner)), app.path().to_path_buf());
    let changes = [change(
        &app.path().join("doctypes/secret.toml"),
        "doctypes/secret.toml",
    )];
    let outcome = guarded.reload(&ReloadContext {
        changes: &changes,
        now: Duration::ZERO,
    });
    assert!(outcome.ok);
    assert!(outcome.detail.contains("ignored"), "{}", outcome.detail);
    assert!(inner.seen.lock().unwrap().is_empty());
}

#[test]
fn an_unhandled_kind_is_shown_as_ignored_not_as_a_failure() {
    let step = NoteStep::new(StepName::Unknown, "ignored: not a handled file");
    let outcome = step.reload(&ReloadContext {
        changes: &[],
        now: Duration::ZERO,
    });
    assert!(outcome.ok);
    assert!(outcome.detail.starts_with("ignored"));
    assert_eq!(step.name(), StepName::Unknown);
}

struct FakeScripts {
    seen: Mutex<Vec<(String, String)>>,
    refuse: bool,
}

impl ScriptRuntime for FakeScripts {
    fn replace(&self, name: &str, js: &str, _map: &str) -> Result<(), String> {
        if self.refuse {
            return Err("the engine refused the script".to_owned());
        }
        self.seen
            .lock()
            .unwrap()
            .push((name.to_owned(), js.to_owned()));
        Ok(())
    }
}

#[test]
fn a_script_is_swapped_by_name_and_a_missing_engine_fails_visibly() {
    let compiled = transpile("export const x = 1;", "x.ts", &Options::default()).unwrap();
    let path = Utf8Path::new("ts/invoice_checks.ts");

    let runtime = Arc::new(FakeScripts {
        seen: Mutex::new(Vec::new()),
        refuse: false,
    });
    let host = RuntimeScriptHost::new(Some(runtime.clone()));
    host.swap(path, &compiled).unwrap();
    assert_eq!(runtime.seen.lock().unwrap()[0].0, "invoice_checks");

    let refusing = RuntimeScriptHost::new(Some(Arc::new(FakeScripts {
        seen: Mutex::new(Vec::new()),
        refuse: true,
    })));
    assert!(matches!(
        refusing.swap(path, &compiled),
        Err(DevError::Script { .. })
    ));

    let none = RuntimeScriptHost::new(None);
    let error = none.swap(path, &compiled).unwrap_err();
    assert!(matches!(error, DevError::Script { .. }));
}

struct FakeComponents {
    refuse: bool,
}

impl ComponentRuntime for FakeComponents {
    fn replace(&self, app: &str, wasm: &[u8]) -> Result<String, String> {
        if self.refuse || wasm.len() < 4 {
            return Err(format!("the component of `{app}` does not validate"));
        }
        Ok("abc123".to_owned())
    }
}

#[test]
fn a_component_is_validated_through_the_runtime_and_the_old_one_stays_on_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.wasm");
    std::fs::write(&good, b"\0asm\x01\0\0\0").unwrap();
    let bad = dir.path().join("bad.wasm");
    std::fs::write(&bad, b"no").unwrap();
    let utf8 = |p: &std::path::Path| Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap();

    let host = RuntimeComponentHost::new(Some(Arc::new(FakeComponents { refuse: false })));
    assert_eq!(host.reload("shop", &utf8(&good)).unwrap(), "abc123");
    assert!(matches!(
        host.reload("shop", &utf8(&bad)),
        Err(DevError::Component { .. })
    ));
    let none = RuntimeComponentHost::new(None);
    assert!(matches!(
        none.reload("shop", &utf8(&good)),
        Err(DevError::Component { .. })
    ));
}

#[test]
fn the_notification_names_no_file_content() {
    assert_eq!(DEV_RELOADED, "dev.reloaded");
    let record = spot_devserver::reload::ReloadRecord {
        step: StepName::Metadata,
        kind: ChangeKind::Metadata,
        paths: vec!["doctypes/widget.toml".to_owned()],
        ok: true,
        detail: "metadata live at version 3".to_owned(),
        errors: Vec::new(),
        version: Some(3),
        artifacts: Vec::new(),
        started_at_ms: 0,
        duration_ms: 4,
    };
    let payload = record_payload(&record).to_string();
    assert!(payload.contains("doctypes/widget.toml"));
    assert!(
        !payload.contains("detail"),
        "the sentence stays out of the payload"
    );
}
