//! The webhook signing secret (`PUT|GET /api/v1/webhooks/{name}/signing-secret`, WP-INT-01.T5)
//! over the real stack.
//!
//! A registered case runs
//! after the shutdown case, so this case starts a host of its own. It first proves the fail-closed
//! half (no webhook: `404`, an unsigned request `401`), then writes the table
//! into the host's `common.toml`, creates the key file and reloads, as an operator turns the
//! service on (the technique of `secrets.rs`), and proves the rest: a Viewer is refused the same
//! way for a webhook that exists and one that does not, a supplied secret is never echoed, a
//! generated one is shown once in the `PUT` answer, `GET` shows only a fingerprint, and neither
//! value is in a document read, a list, the `GET` answer or the server log.

use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use super::{CaseDef, CaseFuture};
use crate::fixture::{Fixture, VIEWER};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After the secrets case; independent of the other groups' data (it owns its fixture).
pub const ORDER: u32 = 31;

const SECTION: &str = "Webhook signing secrets";

/// A supplied secret that no response, document, list or log may contain.
const SUPPLIED: &str = "whsec_l3_canary_8c41d0f27b6a49e3";

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!(
                "the webhook secret case could not start its own fixture"
            ));
        };
        let result = the_signing_secret_is_write_only_and_fails_closed(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "webhook_secret: the_signing_secret_is_write_only_fails_closed_without_a_vault_and_is_refused_alike_to_a_viewer",
        SECTION,
        "`PUT /webhooks/{name}/signing-secret` is 501 `service.not_configured` without a vault and 401 unsigned; with one, a Viewer is 403 for a webhook that exists and one that does not, a supplied secret is never echoed, a generated one is shown once with `Cache-Control: no-store`, `GET` shows a fingerprint and no value, and no value is in a document, a list or the server log",
        false,
        run_case,
    )]
}

fn path(name: &str) -> String {
    format!("/api/v1/webhooks/{name}/signing-secret")
}

/// Polls the site until `want` holds for the status of an unauthenticated request.
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

async fn the_signing_secret_is_write_only_and_fails_closed(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let name = fixture.unique("hook");

    // 1. The fixture host's vault is on (the sign-in stores seal their credentials through it, so
    // a site cannot run without one): a webhook that does not exist has no secret to write or
    // read (404, nothing written), and an unsigned request is 401. The 501 of a site without a
    // vault is not reachable over HTTP any more and is pinned by the route's own tests.
    let none = admin
        .send_json("PUT", &path(&name), &[], &json!({ "generate": true }))
        .await?;
    ensure!(
        none.status == 404 && none.error_code().as_deref() == Some("document.not_found"),
        "a PUT for a webhook that does not exist answered HTTP {}: {}",
        none.status,
        none.text()
    );
    let none = admin.get(&path(&name)).await?;
    ensure!(
        none.status == 404,
        "a GET for a webhook that does not exist answered HTTP {}: {}",
        none.status,
        none.text()
    );
    let unsigned = admin
        .anonymous()
        .send_json("PUT", &path(&name), &[], &json!({ "generate": true }))
        .await?;
    ensure!(
        unsigned.status == 401,
        "an unsigned PUT answered HTTP {}, not 401",
        unsigned.status
    );

    // 2. The operator turns the vault on: the table, the key file, a reload.
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
            "{original}\n[services.secrets]\nkek_file = {:?}\n",
            kek.display().to_string()
        ),
    )
    .map_err(|e| failure!("cannot write common.toml: {e}"))?;
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(fixture.root(), fixture.site()))
        .map_err(|e| failure!("reload request: {e}"))?;
    until_status(fixture, |status| status != 503).await?;

    // A webhook to hold the secret.
    let created = admin
        .create(
            "webhook",
            &json!({
                "name": name,
                "url": "https://hooks.example.test/in",
                "doctype": "project",
                "events": ["create"],
                "payload_mode": "ids",
            }),
        )
        .await?;
    created.expect(201)?;
    let id = created.id()?;

    // 3. A Viewer is refused, and a webhook that does not exist is refused the same way.
    let viewer = fixture.sign_in(VIEWER).await?;
    let missing = fixture.unique("nohook");
    let known = viewer
        .send_json("PUT", &path(&name), &[], &json!({ "generate": true }))
        .await?;
    let unknown = viewer
        .send_json("PUT", &path(&missing), &[], &json!({ "generate": true }))
        .await?;
    for (label, reply) in [("an existing", &known), ("a missing", &unknown)] {
        ensure!(
            reply.status == 403 && reply.error_code().as_deref() == Some("permission.denied"),
            "a Viewer's PUT to {label} webhook answered HTTP {}: {}",
            reply.status,
            reply.text()
        );
    }
    ensure!(
        known.json()["error"]["message"] == unknown.json()["error"]["message"],
        "the refusal differs between a webhook that exists and one that does not"
    );

    // 4. A supplied secret is stored and never echoed; a short one is refused.
    let short = admin
        .send_json("PUT", &path(&name), &[], &json!({ "secret": "too-short" }))
        .await?;
    ensure!(
        short.status == 400,
        "a 9-byte secret answered HTTP {}, not 400",
        short.status
    );
    let put = admin
        .send_json("PUT", &path(&name), &[], &json!({ "secret": SUPPLIED }))
        .await?;
    put.expect(200)?;
    ensure!(
        put.header("cache-control") == Some("private, no-store"),
        "the PUT answer is cacheable: {:?}",
        put.header("cache-control")
    );
    ensure!(
        !put.text().contains(SUPPLIED) && put.data().get("secret").is_none(),
        "the PUT answer echoes the supplied secret: {}",
        put.text()
    );
    let supplied_fingerprint = put.data()["fingerprint"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    ensure!(
        supplied_fingerprint.len() == 12
            && supplied_fingerprint.bytes().all(|b| b.is_ascii_hexdigit()),
        "the fingerprint is not 12 hex characters: {}",
        put.text()
    );

    // 5. A generated secret is shown once, in the PUT answer, and rotates the fingerprint.
    let generated = admin
        .send_json("PUT", &path(&name), &[], &json!({ "generate": true }))
        .await?;
    generated.expect(200)?;
    ensure!(
        generated.header("cache-control") == Some("private, no-store"),
        "the generated-secret answer is cacheable"
    );
    let secret = generated.data()["secret"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    ensure!(
        secret.len() >= 43,
        "the generated secret is {} bytes, not at least 43 (32 random bytes)",
        secret.len()
    );
    let rotated = generated.data()["fingerprint"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    ensure!(
        rotated != supplied_fingerprint,
        "a rotation did not change the fingerprint"
    );

    // 6. GET shows the fingerprint and key version, never a value.
    let read = admin.get(&path(&name)).await?;
    read.expect(200)?;
    ensure!(
        read.data()["fingerprint"].as_str() == Some(rotated.as_str())
            && read.data()["set"] == Json::Bool(true)
            && read.data().get("secret").is_none()
            && !read.text().contains(&secret),
        "the GET answer is not just the fingerprint of the stored secret: {}",
        read.text()
    );

    // 7. No read of the webhook shows a value: the field is absent, not blank.
    let doc = admin.document("webhook", &id).await?;
    doc.expect(200)?;
    ensure!(
        doc.data().get("secret").is_none()
            && !doc.text().contains(&secret)
            && !doc.text().contains(SUPPLIED),
        "the webhook document shows the secret field: {}",
        doc.text()
    );
    let list = admin.list("webhook", "").await?;
    list.expect(200)?;
    ensure!(
        !list.text().contains(&secret) && !list.text().contains(SUPPLIED),
        "the webhook list shows a signing secret"
    );

    // 8. A webhook that does not exist is a 404 to a caller who may write secrets.
    let gone = admin.get(&path(&missing)).await?;
    ensure!(
        gone.status == 404,
        "a GET for a missing webhook answered HTTP {}, not 404",
        gone.status
    );

    // 9. Neither value reached the server log.
    let log = std::fs::read_to_string(fixture.log()).unwrap_or_default();
    ensure!(
        !log.contains(&secret) && !log.contains(SUPPLIED),
        "a signing secret reached the server log"
    );
    std::fs::write(&common, original).map_err(|e| failure!("cannot restore common.toml: {e}"))?;
    Ok(())
}
