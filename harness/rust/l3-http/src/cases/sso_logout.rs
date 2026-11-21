//! The unauthenticated single-logout receivers over the real stack (WP-IAM-05.T13).
//!
//! The shared fixture has no single sign-on, so this case starts a host of its own with the
//! secrets service, the sign-in throttle and `[services.sso]` (in-memory stores: the fixture is a
//! single-node site) turned on, and proves what needs no identity provider: the receivers are
//! reachable without a session, refuse a garbage logout token, an empty one, an oversized one and a
//! malformed SAML request with exactly the contract's `sso.logout_rejected` and no detail, and
//! answer an unknown provider **identically** to a known-but-invalid message (no oracle).
//! The accepted paths (a signed token ends the right sessions, replay refusal) need a stub identity
//! provider and are the end-to-end gate of WP-IAM-05.T14 (`sso_attacks.rs`).

use std::time::{Duration, Instant};

use super::{CaseDef, CaseFuture};
use crate::fixture::Fixture;
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After the sign-in throttle case; the case owns its fixture.
pub const ORDER: u32 = 34;

const SECTION: &str = "Single logout";

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!(
                "the single logout case could not start its own fixture"
            ));
        };
        let result = garbage_is_refused_alike(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "sso_logout: garbage_logout_messages_are_refused_alike_and_without_detail",
        SECTION,
        "the logout receivers need no session, refuse a garbage, empty or oversized token and a malformed SAML request with `sso.logout_rejected`, and answer an unknown provider exactly like a bad token",
        false,
        run_case,
    )]
}

async fn until_ready(fixture: &Fixture) -> Result<(), Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = fixture
            .client()
            .anonymous()
            .get("/api/v1/auth/session")
            .await?;
        if reply.status != 503 {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(failure!(
                "the site is still unavailable 30s after the reload"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn garbage_is_refused_alike(fixture: &Fixture) -> CaseResult {
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
            "{original}\n[services.secrets]\nkek_file = {:?}\n\n[services.signin_throttle]\n\n[services.sso]\nenabled = true\nstore = \"memory\"\n",
            kek.display().to_string()
        ),
    )
    .map_err(|e| failure!("cannot write common.toml: {e}"))?;
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(fixture.root(), fixture.site()))
        .map_err(|e| failure!("reload request: {e}"))?;
    until_ready(fixture).await?;

    let anonymous = fixture.client().anonymous();
    let form = "application/x-www-form-urlencoded";
    let post = |path: &'static str, body: String| {
        let anonymous = anonymous.clone();
        async move {
            anonymous
                .send("POST", path, &[], Some((form, body.into_bytes())))
                .await
        }
    };

    let mut bodies = Vec::new();
    for (label, path, body) in [
        (
            "a garbage token, unknown provider",
            "/api/v1/auth/sso/no-such/backchannel-logout",
            "logout_token=not-a-jwt".to_owned(),
        ),
        (
            "a garbage token, any provider name",
            "/api/v1/auth/sso/other/backchannel-logout",
            "logout_token=not-a-jwt".to_owned(),
        ),
        (
            "an empty token",
            "/api/v1/auth/sso/no-such/backchannel-logout",
            "logout_token=".to_owned(),
        ),
        (
            "an oversized token",
            "/api/v1/auth/sso/no-such/backchannel-logout",
            format!("logout_token={}", "a".repeat(200_000)),
        ),
    ] {
        let reply = post(path, body).await?;
        ensure!(
            reply.status == 400 || reply.status == 413,
            "{label}: HTTP {} instead of a refusal: {}",
            reply.status,
            reply.text()
        );
        if reply.status == 400 {
            ensure!(
                reply.error_code().as_deref() == Some("sso.logout_rejected"),
                "{label}: the code is {:?}, not sso.logout_rejected",
                reply.error_code()
            );
            bodies.push((reply.status, reply.error_code()));
        }
    }
    ensure!(
        bodies.windows(2).all(|w| w[0] == w[1]),
        "the refusals differ between an unknown provider and another name: {bodies:?}"
    );

    // A malformed SAML single-logout request, both bindings.
    let slo_post = post(
        "/api/v1/auth/sso/no-such/slo",
        "SAMLRequest=AAAA".to_owned(),
    )
    .await?;
    ensure!(
        slo_post.status == 400 && slo_post.error_code().as_deref() == Some("sso.logout_rejected"),
        "a malformed SAML POST logout answered HTTP {}: {}",
        slo_post.status,
        slo_post.text()
    );
    let slo_get = anonymous
        .get("/api/v1/auth/sso/no-such/slo?SAMLRequest=AAAA")
        .await?;
    ensure!(
        slo_get.status == 400 && slo_get.error_code().as_deref() == Some("sso.logout_rejected"),
        "a malformed SAML redirect logout answered HTTP {}: {}",
        slo_get.status,
        slo_get.text()
    );
    Ok(())
}
