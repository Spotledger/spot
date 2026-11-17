//! The settings management service (WP-ADM-07.T2) over the seed catalogue and the in-memory
//! store: set/get/history, type and unknown-key refusals, the site's own scope only, restart
//! reporting, secrets as references, the high-risk operation split and a lost version race.
//! The store's own PostgreSQL cases are in `spot-settings-store-pg` (they need
//! `SPOT_TEST_PG_URL` and print their skip reason).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use spot_cli::services::settings_api::{SettingsApi, operations};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::handlers::{HandlerError, HandlerRequest};
use spot_mgmt_ops::{AcceptedParam, OpId, ParamType, ParamValue, Validated};
use spot_settings::{RegistryBuilder, load_seed_catalogue};
use spot_settings_store_pg::{MemorySettingsStore, SettingsBackend};
use time::OffsetDateTime;

fn api() -> (SettingsApi, Arc<MemorySettingsStore>) {
    let registry = RegistryBuilder::new()
        .register_all(load_seed_catalogue().unwrap())
        .build()
        .unwrap();
    let store = Arc::new(MemorySettingsStore::new());
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    ));
    let backend: Arc<dyn SettingsBackend> = store.clone();
    (
        SettingsApi::new(Arc::new(registry), backend, "acme", clock),
        store,
    )
}

fn request(op: &str, params: &[(&str, ParamValue)]) -> HandlerRequest {
    HandlerRequest {
        command_id: "cmd-1".to_owned(),
        principal: "operator:alice".to_owned(),
        operation: OpId::new(op).unwrap(),
        params: Validated {
            op: OpId::new(op).unwrap(),
            params: params
                .iter()
                .map(|(name, value)| AcceptedParam {
                    name: (*name).to_owned(),
                    ty: ParamType::Text {
                        max_len: 4000,
                        pattern: None,
                    },
                    value: value.clone(),
                    from_default: false,
                    secret: false,
                })
                .collect(),
        },
    }
}

fn t(s: &str) -> ParamValue {
    ParamValue::Text(s.to_owned())
}

fn code(error: HandlerError) -> &'static str {
    error.code
}

const KEY: &str = "tenant.backup.retention_days";

#[test]
fn the_catalogue_has_the_five_operations_and_the_high_risk_one_is_critical() {
    let ops = operations();
    let ids: Vec<&str> = ops.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "settings.list",
            "settings.get",
            "settings.set",
            "settings.set_high_risk",
            "settings.history"
        ]
    );
    let high = ops
        .iter()
        .find(|o| o.id.as_str() == "settings.set_high_risk")
        .unwrap();
    assert_eq!(format!("{:?}", high.risk), "Critical");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_get_and_history_round_trip() {
    let (api, _) = api();
    let set = api
        .set(
            &request("settings.set", &[("key", t(KEY)), ("value", t("90"))]),
            false,
        )
        .await
        .unwrap();
    assert_eq!(set["version"], 1);
    assert_eq!(set["applied"], true);
    let got = api
        .get(&request("settings.get", &[("key", t(KEY))]))
        .await
        .unwrap();
    assert_eq!(got["effective"]["value"]["Int"], 90);
    assert_eq!(got["origin"], "tenant:acme");
    let default = api
        .get(&request(
            "settings.get",
            &[("key", t("tenant.default_currency"))],
        ))
        .await
        .unwrap();
    assert_eq!(default["origin"], "default");
    api.set(
        &request(
            "settings.set",
            &[
                ("key", t(KEY)),
                ("value", t("120")),
                ("expected_version", ParamValue::Int(1)),
            ],
        ),
        false,
    )
    .await
    .unwrap();
    let history = api
        .history(&request("settings.history", &[("key", t(KEY))]))
        .await
        .unwrap();
    let rows = history["history"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["version"], 2);
    assert_eq!(rows[0]["actor"], "operator:alice");
    assert_eq!(rows[0]["channel"], "mgmt");
    assert_eq!(rows[0]["at"], 1_800_000_000);
    assert!(
        !history.to_string().contains("120"),
        "the history holds digests, not values"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_type_violation_an_unknown_key_and_a_bad_scope_are_refused_and_nothing_is_written() {
    let (api, store) = api();
    let set = |key: &str, value: &str, scope: Option<&str>| {
        let mut params = vec![("key", t(key)), ("value", t(value))];
        if let Some(s) = scope {
            params.push(("scope", t(s)));
        }
        request("settings.set", &params)
    };
    assert_eq!(
        code(api.set(&set(KEY, "ninety", None), false).await.unwrap_err()),
        "settings_api.value_invalid"
    );
    assert_eq!(
        code(api.set(&set(KEY, "99999", None), false).await.unwrap_err()),
        "settings.validation_failed"
    );
    assert_eq!(
        code(
            api.set(&set("no.such.key", "1", None), false)
                .await
                .unwrap_err()
        ),
        "settings.unknown_setting"
    );
    assert_eq!(
        code(
            api.set(&set(KEY, "5", Some("bogus")), false)
                .await
                .unwrap_err()
        ),
        "settings_api.scope_invalid"
    );
    // fleet is not a scope this setting allows
    assert_eq!(
        code(
            api.set(&set(KEY, "5", Some("fleet")), false)
                .await
                .unwrap_err()
        ),
        "settings.scope_not_allowed"
    );
    // another site's tenant scope, and a company of another site
    assert_eq!(
        code(
            api.set(&set(KEY, "5", Some("tenant:beta")), false)
                .await
                .unwrap_err()
        ),
        "settings_api.scope_forbidden"
    );
    assert_eq!(
        code(
            api.set(&set(KEY, "5", Some("company:beta/C1")), false)
                .await
                .unwrap_err()
        ),
        "settings_api.scope_forbidden"
    );
    assert!(
        store
            .list(&spot_settings::ScopeRef::Tenant("acme".to_owned()))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_company_of_this_site_is_writable_and_inherits_from_the_tenant() {
    let (api, _) = api();
    api.set(
        &request("settings.set", &[("key", t(KEY)), ("value", t("60"))]),
        false,
    )
    .await
    .unwrap();
    let company = api
        .get(&request(
            "settings.get",
            &[("key", t(KEY)), ("scope", t("company:acme/C1"))],
        ))
        .await
        .unwrap();
    assert_eq!(company["effective"]["value"]["Int"], 60);
    assert_eq!(company["origin"], "tenant:acme");
    api.set(
        &request(
            "settings.set",
            &[
                ("key", t(KEY)),
                ("value", t("7")),
                ("scope", t("company:acme/C1")),
            ],
        ),
        false,
    )
    .await
    .unwrap();
    let narrower = api
        .get(&request(
            "settings.get",
            &[("key", t(KEY)), ("scope", t("company:acme/C1"))],
        ))
        .await
        .unwrap();
    assert_eq!(narrower["effective"]["value"]["Int"], 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_setting_reports_restart_required_and_is_applied_whole() {
    let (api, _) = api();
    let set = api
        .set(
            &request(
                "settings.set",
                &[
                    ("key", t("runtime.http.listen_address")),
                    ("scope", t("fleet")),
                    ("value", t("0.0.0.0:9090")),
                ],
            ),
            false,
        )
        .await
        .unwrap();
    assert!(set["restart_required"].as_str().unwrap().contains("http"));
    let got = api
        .get(&request(
            "settings.get",
            &[
                ("key", t("runtime.http.listen_address")),
                ("scope", t("fleet")),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(got["effective"]["value"]["Text"], "0.0.0.0:9090");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_setting_is_a_reference_name_and_never_a_value() {
    let (api, _) = api();
    let plain = request(
        "settings.set",
        &[
            ("key", t("runtime.smtp.password")),
            ("scope", t("fleet")),
            ("value", t("Has Spaces And Caps")),
        ],
    );
    assert_eq!(
        code(api.set(&plain, true).await.unwrap_err()),
        "settings_api.secret_reference_invalid"
    );
    let good = request(
        "settings.set",
        &[
            ("key", t("runtime.smtp.password")),
            ("scope", t("fleet")),
            ("value", t("smtp-password-ref")),
        ],
    );
    api.set(&good, true).await.unwrap();
    let got = api
        .get(&request(
            "settings.get",
            &[("key", t("runtime.smtp.password")), ("scope", t("fleet"))],
        ))
        .await
        .unwrap();
    assert_eq!(got["effective"]["secret_reference"], "smtp-password-ref");
    let history = api
        .history(&request(
            "settings.history",
            &[("key", t("runtime.smtp.password")), ("scope", t("fleet"))],
        ))
        .await
        .unwrap();
    assert!(!history.to_string().contains("smtp-password-ref"));
    let list = api
        .list(&request(
            "settings.list",
            &[("scope", t("fleet")), ("area", t("runtime.smtp"))],
        ))
        .await
        .unwrap();
    assert!(list.to_string().contains("secret_reference"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_high_risk_setting_needs_the_high_risk_operation_and_the_reverse_is_refused() {
    let (api, _) = api();
    let params = [
        ("key", t("runtime.smtp.password")),
        ("scope", t("fleet")),
        ("value", t("ref-1")),
    ];
    assert_eq!(
        code(
            api.set(&request("settings.set", &params), false)
                .await
                .unwrap_err()
        ),
        "settings_api.use_high_risk_operation"
    );
    let low = [("key", t(KEY)), ("value", t("5"))];
    assert_eq!(
        code(
            api.set(&request("settings.set_high_risk", &low), true)
                .await
                .unwrap_err()
        ),
        "settings_api.not_a_high_risk_setting"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_set_loses_cleanly() {
    let (api, _) = api();
    let api = Arc::new(api);
    let attempt = |value: &'static str| {
        let api = Arc::clone(&api);
        tokio::spawn(async move {
            api.set(
                &request(
                    "settings.set",
                    &[
                        ("key", t(KEY)),
                        ("value", t(value)),
                        ("expected_version", ParamValue::Int(0)),
                    ],
                ),
                false,
            )
            .await
        })
    };
    let (a, b) = (attempt("1"), attempt("2"));
    let results = [a.await.unwrap(), b.await.unwrap()];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let loser = results.iter().find_map(|r| r.as_ref().err()).unwrap();
    assert_eq!(loser.code, "settings_api.version_conflict");
    let list: Value = api.list(&request("settings.list", &[])).await.unwrap();
    assert_eq!(list["scope"], json!("tenant:acme"));
}
