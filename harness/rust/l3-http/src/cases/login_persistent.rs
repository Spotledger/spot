//! Sign-in over the storage-backed stores (WP-PLT-02.T8): a session and the credential file
//! behave as the database says, across a reload.
//!
//! The fixture host runs one process, so "a restart" is the operator's reload
//! (`spot_cli::request_reload`), which rebuilds the site's services and its sign-in assembly over
//! the same database. The case proves what a reload must not change: a session started before it
//! still answers after it with the same token, and a signed-out token stays dead. It then reads the
//! site file and proves no chosen-password verifier and no API key digest is left in it (only
//! one-time first sign-in secrets may remain).
//!
//! Cases that need a second node, a killed process or a seeded `site.toml` with interrupted
//! migration are covered by the `spot-cli` tests of the `login_stores` service.

use std::time::{Duration, Instant};

use super::{CaseDef, CaseFuture};
use crate::fixture::{Fixture, VIEWER};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After the webhook secret case; the case owns no data of its own.
pub const ORDER: u32 = 32;

const SECTION: &str = "Persistent sign-in";

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    // The shared host has ended with the shutdown case: the case reloads a host of its own.
    Box::pin(async move {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!("no PostgreSQL for the case's own fixture"));
        };
        let result = a_session_survives_a_reload_and_the_site_file_holds_no_verifier(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "login_persistent: a_session_survives_a_reload_and_the_site_file_holds_no_chosen_verifier",
        SECTION,
        "a session started before a reload answers after it with the same token, a signed-out token stays dead after it, and `site.toml` holds no `[[api_keys]]` and no credential that is not a one-time first sign-in secret",
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

async fn a_session_survives_a_reload_and_the_site_file_holds_no_verifier(
    fixture: &Fixture,
) -> CaseResult {
    let live = fixture.sign_in(VIEWER).await?;
    let gone = fixture.sign_in(VIEWER).await?;
    let ended = gone.logout().await?;
    ensure!(
        ended.status == 204 || ended.status == 200,
        "sign-out answered HTTP {}",
        ended.status
    );

    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(fixture.root(), fixture.site()))
        .map_err(|e| failure!("reload request: {e}"))?;
    until_ready(fixture).await?;

    let after = live.session().await?;
    ensure!(
        after.status == 200,
        "a session started before the reload answered HTTP {} after it: {}",
        after.status,
        after.text()
    );
    let dead = gone.session().await?;
    ensure!(
        dead.status == 401,
        "a signed-out token answered HTTP {} after the reload, not 401",
        dead.status
    );

    let file = fixture
        .root()
        .join("sites")
        .join(fixture.site())
        .join("site.toml");
    let text =
        std::fs::read_to_string(&file).map_err(|e| failure!("cannot read the site file: {e}"))?;
    ensure!(
        !text.contains("[[api_keys]]"),
        "an API key verifier is still in site.toml"
    );
    for block in text.split("[[credentials]]").skip(1) {
        let block = block.split("\n[[").next().unwrap_or("");
        ensure!(
            block.contains("first_sign_in = true"),
            "a chosen-password verifier is still in site.toml"
        );
    }
    Ok(())
}
