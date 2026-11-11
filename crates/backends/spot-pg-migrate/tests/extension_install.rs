//! The audited `extension.install` operation against a real PostgreSQL (ADR-087 section 4.3;
//! skipped, with a SKIPPED line, when `SPOT_TEST_PG_URL` is unset).
//!
//! `btree_gist`, `pg_trgm` and `unaccent` are contrib modules and are created in the scratch
//! database. `postgis` is exercised where the server package is present; the no-privilege case
//! needs a role that is not a superuser and skips, saying so, when the test role is one (the
//! privilege rules themselves are covered by the unit tests).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Mutex;

use spot_pg_migrate::{
    AuditSink, ExtensionAuditEvent, ExtensionAuditOp, ExtensionAuditResult, ExtensionContext,
    ExtensionRequest, apply_extension_install, codes, plan_extension_install,
};
use support::*;

#[derive(Default)]
struct Sink(Mutex<Vec<ExtensionAuditEvent>>);

impl AuditSink for Sink {
    fn record(&self, event: ExtensionAuditEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Sink {
    fn take(&self) -> Vec<ExtensionAuditEvent> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn ctx(sink: &Sink) -> ExtensionContext<'_> {
    ExtensionContext {
        actor: "admin-1",
        tenant: "tenant-a",
        audit: sink,
    }
}

async fn installed(rig: &Rig, name: &str) -> Option<String> {
    rig.db
        .raw_client()
        .await
        .query_opt(
            "SELECT extversion::text FROM pg_extension WHERE extname = $1",
            &[&name],
        )
        .await
        .unwrap()
        .map(|r| r.get(0))
}

async fn is_superuser(rig: &Rig) -> bool {
    rig.db
        .raw_client()
        .await
        .query_one(
            "SELECT rolsuper FROM pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn dry_run_then_install_installs_once_and_a_second_install_is_a_no_op() {
    let Some(rig) = rig().await else { return };
    let sink = Sink::default();
    let req = ExtensionRequest::new("pg_trgm");

    let plan = plan_extension_install(&rig.backend, &ctx(&sink), &req)
        .await
        .unwrap();
    assert_eq!(plan.extension, "pg_trgm");
    assert_eq!(plan.schema, "public");
    assert!(plan.privilege_held && !plan.already_present);
    assert_eq!(plan.plan_hash.len(), 64);
    assert_eq!(
        installed(&rig, "pg_trgm").await,
        None,
        "a dry run changes nothing"
    );
    // Repeating the dry run gives the same hash.
    let again = plan_extension_install(&rig.backend, &ctx(&sink), &req)
        .await
        .unwrap();
    assert_eq!(again.plan_hash, plan.plan_hash);

    let out = apply_extension_install(&rig.backend, &ctx(&sink), &plan.plan_hash, &req)
        .await
        .unwrap();
    assert!(!out.already_present);
    assert_eq!(
        installed(&rig, "pg_trgm").await.as_deref(),
        Some(out.version.as_str())
    );

    // Second round: the dry run says already present and the install is a no-op.
    let plan2 = plan_extension_install(&rig.backend, &ctx(&sink), &req)
        .await
        .unwrap();
    assert!(plan2.already_present);
    assert_ne!(plan2.plan_hash, plan.plan_hash, "the state moved");
    let out2 = apply_extension_install(&rig.backend, &ctx(&sink), &plan2.plan_hash, &req)
        .await
        .unwrap();
    assert!(out2.already_present);
    assert_eq!(out2.version, out.version);

    let events = sink.take();
    let results: Vec<_> = events.iter().map(|e| (e.operation, e.result)).collect();
    assert_eq!(
        results,
        vec![
            (ExtensionAuditOp::DryRun, ExtensionAuditResult::Planned),
            (ExtensionAuditOp::DryRun, ExtensionAuditResult::Planned),
            (ExtensionAuditOp::Install, ExtensionAuditResult::Installed),
            (ExtensionAuditOp::DryRun, ExtensionAuditResult::Planned),
            (
                ExtensionAuditOp::Install,
                ExtensionAuditResult::AlreadyPresent
            ),
        ]
    );
    for e in &events {
        assert_eq!(
            (e.actor.as_str(), e.tenant.as_str(), e.extension.as_str()),
            ("admin-1", "tenant-a", "pg_trgm")
        );
        assert!(e.plan_hash.is_some() && e.version.is_some() && e.at_unix_ms > 0);
    }
}

#[tokio::test]
async fn an_off_list_name_is_refused_changes_nothing_and_is_audited() {
    let Some(rig) = rig().await else { return };
    let sink = Sink::default();
    for name in ["hstore", "pg_trgm\"; DROP SCHEMA public CASCADE; --"] {
        let req = ExtensionRequest::new(name);
        let e = plan_extension_install(&rig.backend, &ctx(&sink), &req)
            .await
            .unwrap_err();
        assert_eq!(e.code(), codes::EXTENSION_NOT_ALLOWED);
        let e = apply_extension_install(&rig.backend, &ctx(&sink), "0".repeat(64).as_str(), &req)
            .await
            .unwrap_err();
        assert_eq!(e.code(), codes::EXTENSION_NOT_ALLOWED);
        assert!(!e.to_string().contains("DROP"));
    }
    assert_eq!(installed(&rig, "hstore").await, None);
    let events = sink.take();
    assert_eq!(events.len(), 4, "one event per call, failures included");
    assert!(events.iter().all(|e| matches!(
        e.result,
        ExtensionAuditResult::Failed { code } if code == codes::EXTENSION_NOT_ALLOWED
    )));
    assert!(events.iter().any(|e| e.extension == "hstore"));
    assert!(events.iter().any(|e| e.extension == "<invalid>"));
}

#[tokio::test]
async fn a_stale_plan_hash_is_refused_and_nothing_is_installed() {
    let Some(rig) = rig().await else { return };
    let sink = Sink::default();
    let req = ExtensionRequest::new("unaccent");
    let plan = plan_extension_install(&rig.backend, &ctx(&sink), &req)
        .await
        .unwrap();
    // The database moves between the two calls.
    rig.db
        .raw_client()
        .await
        .batch_execute("CREATE EXTENSION unaccent")
        .await
        .unwrap();
    let e = apply_extension_install(&rig.backend, &ctx(&sink), &plan.plan_hash, &req)
        .await
        .unwrap_err();
    assert_eq!(e.code(), codes::EXTENSION_PLAN_STALE);
    // A made-up hash is refused as well, with the extension absent.
    let req2 = ExtensionRequest::new("btree_gist");
    let e = apply_extension_install(&rig.backend, &ctx(&sink), "deadbeef", &req2)
        .await
        .unwrap_err();
    assert_eq!(e.code(), codes::EXTENSION_PLAN_STALE);
    assert_eq!(installed(&rig, "btree_gist").await, None);
    let events = sink.take();
    assert_eq!(events.len(), 3);
    assert!(matches!(
        events[1].result,
        ExtensionAuditResult::Failed { code } if code == codes::EXTENSION_PLAN_STALE
    ));
}

#[tokio::test]
async fn a_version_bound_the_server_cannot_meet_changes_nothing() {
    let Some(rig) = rig().await else { return };
    let sink = Sink::default();
    let mut req = ExtensionRequest::new("btree_gist");
    req.min_version = Some("99.0".into());
    let e = plan_extension_install(&rig.backend, &ctx(&sink), &req)
        .await
        .unwrap_err();
    assert_eq!(e.code(), codes::EXTENSION_VERSION);
    assert_eq!(installed(&rig, "btree_gist").await, None);
    assert_eq!(sink.take().len(), 1);
}

#[tokio::test]
async fn the_server_trust_flags_match_the_hard_coded_expectation() {
    let Some(rig) = rig().await else { return };
    let rows = rig
        .db
        .raw_client()
        .await
        .query(
            "SELECT v.name::text, v.trusted FROM pg_available_extension_versions v \
             JOIN pg_available_extensions e ON e.name = v.name AND e.default_version = v.version \
             WHERE v.name IN ('postgis', 'btree_gist', 'pg_trgm', 'unaccent')",
            &[],
        )
        .await
        .unwrap();
    assert!(rows.iter().any(|r| r.get::<_, String>(0) == "pg_trgm"));
    for r in rows {
        let (name, trusted): (String, bool) = (r.get(0), r.get(1));
        assert_eq!(trusted, name != "postgis", "{name}");
    }
}

#[tokio::test]
async fn a_role_without_the_privilege_changes_nothing_and_gets_the_typed_error() {
    let Some(rig) = rig().await else { return };
    if is_superuser(&rig).await {
        println!("SKIPPED: the test role is a superuser; the privilege rules are unit tested");
        return;
    }
    let sink = Sink::default();
    // Not trusted, so a non-superuser cannot create it, whether or not the server offers it.
    let req = ExtensionRequest::new("postgis");
    let e = plan_extension_install(&rig.backend, &ctx(&sink), &req)
        .await
        .unwrap_err();
    assert_eq!(e.code(), codes::EXTENSION_PRIVILEGE_MISSING);
    assert!(e.to_string().contains("CREATE EXTENSION postgis;"));
    assert_eq!(installed(&rig, "postgis").await, None);
}

#[tokio::test]
async fn postgis_installs_where_the_package_is_present_and_is_a_hard_failure_where_not() {
    let Some(rig) = rig().await else { return };
    let sink = Sink::default();
    let req = ExtensionRequest::new("postgis");
    match plan_extension_install(&rig.backend, &ctx(&sink), &req).await {
        Ok(plan) => {
            let out = apply_extension_install(&rig.backend, &ctx(&sink), &plan.plan_hash, &req)
                .await
                .unwrap();
            assert_eq!(
                installed(&rig, "postgis").await.as_deref(),
                Some(out.version.as_str())
            );
        }
        Err(e) => {
            // No degraded outcome: the caller gets an error and nothing is created.
            assert!(
                [
                    codes::EXTENSION_UNAVAILABLE,
                    codes::EXTENSION_PRIVILEGE_MISSING
                ]
                .contains(&e.code()),
                "{e}"
            );
            assert_eq!(installed(&rig, "postgis").await, None);
            println!("NOTE: postgis not installable here: {}", e.code());
        }
    }
}

#[tokio::test]
async fn no_secret_appears_in_any_event_or_error() {
    let Some(rig) = rig().await else { return };
    let password = rig.db.config().password;
    let sink = Sink::default();
    let mut texts = Vec::new();
    for name in ["pg_trgm", "hstore", "postgis"] {
        let req = ExtensionRequest::new(name);
        match plan_extension_install(&rig.backend, &ctx(&sink), &req).await {
            Ok(plan) => {
                if let Ok(out) =
                    apply_extension_install(&rig.backend, &ctx(&sink), &plan.plan_hash, &req).await
                {
                    texts.push(format!("{out:?}"));
                }
                texts.push(format!("{plan:?}"));
            }
            Err(e) => texts.push(format!("{e} {e:?}")),
        }
    }
    texts.extend(sink.take().iter().map(|e| format!("{e:?}")));
    let url = std::env::var("SPOT_TEST_PG_URL").unwrap_or_default();
    for t in texts {
        assert!(password.is_empty() || !t.contains(&password), "{t}");
        assert!(url.is_empty() || !t.contains(&url), "{t}");
    }
}
