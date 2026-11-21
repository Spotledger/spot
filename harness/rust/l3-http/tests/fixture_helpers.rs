//! The fixture helpers for the identity and field-security gates (WP-TST-10.T6): step-up, a user
//! with a generated password, and masked and revealing field grants. They run against the real
//! stack, so they need `SPOT_TEST_PG_URL`; without it each test prints why and returns.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_http::fixture::{FieldGrant, MANAGER, VIEWER};
use l3_http::{Client, Fixture};
use serde_json::json;

macro_rules! fixture {
    () => {
        match Fixture::start().await {
            Some(fixture) => fixture,
            None => {
                println!("SKIPPED: set SPOT_TEST_PG_URL (and SPOT_REQUIRE_PG=1 so CI cannot skip)");
                return;
            }
        }
    };
}

// --- no database needed -------------------------------------------------------------------------

#[test]
fn the_field_guard_channel_list_is_reachable_from_the_harness() {
    // The dev-dependency the field-security cases read: a case file fails when a channel has no
    // case, so the list must be visible here.
    assert!(!spot_field_guard::Channel::ALL.is_empty());
    assert!(
        spot_field_guard::Channel::ALL
            .iter()
            .any(|c| c.as_str() == "version")
    );
}

// --- surfaces the host does not serve yet ---------------------------------------------------------

/// True, after printing why, when the host mounts no route at `path`: the router answers an
/// unmounted path with a bare 404 (no error envelope), unlike a mounted route that refuses a
/// request with a coded error. The same convention as the blocked cases of the suite: a surface
/// the host does not serve is reported with its owner instead of failing, and a host that does
/// serve it runs the whole test.
///
/// Today `auth_step_up` is `NO_HANDLER` in `spot-http`'s `route_table.rs` test (owner: the
/// identity work that hosts step-up) and no host calls `with_reveal`, so `field_reveal` is not
/// mounted either (owner: the sealed-field reveal work, WP-SEC-10).
async fn route_is_not_served(client: &Client, path: &str, what: &str) -> bool {
    let reply = client
        .send_json("POST", path, &[], &json!({}))
        .await
        .unwrap();
    if reply.status == 404 && reply.error_code().is_none() {
        println!("BLOCKED: the host mounts no route for {what} (POST {path})");
        return true;
    }
    false
}

// --- step-up ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_up_token_is_accepted_by_a_step_up_only_route_and_a_plain_session_is_refused() {
    let mut fixture = fixture!();
    let manager = fixture.user(MANAGER).clone();
    let plain = fixture.sign_in(MANAGER).await.unwrap();
    // Creating an API key is a write that needs a recent step-up (`/api/v1/identity/api-keys`);
    // the Manager holds no identity administration, so use the Administrator for the route and
    // compare a plain Administrator session with an elevated one.
    let admin_user = l3_http::User {
        id: fixture.admin_id().to_owned(),
        email: "administrator".to_owned(),
        password: Some(l3_http::fixture::ADMIN_PASSWORD.to_owned()),
        secret: None,
    };
    if route_is_not_served(&fixture.admin(), "/api/v1/auth/step-up", "`auth_step_up`").await {
        fixture.finish().await;
        return;
    }
    let elevated = fixture.step_up(&admin_user).await.unwrap();
    let body = json!({"name": "l3-helper-key"});
    let refused = fixture
        .admin()
        .send_json("POST", "/api/v1/identity/api-keys", &[], &body)
        .await
        .unwrap();
    refused.expect_error("auth.step_up_required").unwrap();
    let accepted = elevated
        .send_json("POST", "/api/v1/identity/api-keys", &[], &body)
        .await
        .unwrap();
    assert_ne!(
        accepted.error_code().as_deref(),
        Some("auth.step_up_required"),
        "the elevated session is still asked to step up: {}",
        accepted.redacted_text()
    );
    drop((manager, plain));
    fixture.finish().await;
}

// --- a user with a password -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_created_user_can_sign_in_with_its_generated_password() {
    let mut fixture = fixture!();
    let user = fixture
        .create_user_with_password("helper", VIEWER)
        .await
        .unwrap();
    let password = user.password.clone().unwrap();
    assert!(
        password.len() >= 12,
        "the generated password is long enough"
    );
    let client = fixture
        .client()
        .sign_in(&user.email, &password)
        .await
        .unwrap();
    client.session().await.unwrap().expect(200).unwrap();
    // The role took effect: a Viewer reads projects.
    client
        .list("project", "limit=1")
        .await
        .unwrap()
        .expect(200)
        .unwrap();
    // A second user gets a different password and a different address.
    let other = fixture
        .create_user_with_password("helper", VIEWER)
        .await
        .unwrap();
    assert_ne!(other.email, user.email);
    assert_ne!(other.password, user.password);
    // The fixture log never holds a password.
    let log = std::fs::read_to_string(fixture.log()).unwrap_or_default();
    assert!(
        !log.contains(&password),
        "the generated password reached the log"
    );
    fixture.finish().await;
}

// --- field grants -----------------------------------------------------------------------------------

async fn task_with_details(fixture: &Fixture, details: &str) -> String {
    let (project, _, _) = fixture.project("Grant helper").await.unwrap();
    let (task, version) = fixture.task(&project, "Grant helper task").await.unwrap();
    let manager = fixture.sign_in(MANAGER).await.unwrap();
    manager
        .update("task", &task, version, &json!({"details": details}))
        .await
        .unwrap()
        .expect(200)
        .unwrap();
    task
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_masked_grant_masks_over_http() {
    let mut fixture = fixture!();
    let value = "ACCOUNT-1234567890";
    let task = task_with_details(&fixture, value).await;
    let viewer = fixture.sign_in(VIEWER).await.unwrap();
    // Before the grant the field is hidden: no key at all.
    let hidden = viewer.document("task", &task).await.unwrap();
    hidden.expect(200).unwrap();
    assert!(hidden.data().get("details").is_none(), "{}", hidden.text());

    fixture
        .grant_field("task", "details", FieldGrant::Masked, VIEWER)
        .await
        .unwrap();
    let masked = viewer.document("task", &task).await.unwrap();
    masked.expect(200).unwrap();
    let shown = masked.data().get("details").cloned();
    assert!(
        shown.is_some(),
        "the masked form is present: {}",
        masked.text()
    );
    assert!(
        !masked.text().contains(value),
        "the value leaked through the mask"
    );
    // A reveal without the reveal right is refused with the one answer.
    let reveal_path = format!("/api/v1/resource/task/{task}/reveal");
    if route_is_not_served(&viewer, &reveal_path, "`field_reveal`").await {
        fixture.finish().await;
        return;
    }
    let refused = viewer
        .reveal(
            "task",
            &task,
            &json!({"field": "details", "reason": "l3 helper check"}),
        )
        .await
        .unwrap();
    refused.expect_error("permission.reveal_denied").unwrap();
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revealing_grant_reveals_one_value_with_a_reason() {
    let mut fixture = fixture!();
    let value = "ACCOUNT-0987654321";
    let task = task_with_details(&fixture, value).await;
    fixture
        .grant_field("task", "details", FieldGrant::Revealing, VIEWER)
        .await
        .unwrap();
    let viewer = fixture.sign_in(VIEWER).await.unwrap();
    // The document still shows the masked form; only the reveal route returns the value.
    let document = viewer.document("task", &task).await.unwrap();
    document.expect(200).unwrap();
    assert!(
        !document.text().contains(value),
        "the document carries the value"
    );
    let reveal_path = format!("/api/v1/resource/task/{task}/reveal");
    if route_is_not_served(&viewer, &reveal_path, "`field_reveal`").await {
        fixture.finish().await;
        return;
    }
    let revealed = viewer
        .reveal(
            "task",
            &task,
            &json!({"field": "details", "reason": "l3 helper check"}),
        )
        .await
        .unwrap();
    revealed.expect(200).unwrap();
    assert!(
        revealed.text().contains(value),
        "{}",
        revealed.redacted_text()
    );
    fixture.finish().await;
}
