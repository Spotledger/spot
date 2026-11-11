//! The secrets service (WP-SEC-03.T9) over the real stack. The shared fixture host configures no
//! `[services.secrets]` table and the registered cases run after the shutdown case, so this case
//! starts a host of its own, writes the table into its `common.toml` and reloads it: the way an
//! operator turns the service on, and the way a damaged key file reaches a running site.

use std::time::{Duration, Instant};

use super::CaseDef;
use crate::failure;
use crate::fixture::Fixture;
use crate::report::{CaseResult, Failure};

/// After `events`; independent of the other groups' data (it owns its fixture).
pub const ORDER: u32 = 30;

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "secrets: an_unusable_key_file_makes_the_site_unavailable_and_a_good_one_serves_it_again",
        "Secrets",
        "a reload onto a missing key file answers 503 for the whole site (no plaintext fallback), a reload onto a good key file serves again, and no key reaches the server log",
        false,
        |_| Box::pin(an_unusable_key_file_makes_the_site_unavailable()),
    )]
}

/// Polls the site until `want` holds for the status of an unauthenticated request.
async fn until_status(fixture: &Fixture, want: impl Fn(u16) -> bool) -> Result<u16, Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = fixture.client().anonymous().get("/api/v1/auth/session").await?;
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

async fn an_unusable_key_file_makes_the_site_unavailable() -> CaseResult {
    let Some(mut fixture) = Fixture::start().await else {
        return Err(failure!("no PostgreSQL for the case's own fixture"));
    };
    let result = run(&fixture).await;
    fixture.finish().await;
    result
}

async fn run(fixture: &Fixture) -> CaseResult {
    let common = fixture.root().join("common.toml");
    let original = std::fs::read_to_string(&common)
        .map_err(|e| failure!("cannot read the fixture's common.toml: {e}"))?;
    let keys = fixture.root().join("keys");
    let kek = keys.join(format!("{}.json", fixture.site()));
    let with_table = |kek_file: &std::path::Path| {
        format!(
            "{original}\n[services.secrets]\nkek_file = {:?}\n",
            kek_file.display().to_string()
        )
    };

    // 1. The table names a key file that is not there: the site answers 503 and nothing else.
    std::fs::write(&common, with_table(&kek))
        .map_err(|e| failure!("cannot write common.toml: {e}"))?;
    spot_cli::request_reload(fixture.root(), fixture.site())
        .map_err(|e| failure!("reload request: {e}"))?;
    until_status(fixture, |status| status == 503).await?;
    let admin = fixture.admin();
    let refused = admin.get("/api/v1/resource/user").await?;
    if refused.status != 503 {
        return Err(failure!(
            "an authenticated read answered {} instead of 503 while the key file is missing: {}",
            refused.status,
            refused.text()
        ));
    }
    if refused.error_code().as_deref() != Some("server.unavailable") {
        return Err(failure!(
            "the 503 does not carry server.unavailable: {}",
            refused.text()
        ));
    }
    if refused.text().contains(&kek.display().to_string()) {
        return Err(failure!(
            "the client-visible 503 names the key file path: {}",
            refused.text()
        ));
    }

    // 2. The operator creates the key file (as `spot secrets init` does) and reloads: it serves.
    spot_cli::services::secrets::init_key_file(&kek, fixture.site())
        .await
        .map_err(|e| failure!("cannot create the key file: {e}"))?;
    spot_cli::request_reload(fixture.root(), fixture.site())
        .map_err(|e| failure!("reload request: {e}"))?;
    until_status(fixture, |status| status != 503).await?;
    let me = fixture.admin().session().await?;
    me.expect(200)?;

    // 3. No key text is anywhere in the server log.
    let text = std::fs::read_to_string(&kek).map_err(|e| failure!("cannot read the key file: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| failure!("the key file is not JSON: {e}"))?;
    let log = std::fs::read_to_string(fixture.log()).unwrap_or_default();
    for key in json["keys"].as_array().into_iter().flatten() {
        if let Some(key) = key["key"].as_str()
            && log.contains(key)
        {
            return Err(failure!("a key reached the server log"));
        }
    }
    std::fs::write(&common, original).map_err(|e| failure!("cannot restore common.toml: {e}"))?;
    Ok(())
}
