//! WP-IAM-11.T24: `spot serve` assembles the sign-in stack: the sign-in service over the storage
//! stores and the coordinator throttle, the other credential doors admitted through the same
//! handle, and the outside-role check at first sign-in. Real PostgreSQL and a real host (every
//! test skips, with no output, when `SPOT_TEST_PG_URL` is unset).
//!
//! What is proved: a host signs in and throttles after the configured failures, and the lock
//! survives a restart of the host (the counters are in the coordinator tables, not in the
//! process); a SCIM credential that keeps failing is throttled with `429` and `Retry-After`,
//! never answered with a `401` detail; a wrong current password in a password change is counted
//! by the throttle's password-change door; a portal user holding a desk role stays `invited` at
//! first sign-in; a host without the throttle's prerequisites refuses to start.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::path::PathBuf;

use common::{Env, Started};
use serde_json::{Value as Json, json};
use spot_cli::services::secrets as svc;

const PLAIN_USER: &str = "plain@example.com";
const PORTAL_USER: &str = "portal@example.com";
const WRONG: &str = "definitely-not-the-password-1";

/// A host with a vault and a small per-account limit on the sign-in throttle.
struct Rig {
    env: Env,
    dir: PathBuf,
    started: Option<Started>,
    first: String,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    /// `None` when no PostgreSQL is configured. `scim` also turns `[services.scim]` on.
    async fn new(scim: bool) -> Option<Rig> {
        let dir = std::env::temp_dir().join(format!(
            "spot_signin_host_{}",
            spot_app_api::prelude::RecordId::new_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut services = format!(
            "[services.secrets]\nkek_file = {:?}\n[services.signin_throttle]\nper_account = {{ capacity = 3, per_seconds = 3600 }}\n",
            format!("{}/{{site}}.json", dir.display())
        );
        if scim {
            services.push_str("[services.scim]\nenabled = true\n");
        }
        let env = Env::new(&services).await?;
        let first = env.site();
        svc::init_key_file(&dir.join(format!("{}.json", env.site)), &env.site)
            .await
            .unwrap();
        env.user(PLAIN_USER, common::ADMIN_PASSWORD);
        let started = common::start(&env).await;
        Some(Rig {
            env,
            dir,
            started: Some(started),
            first,
        })
    }

    fn started(&self) -> &Started {
        self.started.as_ref().unwrap()
    }

    async fn call(
        &self,
        token: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Json>,
    ) -> common::Reply {
        common::http(
            self.started().addr,
            &self.env.site,
            token,
            method,
            path,
            headers,
            body,
        )
        .await
    }

    async fn login(&self, user: &str, password: &str) -> common::Reply {
        common::login(self.started().addr, &self.env.site, user, password).await
    }

    /// Stops the host and starts it again on the same database (a node restart).
    async fn restart(&mut self) {
        if let Some(started) = self.started.take() {
            started.finish().await;
        }
        self.started = Some(common::start(&self.env).await);
    }

    async fn finish(mut self) {
        if let Some(started) = self.started.take() {
            started.finish().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_signs_in_throttles_after_the_configured_failures_and_keeps_the_lock_across_a_restart()
 {
    let Some(mut rig) = Rig::new(false).await else {
        return;
    };
    // A right password signs in.
    let ok = rig.login(PLAIN_USER, common::ADMIN_PASSWORD).await;
    common::expect(&ok, 200);
    // Wrong passwords are counted; within a few attempts the throttle answers `429`.
    let mut limited = None;
    for _ in 0..8 {
        let reply = rig.login(PLAIN_USER, WRONG).await;
        if reply.status == 429 {
            limited = Some(reply);
            break;
        }
        common::expect(&reply, 401);
    }
    let limited = limited.expect("the account is throttled after its failures");
    // The refusal is the same for the right password: the lock is not a wrong-password answer.
    let right = rig.login(PLAIN_USER, common::ADMIN_PASSWORD).await;
    assert_eq!(right.status, 429);
    assert!(limited.header("retry-after").is_some());
    // The lock is in the coordinator tables, so a restarted node still refuses.
    rig.restart().await;
    let after = rig.login(PLAIN_USER, common::ADMIN_PASSWORD).await;
    assert_eq!(after.status, 429);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scim_credential_that_keeps_failing_is_throttled_and_never_gets_a_401_detail() {
    let Some(rig) = Rig::new(true).await else {
        return;
    };
    let mut statuses = Vec::new();
    let mut last = None;
    for _ in 0..8 {
        let reply = rig
            .call(Some("not-a-scim-key"), "GET", "/scim/v2/Users", &[], None)
            .await;
        statuses.push(reply.status);
        last = Some(reply);
    }
    assert_eq!(statuses.first(), Some(&401), "{statuses:?}");
    assert_eq!(statuses.last(), Some(&429), "{statuses:?}");
    let last = last.unwrap();
    assert!(last.header("retry-after").is_some());
    let text = String::from_utf8_lossy(&last.bytes).into_owned();
    assert!(
        !text.contains("A SCIM credential is required"),
        "a throttled attempt is not a 401 detail: {text}"
    );
    assert!(!text.contains("not-a-scim-key"));
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn change_password_with_a_wrong_current_password_is_counted() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let admin = common::admin(rig.started().addr, &rig.env.site, &rig.first).await;
    let mut statuses = Vec::new();
    for _ in 0..8 {
        let reply = rig
            .call(
                Some(&admin),
                "POST",
                "/api/v1/auth/password",
                &[],
                Some(json!({"current": WRONG, "new": "another-long-staple-9-Quartz"})),
            )
            .await;
        statuses.push(reply.status);
    }
    assert_eq!(statuses.first(), Some(&401), "{statuses:?}");
    assert!(statuses.contains(&429), "{statuses:?}");
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_portal_user_holding_a_desk_role_stays_invited_at_first_sign_in() {
    let Some(rig) = Rig::new(false).await else {
        return;
    };
    let addr = rig.started().addr;
    let site = rig.env.site.clone();
    let admin = common::admin(addr, &site, &rig.first).await;
    // An invited user with a one-time secret, made a portal user, holding a role.
    let secret = common::secret(&common::ok(
        &rig.env.root.0,
        &["add-user", &site, PORTAL_USER],
    ));
    let id = common::user_id(addr, &site, &admin, PORTAL_USER).await;
    let read = rig
        .call(
            Some(&admin),
            "GET",
            &format!("/api/v1/resource/user/{id}"),
            &[],
            None,
        )
        .await;
    common::expect(&read, 200);
    let patched = rig
        .call(
            Some(&admin),
            "PATCH",
            &format!("/api/v1/resource/user/{id}"),
            &[("If-Match", read.etag.as_str())],
            Some(json!({"user_type": "portal"})),
        )
        .await;
    common::expect(&patched, 200);
    let role = common::role(addr, &site, &admin, "desk-clerk").await;
    common::assign(addr, &site, &admin, &id, &role).await;
    // The one-time secret signs in (a restricted session); choosing a password is refused.
    let first = rig.login(PORTAL_USER, &secret).await;
    common::expect(&first, 200);
    let token = first.body["data"]["token"].as_str().unwrap().to_owned();
    let changed = rig
        .call(
            Some(&token),
            "POST",
            "/api/v1/auth/password",
            &[],
            Some(json!({"current": secret, "new": common::ADMIN_PASSWORD})),
        )
        .await;
    common::expect(&changed, 403);
    let after = rig
        .call(
            Some(&admin),
            "GET",
            &format!("/api/v1/resource/user/{id}"),
            &[],
            None,
        )
        .await;
    common::expect(&after, 200);
    assert_eq!(after.body["data"]["state"], json!("invited"));
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_without_the_throttle_prerequisites_leaves_the_site_unavailable() {
    // No `[services.secrets]`: the throttle cannot derive its salt, so nobody could be admitted.
    let Some(env) = Env::new("# no-secrets\n").await else {
        return;
    };
    let _first = env.site();
    let started = common::start(&env).await;
    let reply = common::http(
        started.addr,
        &env.site,
        None,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"user": "administrator", "password": "x"})),
    )
    .await;
    assert_eq!(
        reply.status, 503,
        "a site without the sign-in throttle must not serve: {}",
        reply.body
    );
    started.finish().await;
}
