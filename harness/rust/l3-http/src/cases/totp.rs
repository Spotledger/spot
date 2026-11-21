//! Second-factor self-service over the real stack (WP-PLT-02.T9): enrol, confirm, sign in with the
//! code, remove, and the refusals around them.
//!
//! The shared fixture host configures no `[services.secrets]` or `[services.signin_throttle]`, so
//! the case starts a host of its own, turns both on (the technique of `webhook_secret.rs`) and
//! reloads. It proves: without enrolment a confirm is `409 identity.totp_state`; enrol shows the
//! secret once with `Cache-Control: no-store`; a wrong confirm code is `401 auth.totp_invalid`;
//! confirm answers ten recovery codes; sign-in then asks for a code (`401 auth.totp_required`) and
//! accepts a valid one; the same code a second time is refused (step replay); a recovery code
//! signs in once; enrolling again is `409`; disable with a wrong code is refused and the factor
//! stays; disable with the password and a code succeeds.
//!
//! `spot-auth` has no code generator, so the case finds the current code by trying every six-digit
//! value against `TotpConfig::verify`.

use std::time::{Duration, Instant};

use serde_json::json;
use spot_auth::{TotpConfig, TotpSecret};

use super::{CaseDef, CaseFuture};
use crate::fixture::{ADMIN_PASSWORD, Fixture};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After the sign-in throttle case; the case owns its fixture.
pub const ORDER: u32 = 34;

const SECTION: &str = "Second factor";

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!(
                "the second-factor case could not start its own fixture"
            ));
        };
        let result = a_user_enrols_signs_in_with_the_code_and_removes_the_factor(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "totp: a_user_enrols_signs_in_with_the_code_and_removes_the_factor",
        SECTION,
        "enrol shows the secret once and uncached, a wrong confirm code is `auth.totp_invalid`, confirm answers recovery codes, sign-in then needs a code, a code works once per step, a recovery code works once, enrolling again and a disable with a wrong code are refused, and a disable with the password and a code removes the factor",
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

/// A valid code for `secret` now. `spot-auth` exposes verification and no generator, so the case
/// finds the code by trying all 10^6 six-digit values against `verify` (under a second).
fn code_now(secret: &str) -> Result<String, Failure> {
    let secret =
        TotpSecret::from_base32(secret).map_err(|_| failure!("the secret is not base32"))?;
    let config = TotpConfig {
        algorithm: spot_auth::TotpAlgorithm::Sha1,
        digits: spot_auth::TotpDigits::Six,
        step_seconds: spot_auth::totp::DEFAULT_STEP_SECONDS,
        skew_steps: 0,
    };
    let clock = spot_core::SystemClock::new();
    (0..1_000_000u32)
        .map(|n| format!("{n:06}"))
        .find(|code| config.verify(&secret, code, &clock, None).is_ok())
        .ok_or_else(|| failure!("no code verifies for the secret"))
}

async fn a_user_enrols_signs_in_with_the_code_and_removes_the_factor(
    fixture: &Fixture,
) -> CaseResult {
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
            "{original}\n[services.secrets]\nkek_file = {:?}\n\n[services.signin_throttle]\nper_account = {{ capacity = 100, per_seconds = 3600 }}\nper_source = {{ capacity = 1000, per_seconds = 3600 }}\nper_tenant = {{ capacity = 100000, per_seconds = 3600 }}\n",
            kek.display().to_string()
        ),
    )
    .map_err(|e| failure!("cannot write common.toml: {e}"))?;
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(fixture.root(), fixture.site()))
        .map_err(|e| failure!("reload request: {e}"))?;
    until_ready(fixture).await?;

    let admin = fixture
        .client()
        .sign_in("administrator", ADMIN_PASSWORD)
        .await?;
    let confirm_path = "/api/v1/auth/totp/confirm";

    // 1. No enrolment in progress: a confirm is a state error.
    let early = admin
        .send_json("POST", confirm_path, &[], &json!({ "code": "000000" }))
        .await?;
    ensure!(
        early.status == 409 && early.error_code().as_deref() == Some("identity.totp_state"),
        "a confirm with no enrolment answered HTTP {}: {}",
        early.status,
        early.text()
    );

    // 2. Enrol: the secret is shown once, uncached.
    let enrol = admin
        .send_json("POST", "/api/v1/auth/totp/enrol", &[], &json!({}))
        .await?;
    enrol.expect(200)?;
    ensure!(
        enrol
            .header("cache-control")
            .is_some_and(|c| c.contains("no-store")),
        "the enrol answer is cacheable: {:?}",
        enrol.header("cache-control")
    );
    let secret = enrol.data()["secret"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    ensure!(!secret.is_empty(), "the enrol answer holds no secret");

    // 3. A wrong code does not confirm; the right one does and answers the recovery codes.
    let wrong = admin
        .send_json("POST", confirm_path, &[], &json!({ "code": "000000" }))
        .await?;
    ensure!(
        wrong.status == 401 && wrong.error_code().as_deref() == Some("auth.totp_invalid"),
        "a wrong confirm code answered HTTP {}: {}",
        wrong.status,
        wrong.text()
    );
    let confirmed = admin
        .send_json(
            "POST",
            confirm_path,
            &[],
            &json!({ "code": code_now(&secret)? }),
        )
        .await?;
    confirmed.expect(200)?;
    let codes: Vec<String> = confirmed.data()["recovery_codes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    ensure!(
        codes.len() == 10,
        "confirm answered {} recovery codes, not 10",
        codes.len()
    );

    // 4. Enrolling again is refused, not replaced.
    let again = admin
        .send_json("POST", "/api/v1/auth/totp/enrol", &[], &json!({}))
        .await?;
    ensure!(
        again.status == 409,
        "a second enrol answered HTTP {}",
        again.status
    );

    // 5. Sign-in now asks for a code; a recovery code works once.
    let anonymous = fixture.client().anonymous();
    let needs = anonymous.login("administrator", ADMIN_PASSWORD).await?;
    ensure!(
        needs.status == 401 && needs.error_code().as_deref() == Some("auth.totp_required"),
        "sign-in without a code answered HTTP {}: {}",
        needs.status,
        needs.text()
    );
    let body = |recovery: &str| json!({ "user": "administrator", "password": ADMIN_PASSWORD, "recovery_code": recovery });
    let first = anonymous
        .send_json("POST", "/api/v1/auth/login", &[], &body(&codes[0]))
        .await?;
    ensure!(
        first.status == 200,
        "a recovery code did not sign in: HTTP {}",
        first.status
    );
    let reuse = anonymous
        .send_json("POST", "/api/v1/auth/login", &[], &body(&codes[0]))
        .await?;
    ensure!(
        reuse.status == 401 && reuse.error_code().as_deref() == Some("auth.totp_invalid"),
        "a spent recovery code answered HTTP {}",
        reuse.status
    );

    // 6. Disable with a wrong code is refused and the factor stays; with the proof it goes.
    let disable_path = "/api/v1/auth/totp/disable";
    let refused = admin
        .send_json(
            "POST",
            disable_path,
            &[],
            &json!({ "password": ADMIN_PASSWORD, "code": "000000" }),
        )
        .await?;
    ensure!(
        refused.status == 401,
        "a disable with a wrong code answered HTTP {}",
        refused.status
    );
    let still = anonymous.login("administrator", ADMIN_PASSWORD).await?;
    ensure!(
        still.status == 401,
        "the factor was removed by a refused disable"
    );
    let removed = admin
        .send_json(
            "POST",
            disable_path,
            &[],
            &json!({ "password": ADMIN_PASSWORD, "code": codes[1] }),
        )
        .await?;
    ensure!(
        removed.status == 204,
        "disable with the proof answered HTTP {}: {}",
        removed.status,
        removed.text()
    );
    let signed = anonymous.login("administrator", ADMIN_PASSWORD).await?;
    ensure!(
        signed.status == 200,
        "sign-in after removal answered HTTP {}",
        signed.status
    );
    Ok(())
}
