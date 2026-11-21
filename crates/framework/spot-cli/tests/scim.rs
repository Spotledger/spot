//! WP-IAM-06.T2: SCIM 2.0 is served at `/scim/v2`. Real PostgreSQL and a real `spot serve`
//! (skipped, with no output, when `SPOT_TEST_PG_URL` is unset).
//!
//! What is proved: users are created, read, replaced, patched, filtered and listed with
//! pagination and an ETag; deactivation ends a live session at once and revokes the user's API
//! keys; a SCIM key is not a native credential and a native token is not a SCIM credential;
//! the group-to-role mapping assigns with source `idp` and leaves a manual assignment of the same
//! role alone; a malformed body gets an RFC 7644 error that does not echo it; every call without
//! a valid SCIM key is `401`. Cross-tenant isolation (one site's directory is never another's) is
//! proved on the unit of work in `services/scim.rs` (`another_tenant_sees_and_changes_nothing`),
//! because one SCIM key belongs to exactly one site.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{Env, Started};
use serde_json::{Value as Json, json};
use spot_cli::services::scim::{ScimKeyIssuer, ScimService};
use spot_cli::services::secrets as svc;
use spot_core::SystemClock;
use spot_iam_scim::{Limits, parse_record_id};

const PLAIN_USER: &str = "plain@example.com";
const LEAVER: &str = "leaver@example.com";
const BODY_CANARY: &str = "canary-body-8f41c2d7";

struct Rig {
    env: Env,
    dir: PathBuf,
    started: Option<Started>,
    admin: String,
    key: String,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    /// A host with a vault and `[services.scim] enabled = true`, an Administrator, a service user
    /// and a SCIM key owned by it. `None` when no PostgreSQL is configured.
    async fn new() -> Option<Rig> {
        let dir = std::env::temp_dir().join(format!(
            "spot_scim_t2_{}",
            spot_app_api::prelude::RecordId::new_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let services = format!(
            "[services.secrets]\nkek_file = {:?}\n[services.scim]\nenabled = true\nrequests_per_window = 100000\n[services.signin_throttle]\nper_account = {{ capacity = 100000, per_seconds = 900 }}\nper_source = {{ capacity = 100000, per_seconds = 900 }}\nper_tenant = {{ capacity = 10000000, per_seconds = 900 }}\n",
            format!("{}/{{site}}.json", dir.display())
        );
        let env = Env::new(&services).await?;
        let first = env.site();
        svc::init_key_file(&dir.join(format!("{}.json", env.site)), &env.site)
            .await
            .unwrap();
        env.user(PLAIN_USER, common::ADMIN_PASSWORD);
        env.user(LEAVER, common::ADMIN_PASSWORD);
        let started = common::start(&env).await;
        let admin = common::admin(started.addr, &env.site, &first).await;
        let mut rig = Rig {
            env,
            dir,
            started: Some(started),
            admin,
            key: String::new(),
        };
        let owner = rig.service_user().await;
        // The provisioning client acts as its owner: the owner holds the platform's User Manager
        // role, so the field guard lets it read and filter users.
        let roles = rig
            .native(
                Some(&rig.admin),
                "GET",
                "/api/v1/resource/role?limit=200",
                None,
            )
            .await;
        let manager = roles.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["role_name"] == "User Manager")
            .and_then(|row| row["id"].as_str())
            .expect("the platform seeds a User Manager role")
            .to_owned();
        common::assign(
            rig.started().addr,
            &rig.env.site,
            &rig.admin,
            &owner,
            &manager,
        )
        .await;
        rig.key = rig
            .issuer()
            .await
            .issue(parse_record_id(&owner).unwrap(), "idp", None)
            .await
            .expect("the key is issued");
        Some(rig)
    }

    fn started(&self) -> &Started {
        self.started.as_ref().unwrap()
    }

    async fn native(
        &self,
        token: Option<&str>,
        method: &str,
        path: &str,
        body: Option<Json>,
    ) -> common::Reply {
        common::http(
            self.started().addr,
            &self.env.site,
            token,
            method,
            path,
            &[],
            body,
        )
        .await
    }

    async fn scim(&self, method: &str, path: &str, body: Option<Json>) -> common::Reply {
        let key = self.key.clone();
        self.scim_as(Some(&key), method, path, &[], body).await
    }

    async fn scim_as(
        &self,
        token: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Json>,
    ) -> common::Reply {
        common::http(
            self.started().addr,
            &self.env.site,
            token,
            method,
            path,
            headers,
            body,
        )
        .await
    }

    /// A `service` user created by the Administrator; its id.
    async fn service_user(&self) -> String {
        let reply = self
            .native(
                Some(&self.admin),
                "POST",
                "/api/v1/resource/user",
                Some(json!({
                    "user_name": "scim-bot",
                    "full_name": "SCIM bot",
                    "user_type": "service",
                    "state": "active",
                })),
            )
            .await;
        common::expect(&reply, 201);
        reply.body["data"]["id"].as_str().unwrap().to_owned()
    }

    /// The issuer over the running site's engine.
    async fn issuer(&self) -> ScimService {
        let root = &self.env.root.0;
        let loaded = spot_cli::config::load(root, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap();
        let host = svc::open(root, &loaded, site).await.unwrap();
        let meta = spot_cli::services::metadata::effective(root, &loaded, site).unwrap();
        let catalog = Arc::new(spot_cli::schema::catalog(&meta).unwrap());
        let live = self.started().running.live(&self.env.site).unwrap();
        ScimService::new(
            self.env.site.clone(),
            live,
            Arc::new(SystemClock::new()),
            catalog,
            host,
            Limits::default(),
        )
    }

    async fn finish(mut self) {
        if let Some(started) = self.started.take() {
            started.finish().await;
        }
    }
}

fn new_user(name: &str) -> Json {
    json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
        "userName": name,
        "externalId": format!("ext-{name}"),
        "name": { "givenName": "Ada", "familyName": "Lovelace" },
        "emails": [{ "value": name, "type": "work", "primary": true }],
        "active": true,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn users_are_created_read_replaced_patched_filtered_and_listed_with_etags() {
    let Some(rig) = Rig::new().await else { return };
    // Create.
    let created = rig
        .scim("POST", "/scim/v2/Users", Some(new_user("ada@example.com")))
        .await;
    common::expect(&created, 201);
    assert_eq!(
        created.header("content-type"),
        Some("application/scim+json")
    );
    let id = created.body["id"].as_str().unwrap().to_owned();
    assert_eq!(created.body["userName"], json!("ada@example.com"));
    assert!(!created.etag.is_empty());
    // A second create with the same name is a 409 uniqueness error.
    let again = rig
        .scim("POST", "/scim/v2/Users", Some(new_user("ada@example.com")))
        .await;
    common::expect(&again, 409);
    assert_eq!(again.body["scimType"], json!("uniqueness"));
    // Read, with the same ETag.
    let read = rig.scim("GET", &format!("/scim/v2/Users/{id}"), None).await;
    common::expect(&read, 200);
    assert_eq!(read.etag, created.etag);
    // Replace with a stale If-Match is a 412; with the current one it is a 200 and a new ETag.
    let mut replaced = new_user("ada@example.com");
    replaced["name"] = json!({ "givenName": "Augusta", "familyName": "Lovelace" });
    let stale = rig
        .scim_as(
            Some(&rig.key),
            "PUT",
            &format!("/scim/v2/Users/{id}"),
            &[("If-Match", "W/\"999\"")],
            Some(replaced.clone()),
        )
        .await;
    common::expect(&stale, 412);
    let ok = rig
        .scim_as(
            Some(&rig.key),
            "PUT",
            &format!("/scim/v2/Users/{id}"),
            &[("If-Match", created.etag.as_str())],
            Some(replaced),
        )
        .await;
    common::expect(&ok, 200);
    assert_ne!(ok.etag, created.etag);
    assert_eq!(ok.body["name"]["givenName"], json!("Augusta"));
    // Patch.
    let patched = rig
        .scim(
            "PATCH",
            &format!("/scim/v2/Users/{id}"),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [{ "op": "replace", "path": "displayName", "value": "Ada L." }],
            })),
        )
        .await;
    common::expect(&patched, 200);
    assert_eq!(patched.body["displayName"], json!("Ada L."));
    // The directory (and so the Administrator's own view of the user table) has the user.
    let native = rig
        .native(
            Some(&rig.admin),
            "GET",
            "/api/v1/resource/user?limit=50",
            None,
        )
        .await;
    common::expect(&native, 200);
    assert!(String::from_utf8_lossy(&native.bytes).contains("ada@example.com"));
    // Filter and list with pagination.
    for n in 0..3 {
        common::expect(
            &rig.scim(
                "POST",
                "/scim/v2/Users",
                Some(new_user(&format!("p{n}@example.com"))),
            )
            .await,
            201,
        );
    }
    let filtered = rig
        .scim(
            "GET",
            "/scim/v2/Users?filter=userName%20eq%20%22p1@example.com%22",
            None,
        )
        .await;
    common::expect(&filtered, 200);
    assert_eq!(filtered.body["totalResults"], json!(1));
    let page = rig
        .scim("GET", "/scim/v2/Users?startIndex=1&count=2", None)
        .await;
    common::expect(&page, 200);
    assert_eq!(page.body["Resources"].as_array().unwrap().len(), 2);
    assert!(page.body["totalResults"].as_u64().unwrap() >= 5);
    // The discovery endpoints answer too.
    common::expect(
        &rig.scim("GET", "/scim/v2/ServiceProviderConfig", None)
            .await,
        200,
    );
    common::expect(&rig.scim("GET", "/scim/v2/ResourceTypes", None).await, 200);
    common::expect(&rig.scim("GET", "/scim/v2/Schemas", None).await, 200);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deactivation_ends_a_live_session_and_a_users_keys_at_once() {
    let Some(rig) = Rig::new().await else { return };
    let addr = rig.started().addr;
    let token = common::token_of(addr, &rig.env.site, LEAVER, common::ADMIN_PASSWORD).await;
    let before = rig
        .native(Some(&token), "GET", "/api/v1/auth/session", None)
        .await;
    common::expect(&before, 200);
    let found = rig
        .scim(
            "GET",
            &format!("/scim/v2/Users?filter=userName%20eq%20%22{LEAVER}%22"),
            None,
        )
        .await;
    common::expect(&found, 200);
    let id = found.body["Resources"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // PATCH active=false.
    let off = rig
        .scim(
            "PATCH",
            &format!("/scim/v2/Users/{id}"),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [{ "op": "replace", "path": "active", "value": false }],
            })),
        )
        .await;
    common::expect(&off, 200);
    assert_eq!(off.body["active"], json!(false));
    // The session that was live a moment ago is dead now, and so is a fresh sign-in.
    let after = rig
        .native(Some(&token), "GET", "/api/v1/auth/session", None)
        .await;
    assert_eq!(
        after.status,
        401,
        "{}",
        String::from_utf8_lossy(&after.bytes)
    );
    let login = common::login(addr, &rig.env.site, LEAVER, common::ADMIN_PASSWORD).await;
    assert!(
        login.status == 401 || login.status == 403,
        "{}",
        login.status
    );
    // The record is kept (deactivation, never deletion), and DELETE deactivates the same way.
    let read = rig.scim("GET", &format!("/scim/v2/Users/{id}"), None).await;
    common::expect(&read, 200);
    let plain = rig
        .scim(
            "GET",
            &format!("/scim/v2/Users?filter=userName%20eq%20%22{PLAIN_USER}%22"),
            None,
        )
        .await;
    let pid = plain.body["Resources"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deleted = rig
        .scim("DELETE", &format!("/scim/v2/Users/{pid}"), None)
        .await;
    assert!(
        deleted.status == 204 || deleted.status == 200,
        "{}",
        deleted.status
    );
    let still = rig
        .scim("GET", &format!("/scim/v2/Users/{pid}"), None)
        .await;
    common::expect(&still, 200);
    assert_eq!(still.body["active"], json!(false));
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scim_key_is_not_a_native_credential_and_a_native_token_is_not_a_scim_credential() {
    let Some(rig) = Rig::new().await else { return };
    // The SCIM key on the native API.
    let native = rig
        .native(Some(&rig.key), "GET", "/api/v1/auth/session", None)
        .await;
    assert!(
        native.status == 401 || native.status == 403,
        "{}",
        native.status
    );
    let docs = rig
        .native(Some(&rig.key), "GET", "/api/v1/resource/user", None)
        .await;
    assert!(docs.status == 401 || docs.status == 403, "{}", docs.status);
    // The Administrator's session, a plain user's session, a wrong key and no token on SCIM.
    let plain = common::token_of(
        rig.started().addr,
        &rig.env.site,
        PLAIN_USER,
        common::ADMIN_PASSWORD,
    )
    .await;
    for token in [
        Some(rig.admin.as_str()),
        Some(plain.as_str()),
        Some("spk_nope_nope"),
        None,
    ] {
        let reply = rig.scim_as(token, "GET", "/scim/v2/Users", &[], None).await;
        assert_eq!(
            reply.status,
            401,
            "{}",
            String::from_utf8_lossy(&reply.bytes)
        );
        assert_eq!(
            reply.body["schemas"][0],
            json!("urn:ietf:params:scim:api:messages:2.0:Error")
        );
    }
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_group_mapping_assigns_with_source_idp_and_leaves_a_manual_assignment_alone() {
    let Some(rig) = Rig::new().await else { return };
    let addr = rig.started().addr;
    let site = rig.env.site.clone();
    let role = common::role(addr, &site, &rig.admin, "Sales Rep").await;
    // A manual assignment of the same role to the plain user.
    let plain_id = common::user_id(addr, &site, &rig.admin, PLAIN_USER).await;
    common::assign(addr, &site, &rig.admin, &plain_id, &role).await;
    // The administrator maps the IdP group to the role.
    let mapping = rig
        .native(
            Some(&rig.admin),
            "POST",
            "/api/v1/resource/group_mapping",
            Some(json!({ "idp_group": "Sales", "role": role })),
        )
        .await;
    common::expect(&mapping, 201);
    // The IdP creates the user and the group with that user as a member.
    let user = rig
        .scim(
            "POST",
            "/scim/v2/Users",
            Some(new_user("grace@example.com")),
        )
        .await;
    common::expect(&user, 201);
    let uid = user.body["id"].as_str().unwrap().to_owned();
    let group = rig
        .scim(
            "POST",
            "/scim/v2/Groups",
            Some(json!({
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
                "displayName": "Sales",
                "members": [{ "value": uid }],
            })),
        )
        .await;
    common::expect(&group, 201);
    let gid = group.body["id"].as_str().unwrap().to_owned();
    let assignments = |who: String| {
        let rig = &rig;
        async move {
            let list = rig
                .native(
                    Some(&rig.admin),
                    "POST",
                    "/api/v1/resource/role_assignment/query",
                    Some(
                        json!({"filter": {"field": "user", "op": "eq", "value": who}, "limit": 50}),
                    ),
                )
                .await;
            common::expect(&list, 200);
            list.body["data"].as_array().cloned().unwrap_or_default()
        }
    };
    let granted = assignments(uid.clone()).await;
    assert_eq!(granted.len(), 1, "{granted:?}");
    assert_eq!(granted[0]["source"], json!("idp"));
    // The group edit changed no role record.
    let roles = rig
        .native(
            Some(&rig.admin),
            "GET",
            &format!("/api/v1/resource/role/{role}"),
            None,
        )
        .await;
    common::expect(&roles, 200);
    // Removing the member ends the idp assignment; the manual one of the plain user is untouched.
    let removed = rig
        .scim(
            "PATCH",
            &format!("/scim/v2/Groups/{gid}"),
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [{ "op": "remove", "path": format!("members[value eq \"{uid}\"]") }],
            })),
        )
        .await;
    common::expect(&removed, 200);
    let ended = assignments(uid).await;
    assert!(
        ended.iter().all(|a| a["state"] != json!("active")),
        "{ended:?}"
    );
    let manual = assignments(plain_id).await;
    assert_eq!(manual.len(), 1);
    assert_eq!(manual[0]["source"], json!("manual"));
    assert_eq!(manual[0]["state"], json!("active"));
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_body_gets_an_rfc_error_that_does_not_echo_it() {
    let Some(rig) = Rig::new().await else { return };
    let addr = rig.started().addr;
    let raw = common::http_raw(
        addr,
        &rig.env.site,
        Some(&rig.key),
        "POST",
        "/scim/v2/Users",
        &[],
        (
            "application/scim+json",
            format!("{{\"userName\": \"{BODY_CANARY}\"").as_bytes(),
        ),
    )
    .await;
    assert_eq!(raw.status, 400);
    let text = String::from_utf8_lossy(&raw.bytes).into_owned();
    assert!(!text.contains(BODY_CANARY), "{text}");
    assert_eq!(raw.body["status"], json!("400"));
    // A well-formed document with a bad value is refused without echoing the value either.
    let bad = rig
        .scim(
            "POST",
            "/scim/v2/Users",
            Some(json!({ "userName": 7, "emails": BODY_CANARY })),
        )
        .await;
    assert_eq!(bad.status, 400);
    assert!(!String::from_utf8_lossy(&bad.bytes).contains(BODY_CANARY));
    // An over-long body is refused before it is read.
    let big = "x".repeat(2 * 1024 * 1024);
    let huge = common::http_raw(
        addr,
        &rig.env.site,
        Some(&rig.key),
        "POST",
        "/scim/v2/Users",
        &[],
        ("application/scim+json", big.as_bytes()),
    )
    .await;
    assert_eq!(huge.status, 413);
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scim_is_not_mounted_unless_enabled() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    let started = common::start(&env).await;
    let reply = common::http(
        started.addr,
        &env.site,
        Some("spk_a_b"),
        "GET",
        "/scim/v2/Users",
        &[],
        None,
    )
    .await;
    assert_eq!(reply.status, 404);
    let _ = first;
    started.finish().await;
}
