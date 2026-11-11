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

const BASE: &str = "/api/v1/method/sample/reports";

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "function: a_declared_function_is_served_with_its_verbs_schema_limit_and_sign_in_rules",
        "Functions",
        "a function an app declares is called with `GET` and `POST` as declared (`405` with the `Allow` set otherwise), a bad argument is `400` naming the problem and never the value, an unknown function is `404`, nobody signed in is `401`, a user the function refuses is `403 permission.denied` (not an empty success), and a spent rate limit is `429` with `Retry-After`",
        false,
        |_| Box::pin(a_declared_function_is_served()),
    )]
}

fn register(admin_id: String) {
    register_function_fn(
        "sample",
        "reports",
        "total",
        FunctionSpec {
            limit: None,
            arguments: Some(json!({
                "type": "object",
                "required": ["n"],
                "properties": {"n": {"type": "integer"}}
            })),
        },
        |call| Box::pin(async move { Ok(json!({"total": call.args["n"]})) }),
    );
    register_function_fn("sample", "reports", "stamp", FunctionSpec::default(), |_| {
        Box::pin(async { Ok(json!({"stamped": true})) })
    });
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
    register_function_fn(
        "sample",
        "reports",
        "limited",
        FunctionSpec {
            limit: Some((2, Duration::from_secs(600))),
            arguments: None,
        },
        |_| Box::pin(async { Ok(json!({"ok": true})) }),
    );
}

async fn a_declared_function_is_served() -> CaseResult {
    let Some(mut fixture) = Fixture::start().await else {
        return Err(failure!("the function case could not start its own fixture"));
    };
    let result = run(&fixture).await;
    fixture.finish().await;
    result
}

async fn run(fixture: &Fixture) -> CaseResult {
    register(fixture.admin_id().to_owned());
    let manifest = fixture.root().join("sample/app.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| failure!("cannot read {}: {e}", manifest.display()))?;
    let mut declared = text;
    for (name, verb) in [
        ("total", "GET"),
        ("stamp", "POST"),
        ("admin_only", "GET"),
        ("limited", "GET"),
    ] {
        declared.push_str(&format!(
            "\n[[functions]]\nmodule = \"reports\"\nname = \"{name}\"\nhttp = [\"{verb}\"]\n"
        ));
    }
    std::fs::write(&manifest, declared)
        .map_err(|e| failure!("cannot write {}: {e}", manifest.display()))?;
    spot_cli::request_reload(fixture.root(), fixture.site())
        .map_err(|e| failure!("cannot request a reload: {e}"))?;

    let admin = fixture.admin();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = admin.get(&format!("{BASE}/total?n=1")).await?;
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

    // GET arguments are read by the schema; a POST body carries `args`.
    let total = admin.get(&format!("{BASE}/total?n=41")).await?;
    total.expect(200)?;
    ensure!(
        total.data()["total"] == json!(41),
        "the integer argument was not read as an integer: {}",
        total.text()
    );
    let stamped = admin
        .send_json("POST", &format!("{BASE}/stamp"), &[], &json!({"args": {}}))
        .await?;
    stamped.expect(200)?;

    // A verb the function does not answer: 405 with the `Allow` set.
    let wrong = admin.get(&format!("{BASE}/stamp")).await?;
    wrong.expect(405)?.expect_error("api.method_not_allowed")?;
    ensure!(
        wrong.error_details()["allow"] == json!("POST"),
        "the Allow set is missing: {}",
        wrong.text()
    );

    // A bad argument names the problem and never the value.
    let bad = admin.get(&format!("{BASE}/total?n=SECRET-VALUE-77")).await?;
    bad.expect(400)?.expect_error("api.invalid_request")?;
    ensure!(
        !bad.text().contains("SECRET-VALUE-77"),
        "the argument value was echoed: {}",
        bad.text()
    );
    let missing = admin.get(&format!("{BASE}/total")).await?;
    missing.expect(400)?;

    // Unknown, and nobody signed in.
    admin
        .get(&format!("{BASE}/nothing"))
        .await?
        .expect(404)?
        .expect_error("method.not_found")?;
    fixture
        .client()
        .anonymous()
        .get(&format!("{BASE}/total?n=1"))
        .await?
        .expect(401)?;

    // A user the function refuses is denied, not served an empty success.
    admin.get(&format!("{BASE}/admin_only")).await?.expect(200)?;
    let viewer = fixture.sign_in(VIEWER).await?;
    let denied = viewer.get(&format!("{BASE}/admin_only")).await?;
    denied.expect(403)?.expect_error("permission.denied")?;

    // The rate limit: two calls pass, the third is 429 with `Retry-After`.
    for _ in 0..2 {
        admin.get(&format!("{BASE}/limited")).await?.expect(200)?;
    }
    let limited = admin.get(&format!("{BASE}/limited")).await?;
    limited.expect(429)?.expect_error("api.rate_limited")?;
    ensure!(
        limited.header("retry-after").is_some(),
        "a spent rate limit carries no Retry-After: {}",
        limited.text()
    );
    Ok(())
}
