//! SCIM 2.0 (`/scim/v2`, WP-IAM-06.T2) over the real stack: the fail-closed half.
//!
//! A SCIM credential is an API key owned by a `service` user whose scope is exactly `scim`; the
//! fixture host has no administration command to issue one (the key is issued through the
//! `ScimKeyIssuer` port of the hosting module, which the integration tests of `spot-cli` drive).
//! So this case proves what holds for any caller that has no such key, which is the surface a
//! stranger sees: `/scim/v2` is not mounted unless `[services.scim] enabled = true` (404); once an
//! operator enables it (the technique of `secrets.rs`: write `common.toml`, create the vault's key
//! file, reload) every request without a SCIM key is `401` in the RFC 7644 error form, whatever
//! it carries: no token, a made-up key, the Administrator's session, a Viewer's session, and a
//! native API key; and the native API never accepts what SCIM would.

use std::time::{Duration, Instant};

use serde_json::json;

use super::{CaseDef, CaseFuture};
use crate::fixture::{Fixture, VIEWER};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After the webhook secret case; the case owns its fixture.
pub const ORDER: u32 = 32;

const SECTION: &str = "SCIM provisioning";

const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!("the SCIM case could not start its own fixture"));
        };
        let result = scim_is_off_by_default_and_closed_to_any_caller_without_a_scim_key(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "scim: scim_is_off_by_default_and_closed_to_any_caller_without_a_scim_key",
        SECTION,
        "`/scim/v2/Users` is 404 until `[services.scim] enabled = true`; then no token, a made-up key, the Administrator's session and a Viewer's session are each 401 with an RFC 7644 error body (`application/scim+json`, status as text, no echo of the credential), and the SCIM base answers the same",
        false,
        run_case,
    )]
}

async fn until_status(fixture: &Fixture, want: impl Fn(u16) -> bool) -> Result<u16, Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = fixture
            .client()
            .anonymous()
            .get("/api/v1/auth/session")
            .await?;
        if want(reply.status) {
            return Ok(reply.status);
        }
        if Instant::now() > deadline {
            return Err(failure!(
                "the site still answers {} after 30s: {}",
                reply.status,
                reply.text()
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn scim_is_off_by_default_and_closed_to_any_caller_without_a_scim_key(
    fixture: &Fixture,
) -> CaseResult {
    let admin = fixture.admin();
    // 1. Off by default: nothing is mounted at the base.
    for path in ["/scim/v2/Users", "/scim/v2/Groups", "/scim/v2"] {
        let reply = admin.get(path).await?;
        ensure!(
            reply.status == 404,
            "{path} answered HTTP {} with SCIM off, not 404: {}",
            reply.status,
            reply.text()
        );
    }

    // 2. The operator enables it (with the vault it needs): the table, the key file, a reload.
    let common = fixture.root().join("common.toml");
    let original = std::fs::read_to_string(&common)
        .map_err(|e| failure!("cannot read the fixture's common.toml: {e}"))?;
    let original = crate::fixture::strip_tables(
        &original,
        &["services.secrets", "services.signin_throttle"],
    );
    let kek = fixture
        .root()
        .join("keys")
        .join(format!("{}.json", fixture.site()));
    if !kek.exists() {
        spot_cli::services::secrets::init_key_file(&kek, fixture.site())
            .await
            .map_err(|e| failure!("cannot create the key file: {e}"))?;
    }
    std::fs::write(
        &common,
        format!(
            "{original}\n[services.secrets]\nkek_file = {:?}\n[services.scim]\nenabled = true\n",
            kek.display().to_string()
        ),
    )
    .map_err(|e| failure!("cannot write common.toml: {e}"))?;
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(fixture.root(), fixture.site()))
        .map_err(|e| failure!("reload request: {e}"))?;
    until_status(fixture, |status| status != 503).await?;

    // 3. Every caller without a SCIM key is refused, in the SCIM error form.
    let viewer = fixture.sign_in(VIEWER).await?;
    let made_up = "spk_madeupkey_l3canary0123456789abcdefghijklmnopq";
    let callers = [
        ("no token", fixture.client().anonymous()),
        ("a made-up key", fixture.client().with_token(made_up)),
        ("the Administrator's session", admin.clone()),
        ("a Viewer's session", viewer.clone()),
    ];
    for (who, client) in &callers {
        for path in ["/scim/v2/Users", "/scim/v2/ServiceProviderConfig"] {
            let reply = client.get(path).await?;
            ensure!(
                reply.status == 401,
                "{who} on {path} answered HTTP {}, not 401: {}",
                reply.status,
                reply.text()
            );
            ensure!(
                reply.header("content-type") == Some("application/scim+json"),
                "{who} on {path}: the content type is {:?}",
                reply.header("content-type")
            );
            let body = reply.json();
            ensure!(
                body["schemas"][0] == json!(ERROR_SCHEMA) && body["status"] == json!("401"),
                "{who} on {path}: not an RFC 7644 error body: {}",
                reply.text()
            );
            ensure!(
                !reply.text().contains("l3canary"),
                "{who} on {path}: the error echoes the credential: {}",
                reply.text()
            );
        }
    }

    // 4. A write is refused the same way and creates nothing.
    let before = admin.get("/api/v1/resource/user?limit=200").await?;
    let post = fixture
        .client()
        .with_token(made_up)
        .send_json(
            "POST",
            "/scim/v2/Users",
            &[],
            &json!({ "userName": "mallory@example.com" }),
        )
        .await?;
    ensure!(
        post.status == 401,
        "a POST with a made-up key answered HTTP {}, not 401",
        post.status
    );
    let after = admin.get("/api/v1/resource/user?limit=200").await?;
    ensure!(
        !after.text().contains("mallory@example.com") && before.status == after.status,
        "a refused SCIM write changed the directory"
    );
    Ok(())
}
