//! Module-level whitelisted functions (`/api/v1/method/{app}/{module}/{function}`,
//! WP-PLT-17.T2): an app declares `[[functions]]`, the binary registers native handlers, and the
//! host serves them with the verbs, argument schema, rate limit and sign-in rules of the registry.
//!
//! A registered case runs after the hand-written block, whose last case stops the shared host, so
//! this case starts its own (one more database and host, a few seconds). The fixture's host is
//! already running, so the case adds the declarations to its private copy of the sample app and
//! reloads the site: the registry is rebuilt from the manifests on every reload.

use std::time::{Duration, Instant};

use serde_json::json;
use spot_cli::natives::{FunctionFailure, FunctionSpec, register_function_fn};

use super::CaseDef;
use crate::fixture::{Fixture, VIEWER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After `secrets`; independent of the other groups' data (it owns its fixture).
pub const ORDER: u32 = 40;

const BASE: &str = "/api/v1/method/sample/profile";
const PUBLIC: &str = "/api/v1/method/sample/public";
const REPORTS: &str = "/api/v1/method/sample/reports";

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "function: a_declared_function_is_served_with_its_verbs_schema_limit_and_sign_in_rules",
        "Functions",
        "the functions the sample app ships (`profile/whoami` GET, `profile/greet` POST with an argument schema and a limit, `public/ping` guest GET) are called as declared (`405` with the `Allow` set otherwise), a bad argument is `400` naming the problem and never the value, an unknown function and a non-guest one asked by nobody are the same `404`, a guest function answers a caller with no credential, a handler that refuses is `403 permission.denied` (not an empty success), and a spent rate limit is `429` with `Retry-After`",
        false,
        |_| Box::pin(a_declared_function_is_served()),
    )]
}

/// A function of this case's own: the shipped ones cannot show a handler that refuses a user.
fn register(admin_id: String) {
    register_function_fn(
        "sample",
        "reports",
        "admin_only",
        FunctionSpec::default(),
        move |call| {
            let admin_id = admin_id.clone();
            Box::pin(async move {
                if call.user.as_deref() == Some(admin_id.as_str()) {
                    Ok(json!({"ok": true}))
                } else {
                    Err(FunctionFailure {
                        code: "permission.denied".to_owned(),
                        message: "only the Administrator may call this function".to_owned(),
                        denied: true,
                    })
                }
            })
        },
    );
}

async fn a_declared_function_is_served() -> CaseResult {
    let Some(mut fixture) = Fixture::start_secondary().await else {
        return Err(failure!(
            "the function case could not start its own fixture"
        ));
    };
    let result = run(&fixture).await;
    fixture.finish().await;
    result
}

async fn run(fixture: &Fixture) -> CaseResult {
    // The shipped functions are declared by the sample app already; only the refusing one of this
    // case is added, with a reload (the registry is rebuilt from the manifests on every reload).
    register(fixture.admin_id().to_owned());
    let manifest = fixture.root().join("sample/app.toml");
    let mut declared = std::fs::read_to_string(&manifest)
        .map_err(|e| failure!("cannot read {}: {e}", manifest.display()))?;
    declared.push_str(
        "\n[[functions]]\nmodule = \"reports\"\nname = \"admin_only\"\nhttp = [\"GET\"]\n",
    );
    std::fs::write(&manifest, declared)
        .map_err(|e| failure!("cannot write {}: {e}", manifest.display()))?;
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(fixture.root(), fixture.site()))
        .map_err(|e| failure!("cannot request a reload: {e}"))?;

    let admin = fixture.admin();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = admin.get(&format!("{REPORTS}/admin_only")).await?;
        if reply.status == 200 {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "the reloaded site never served the declared function: {}",
            reply.text()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // The read function answers with the caller's own user id.
    let whoami = admin.get(&format!("{BASE}/whoami")).await?;
    whoami.expect(200)?;
    ensure!(
        whoami.data()["user"] == json!(fixture.admin_id()),
        "whoami did not return the caller's user id: {}",
        whoami.text()
    );
    // The write function reads its arguments from the body, by its schema.
    let greeted = admin
        .send_json(
            "POST",
            &format!("{BASE}/greet"),
            &[],
            &json!({"args": {"name": "Ama", "formal": true}}),
        )
        .await?;
    greeted.expect(200)?;
    ensure!(
        greeted.data()["greeting"] == json!("Good day, Ama."),
        "unexpected greeting: {}",
        greeted.text()
    );

    // A verb the function does not answer: 405 with the `Allow` set.
    let wrong = admin.get(&format!("{BASE}/greet")).await?;
    wrong.expect(405)?.expect_error("api.method_not_allowed")?;
    ensure!(
        wrong.error_details()["allow"] == json!("POST"),
        "the Allow set is missing: {}",
        wrong.text()
    );

    // A bad argument names the problem and never the value.
    let bad = admin
        .send_json(
            "POST",
            &format!("{BASE}/greet"),
            &[],
            &json!({"args": {"name": 12345678}}),
        )
        .await?;
    bad.expect(400)?.expect_error("api.invalid_request")?;
    ensure!(
        !bad.text().contains("12345678"),
        "the argument value was echoed: {}",
        bad.text()
    );
    let missing = admin
        .send_json("POST", &format!("{BASE}/greet"), &[], &json!({"args": {}}))
        .await?;
    missing.expect(400)?;

    // Unknown is `404`; a non-guest function asked by nobody is the very same answer; a guest
    // function answers a caller with no credential.
    admin
        .get(&format!("{BASE}/nothing"))
        .await?
        .expect(404)?
        .expect_error("method.not_found")?;
    let anonymous = fixture.client().anonymous();
    anonymous
        .get(&format!("{BASE}/whoami"))
        .await?
        .expect(404)?
        .expect_error("method.not_found")?;
    let ping = anonymous.get(&format!("{PUBLIC}/ping")).await?;
    ping.expect(200)?;
    ensure!(
        ping.data()["ok"] == json!(true),
        "the guest function did not answer: {}",
        ping.text()
    );

    // A handler that refuses a user is denied, not served an empty success.
    admin
        .get(&format!("{REPORTS}/admin_only"))
        .await?
        .expect(200)?;
    let viewer = fixture.sign_in(VIEWER).await?;
    let denied = viewer.get(&format!("{REPORTS}/admin_only")).await?;
    denied.expect(403)?.expect_error("permission.denied")?;

    // The rate limit (20 calls per ten minutes on `greet`): the call after the last permit is
    // `429` with `Retry-After`.
    let mut limited = None;
    for _ in 0..25 {
        let reply = admin
            .send_json(
                "POST",
                &format!("{BASE}/greet"),
                &[],
                &json!({"args": {"name": "Ama"}}),
            )
            .await?;
        if reply.status == 429 {
            limited = Some(reply);
            break;
        }
        reply.expect(200)?;
    }
    let Some(limited) = limited else {
        return Err(failure!("the rate limit of `greet` never answered 429"));
    };
    limited.expect_error("api.rate_limited")?;
    ensure!(
        limited.header("retry-after").is_some(),
        "a spent rate limit carries no Retry-After: {}",
        limited.text()
    );
    Ok(())
}
