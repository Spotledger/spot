//! The coordinator sign-in throttle over the real stack (WP-IAM-11.T23).
//!
//! The shared fixture host configures no `[services.secrets]` table, so this case starts a host
//! of its own, turns the secrets service and `[services.signin_throttle]` on with a small
//! per-account limit (the technique of `webhook_secret.rs`), and proves: attempts on one account
//! beyond the limit are refused with `api.rate_limited` and a `Retry-After`, **even with the right
//! password**; the refusal survives a reload (the counters are in the database, not in the
//! process); another account is untouched; and a name that belongs to nobody is refused after
//! exactly the same number of attempts as a real one (no account-existence oracle).
//!
//! Not here: a second server node and a stopped coordinator. Those need two processes and a way
//! to cut the database tables, and are covered by the PostgreSQL tests of
//! `spot-throttle-coordinator` and the unit tests of `services/signin_throttle.rs`.

use std::time::{Duration, Instant};

use super::{CaseDef, CaseFuture};
use crate::fixture::{Fixture, VIEWER};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After the persistent sign-in case; the case owns its fixture.
pub const ORDER: u32 = 33;

const SECTION: &str = "Sign-in throttle";

/// Attempts one account may make per window in this case.
const LIMIT: usize = 3;

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!(
                "the sign-in throttle case could not start its own fixture"
            ));
        };
        let result = the_account_limit_is_shared_survives_a_reload_and_is_no_oracle(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "signin_throttle: the_account_limit_survives_a_reload_refuses_the_right_password_and_treats_unknown_names_alike",
        SECTION,
        "with `[services.signin_throttle]` on, attempts on one account beyond the limit are `api.rate_limited` with Retry-After even with the right password, the refusal survives a reload, another account still signs in, and a name that belongs to nobody is refused after the same number of attempts",
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

async fn the_account_limit_is_shared_survives_a_reload_and_is_no_oracle(
    fixture: &Fixture,
) -> CaseResult {
    // The account under test must be one nobody has tried yet: the throttle counts every attempt
    // on an account, the fixture's own sign-in of the Administrator included, and the counters
    // are kept across the reload that changes the limit. A fresh account has a full bucket.
    const PROBE: &str = "throttle-probe@l3.test";
    const PROBE_PASSWORD: &str = "river-copper-lantern-42-mosaic";
    spot_cli::users::add_user(fixture.root(), fixture.site(), PROBE, None, &[])
        .await
        .map_err(|e| failure!("add-user {PROBE}: {e}"))?;
    spot_cli::users::set_password(
        fixture.root(),
        fixture.site(),
        PROBE,
        &spot_auth::Password::new(PROBE_PASSWORD),
    )
    .await
    .map_err(|e| failure!("set-password {PROBE}: {e}"))?;

    // The operator turns the vault and the throttle on, with a small limit, and reloads.
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
            "{original}\n[services.secrets]\nkek_file = {:?}\n\n[services.signin_throttle]\nper_account = {{ capacity = {LIMIT}, per_seconds = 3600 }}\nper_source = {{ capacity = 1000, per_seconds = 3600 }}\nper_tenant = {{ capacity = 100000, per_seconds = 3600 }}\n",
            kek.display().to_string()
        ),
    )
    .map_err(|e| failure!("cannot write common.toml: {e}"))?;
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(fixture.root(), fixture.site()))
        .map_err(|e| failure!("reload request: {e}"))?;
    until_ready(fixture).await?;

    let anonymous = fixture.client().anonymous();
    // Wrong passwords on a real account and on a name that belongs to nobody, in step. The
    // fixture's sign-in throttle backs an account off after two failures, so the first two are
    // 401 and every later one is the same 429 for both: no difference may show which name exists.
    for attempt in 0..LIMIT {
        let real = anonymous.login(PROBE, "wrong-password-1").await?;
        let unknown = anonymous.login("nobody-at-all", "wrong-password-1").await?;
        let want = if attempt < 2 { 401 } else { 429 };
        ensure!(
            real.status == want && unknown.status == want,
            "attempt {attempt}: a wrong password answered HTTP {} (real) and {} (unknown), not {want}",
            real.status,
            unknown.status
        );
        ensure!(
            real.error_code() == unknown.error_code()
                && real.header("retry-after").is_some() == unknown.header("retry-after").is_some(),
            "attempt {attempt}: the real account answered {:?} and the unknown name {:?}",
            real.error_code(),
            unknown.error_code()
        );
    }
    // Over the limit: refused even with the right password, with a retry hint; the unknown name
    // is refused at exactly the same attempt.
    let real = anonymous.login(PROBE, PROBE_PASSWORD).await?;
    let unknown = anonymous.login("nobody-at-all", PROBE_PASSWORD).await?;
    for (label, reply) in [("the real account", &real), ("the unknown name", &unknown)] {
        ensure!(
            reply.status == 429 && reply.error_code().as_deref() == Some("api.rate_limited"),
            "{label} answered HTTP {} after {LIMIT} attempts: {}",
            reply.status,
            reply.text()
        );
        ensure!(
            reply.header("retry-after").is_some(),
            "{label}: the refusal carries no Retry-After"
        );
    }

    // A reload does not reset it: the counters are in the database.
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(fixture.root(), fixture.site()))
        .map_err(|e| failure!("reload request: {e}"))?;
    until_ready(fixture).await?;
    let after = anonymous.login(PROBE, PROBE_PASSWORD).await?;
    ensure!(
        after.status == 429,
        "the account limit was forgotten by a reload: HTTP {}",
        after.status
    );

    // Another account is untouched.
    let viewer = fixture.sign_in(VIEWER).await?;
    ensure!(
        viewer.session().await?.status == 200,
        "another account could not sign in while one was throttled"
    );
    Ok(())
}
