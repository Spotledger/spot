//! The HTTP node: method, URL template, headers, body mapping, response mapping and
//! status-to-outcome rules, every call through the egress policy.
//!
//! The handler builds the request from the node's config and the mapping context, opens the
//! credential if the config names one, and sends it through an [`EgressPort`] — it never opens a
//! socket. The `Idempotency-Key` header is the node's `(case, node, attempt)` key, so a retry
//! carries the same key and a receiver can deduplicate.
//!
//! The status rules decide success, retry or error. A transport failure or a timeout is retried
//! within the node's [`RetryPolicy`]; a policy denial, a non-retryable status and a malformed
//! config take the error branch at once.

use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::Value as Json;
use spot_bpm_engine::NodeOutcome;

use crate::config::{AuthScheme, HttpNodeConfig, StatusClass};
use crate::error::IntegrationError;
use crate::handler::{IntegrationCtx, IntegrationHandler, IntegrationKind};
use crate::ports::{EgressPort, HttpCall};
use crate::util::base64_encode;

/// The header a retry carries the idempotency key in.
pub const IDEMPOTENCY_HEADER: &str = "Idempotency-Key";

/// Runs HTTP nodes through an egress port.
pub struct HttpNodeHandler {
    egress: Arc<dyn EgressPort>,
    tenant: String,
}

impl HttpNodeHandler {
    /// A handler that sends through `egress` for `tenant`.
    #[must_use]
    pub fn new(egress: Arc<dyn EgressPort>, tenant: impl Into<String>) -> Self {
        Self {
            egress,
            tenant: tenant.into(),
        }
    }
}

#[async_trait]
impl IntegrationHandler for HttpNodeHandler {
    fn kind(&self) -> IntegrationKind {
        IntegrationKind::Http
    }

    async fn run(&self, ctx: &mut IntegrationCtx<'_>) -> NodeOutcome {
        let config = match HttpNodeConfig::from_json(&ctx.node.to_string(), ctx.config) {
            Ok(config) => config,
            Err(error) => return ctx.classify_failure(&error, &default_retry()),
        };
        let retry = config.retry.clone();
        match self.call(ctx, &config).await {
            Ok(outcome) => outcome,
            Err(error) => ctx.classify_failure(&error, &retry),
        }
    }
}

impl HttpNodeHandler {
    /// Builds, sends and interprets one request. A returned [`NodeOutcome`] is the classified
    /// response; an [`IntegrationError`] is a failure the caller classifies against the policy.
    async fn call(
        &self,
        ctx: &mut IntegrationCtx<'_>,
        config: &HttpNodeConfig,
    ) -> Result<NodeOutcome, IntegrationError> {
        let context = ctx.mapping_context();
        let url = crate::mapping::render_template(&config.url, &context)?;
        let mut headers = http::HeaderMap::new();
        for (name, template) in &config.headers {
            let value = crate::mapping::render_template(template, &context)?;
            insert_header(&mut headers, name, &value)?;
        }

        let mut auth_logged = "-";
        if let Some(auth) = &config.auth {
            // The credential goes only to a host the author wrote down: checked on the rendered
            // URL before the secret is opened, so a refusal never touches it.
            crate::host::ensure_credential_host(
                &ctx.node.to_string(),
                &config.url,
                &config.allowed_hosts,
                &url,
            )?;
            let secret = ctx.resolve(&auth.credential_ref).await?;
            let header = auth_header_name(auth)?;
            let value = match auth.scheme {
                AuthScheme::Bearer => format!("Bearer {}", secret.expose()),
                AuthScheme::Basic => {
                    let user = auth.username.as_deref().unwrap_or("");
                    format!(
                        "Basic {}",
                        base64_encode(format!("{user}:{}", secret.expose()).as_bytes())
                    )
                }
                AuthScheme::Header => secret.expose().to_owned(),
            };
            let header_value =
                HeaderValue::from_str(&value).map_err(|e| IntegrationError::ConfigInvalid {
                    node: ctx.node.to_string(),
                    detail: format!("the credential header is not a valid header value: {e}"),
                })?;
            headers.insert(header, header_value);
            // Never log the header value: a Basic header carries the secret base64-encoded, which
            // text redaction of the raw secret would not catch.
            auth_logged = "[credential]";
        }

        let key = ctx.key.to_string();
        insert_header(&mut headers, IDEMPOTENCY_HEADER, &key)?;

        let body = if config.body.is_some() {
            let mapped = ctx.apply_mapping(config.body.as_ref())?;
            Some(
                serde_json::to_vec(&mapped).map_err(|e| IntegrationError::ConfigInvalid {
                    node: ctx.node.to_string(),
                    detail: format!("the request body did not serialise: {e}"),
                })?,
            )
        } else {
            None
        };

        // Log only through the registry: the summary may hold the credential header, and a value
        // that could not be registered was replaced by a placeholder above.
        let summary = ctx.redact_reason(&format!(
            "{} {} auth={auth_logged}",
            config.method.to_egress().as_str(),
            url
        ));
        tracing::debug!(target: "bpm_integration", node = %ctx.node, attempt = ctx.attempt, "http {summary}");

        let call = HttpCall {
            method: config.method.to_egress(),
            url,
            headers,
            body,
        };
        let response = self.egress.send(&self.tenant, &call).await?;

        match config.status.classify(response.status) {
            StatusClass::Success => {
                let parsed = response.body_json();
                let mut output = match (config.response.as_ref(), parsed) {
                    (Some(mapping), body) => {
                        let mut extended = context;
                        let response_value = body.unwrap_or(Json::Null);
                        if let Json::Object(extended_map) = &mut extended {
                            extended_map.insert(String::from("response"), response_value);
                        }
                        mapping.apply(&extended, ctx.now)?
                    }
                    (None, Some(body)) => body,
                    (None, None) => Json::Null,
                };
                // An endpoint may echo a credential back; the output must not carry it on.
                ctx.redact_json(&mut output);
                Ok(NodeOutcome::Done {
                    output: Some(output),
                    branches: Vec::new(),
                })
            }
            StatusClass::Retry => Ok(ctx.retry_or_fail(
                "bpm_integration.http_status",
                &ctx.redact_reason(&format!("HTTP {} may succeed on retry", response.status)),
                &config.retry,
            )),
            StatusClass::Error => Ok(NodeOutcome::Fail {
                code: String::from("bpm_integration.http_status"),
                reason: ctx.redact_reason(&format!("HTTP {}", response.status)),
            }),
        }
    }
}

/// The header an auth scheme writes to.
fn auth_header_name(auth: &crate::config::CredentialAuth) -> Result<HeaderName, IntegrationError> {
    match auth.scheme {
        AuthScheme::Header => HeaderName::from_bytes(auth.header.as_bytes()).map_err(|e| {
            IntegrationError::ConfigInvalid {
                node: auth.header.clone(),
                detail: format!("`{}` is not a valid header name: {e}", auth.header),
            }
        }),
        _ => Ok(http::header::AUTHORIZATION),
    }
}

/// Inserts a header, mapping an invalid name or value to a config error.
fn insert_header(
    headers: &mut http::HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), IntegrationError> {
    let name =
        HeaderName::from_bytes(name.as_bytes()).map_err(|e| IntegrationError::ConfigInvalid {
            node: String::from("http"),
            detail: format!("`{name}` is not a valid header name: {e}"),
        })?;
    let value = HeaderValue::from_str(value).map_err(|e| IntegrationError::ConfigInvalid {
        node: String::from("http"),
        detail: format!("a header value is not valid: {e}"),
    })?;
    headers.insert(name, value);
    Ok(())
}

/// A conservative retry policy for a failure that happened before the config was read.
fn default_retry() -> crate::config::RetryPolicy {
    crate::config::RetryPolicy {
        max_attempts: 1,
        backoff_secs: 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{CredentialAuth, HttpMethod};
    use crate::credential::InMemorySecretResolver;
    use crate::handler::{IntegrationCtx, IntegrationKind};
    use crate::ports::{FakeEgress, HttpResponse};
    use crate::registry::SecretRegistry;
    use http::HeaderMap;
    use serde_json::json;
    use spot_app_api::prelude::OffsetDateTime;
    use spot_bpm_engine::{CaseId, IdempotencyKey};
    use spot_dag::NodeId;

    const CANARY: &str = "canary-secret-value-0123456789";

    struct Fixture {
        case: CaseId,
        node: NodeId,
        input: Json,
        outputs: Json,
        variables: Json,
        secrets: InMemorySecretResolver,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                case: CaseId::new("case1").unwrap(),
                node: NodeId::new("n1").unwrap(),
                input: json!({ "order_id": "SO-1" }),
                outputs: json!({}),
                variables: json!({ "qty": 2 }),
                secrets: InMemorySecretResolver::new(),
            }
        }

        fn now() -> OffsetDateTime {
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
        }

        async fn run(&mut self, config: Json, handler: &HttpNodeHandler) -> NodeOutcome {
            let mut registry = SecretRegistry::new().unwrap();
            let mut ctx = IntegrationCtx {
                case: &self.case,
                node: &self.node,
                attempt: 1,
                key: IdempotencyKey::new(self.case.clone(), self.node.clone(), 1),
                is_retry: false,
                now: Self::now(),
                principal: None,
                input: &self.input,
                node_outputs: &self.outputs,
                variables: &mut self.variables,
                config: &config,
                secrets: &self.secrets,
                registry: &mut registry,
            };
            handler.run(&mut ctx).await
        }
    }

    fn ok(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn a_get_renders_the_url_and_returns_the_body() {
        let fake = Arc::new(FakeEgress::new());
        fake.push_response(ok(r#"{"id":"SO-1","total":42}"#));
        let handler = HttpNodeHandler::new(fake.clone(), "acme");
        let mut fixture = Fixture::new();
        let outcome = fixture
            .run(
                json!({ "url": "https://api.test/orders/{input.order_id}" }),
                &handler,
            )
            .await;
        match outcome {
            NodeOutcome::Done { output, .. } => {
                assert_eq!(output, Some(json!({ "id": "SO-1", "total": 42 })));
            }
            other => panic!("expected done, got {other:?}"),
        }
        let calls = fake.calls();
        assert_eq!(calls[0].url, "https://api.test/orders/SO-1");
        assert_eq!(calls[0].method, spot_egress::Method::Get);
    }

    #[tokio::test]
    async fn the_idempotency_key_is_sent_as_a_header() {
        let fake = Arc::new(FakeEgress::new());
        fake.push_response(ok("{}"));
        let handler = HttpNodeHandler::new(fake.clone(), "acme");
        let mut fixture = Fixture::new();
        let _ = fixture
            .run(json!({ "url": "https://api.test/x" }), &handler)
            .await;
        let calls = fake.calls();
        assert_eq!(
            calls[0]
                .headers
                .get(IDEMPOTENCY_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
            "case1:n1:1"
        );
    }

    #[tokio::test]
    async fn a_body_mapping_is_sent_and_a_response_mapping_applied() {
        let fake = Arc::new(FakeEgress::new());
        fake.push_response(ok(r#"{"status":"accepted","id":"X"}"#));
        let handler = HttpNodeHandler::new(fake.clone(), "acme");
        let mut fixture = Fixture::new();
        let config = json!({
            "method": "post",
            "url": "https://api.test/orders",
            "body": { "qty": "doc.case.qty" },
            "response": { "remote_id": "doc.response.id" },
        });
        let outcome = fixture.run(config, &handler).await;
        match outcome {
            NodeOutcome::Done { output, .. } => {
                assert_eq!(output, Some(json!({ "remote_id": "X" })))
            }
            other => panic!("expected done, got {other:?}"),
        }
        let calls = fake.calls();
        assert_eq!(calls[0].method, spot_egress::Method::Post);
        assert_eq!(
            serde_json::from_slice::<Json>(calls[0].body.as_deref().unwrap()).unwrap(),
            json!({ "qty": 2 })
        );
    }

    #[tokio::test]
    async fn a_credential_becomes_a_bearer_header_and_never_an_output() {
        let fake = Arc::new(FakeEgress::new());
        fake.push_response(ok(r#"{"echo":"canary-secret-value-0123456789"}"#));
        let handler = HttpNodeHandler::new(fake.clone(), "acme");
        let mut fixture = Fixture::new();
        fixture.secrets.insert("sales_api", CANARY);
        let config = json!({
            "url": "https://api.test/x",
            "auth": { "credential_ref": "sales_api", "scheme": "bearer" },
        });
        let outcome = fixture.run(config, &handler).await;
        let calls = fake.calls();
        assert_eq!(
            calls[0]
                .headers
                .get(http::header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {CANARY}")
        );
        // The response body echoed the canary; the node's output must not carry it.
        if let NodeOutcome::Done { output, .. } = outcome {
            let text = output.unwrap().to_string();
            assert!(
                !text.contains(CANARY),
                "the output leaked the credential: {text}"
            );
        } else {
            panic!("expected done");
        }
    }

    fn credentialed(url: &str) -> Json {
        json!({
            "url": url,
            "auth": { "credential_ref": "sales_api", "scheme": "bearer" },
        })
    }

    async fn run_credentialed(config: Json, input: Json) -> (NodeOutcome, Arc<FakeEgress>) {
        let fake = Arc::new(FakeEgress::new());
        fake.push_response(ok("{}"));
        let handler = HttpNodeHandler::new(fake.clone(), "acme");
        let mut fixture = Fixture::new();
        fixture.input = input;
        fixture.secrets.insert("sales_api", CANARY);
        let outcome = fixture.run(config, &handler).await;
        (outcome, fake)
    }

    fn assert_host_refused(outcome: &NodeOutcome, fake: &FakeEgress) {
        match outcome {
            NodeOutcome::Fail { code, reason } => {
                assert_eq!(code, "bpm_integration.credential_host_mismatch");
                assert!(!reason.contains(CANARY), "{reason}");
                assert!(
                    !reason.contains("evil"),
                    "the refusal names the steered host: {reason}"
                );
                assert!(!format!("{outcome:?}").contains(CANARY));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(fake.calls().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn a_credentialed_host_from_mapped_data_is_refused() {
        let (outcome, fake) = run_credentialed(
            credentialed("https://{input.host}/x"),
            json!({ "host": "evil.test" }),
        )
        .await;
        assert_host_refused(&outcome, &fake);
        // Even a host that happens to be fine is refused: it came from data, not the config.
        let (outcome, fake) = run_credentialed(
            credentialed("https://{input.host}/x"),
            json!({ "host": "api.test" }),
        )
        .await;
        assert_host_refused_without_evil(&outcome, &fake);
    }

    fn assert_host_refused_without_evil(outcome: &NodeOutcome, fake: &FakeEgress) {
        assert!(
            matches!(outcome, NodeOutcome::Fail { code, .. } if code == "bpm_integration.credential_host_mismatch"),
            "{outcome:?}"
        );
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn a_literal_credentialed_host_passes_even_with_a_mapped_path() {
        let (outcome, fake) = run_credentialed(
            credentialed("https://API.test/orders/{input.order_id}"),
            json!({ "order_id": "SO-1" }),
        )
        .await;
        assert!(matches!(outcome, NodeOutcome::Done { .. }), "{outcome:?}");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].headers.contains_key(http::header::AUTHORIZATION));
    }

    #[tokio::test]
    async fn an_allowed_host_lets_a_mapped_host_through_only_on_an_exact_match() {
        let mut config = credentialed("https://{input.host}/x");
        config["allowed_hosts"] = json!(["api.test", "eu.api.test"]);
        let (outcome, fake) =
            run_credentialed(config.clone(), json!({ "host": "eu.api.test" })).await;
        assert!(matches!(outcome, NodeOutcome::Done { .. }), "{outcome:?}");
        assert_eq!(fake.calls().len(), 1);
        let (outcome, fake) = run_credentialed(config, json!({ "host": "evil.test" })).await;
        assert_host_refused(&outcome, &fake);
    }

    #[tokio::test]
    async fn userinfo_and_look_alike_hosts_are_refused() {
        for url in [
            "https://api.test@evil.test/x",
            "https://evil.test@api.test/x",
            "https://api.test.evil.test/x",
            "https://evil-api.test/x",
        ] {
            let (outcome, fake) = run_credentialed(
                credentialed("https://{input.u}"),
                json!({ "u": url.trim_start_matches("https://") }),
            )
            .await;
            assert!(
                matches!(&outcome, NodeOutcome::Fail { code, .. } if code == "bpm_integration.credential_host_mismatch"),
                "{url}: {outcome:?}"
            );
            assert!(fake.calls().is_empty(), "{url}");
        }
        // The same strings written literally in the url are refused too: userinfo is never sent.
        let (outcome, fake) =
            run_credentialed(credentialed("https://api.test@api.test/x"), json!({})).await;
        assert_host_refused_without_evil(&outcome, &fake);
    }

    #[tokio::test]
    async fn a_node_without_a_credential_may_still_use_a_mapped_host() {
        let fake = Arc::new(FakeEgress::new());
        fake.push_response(ok("{}"));
        let handler = HttpNodeHandler::new(fake.clone(), "acme");
        let mut fixture = Fixture::new();
        fixture.input = json!({ "host": "anywhere.test" });
        let outcome = fixture
            .run(json!({ "url": "https://{input.host}/x" }), &handler)
            .await;
        assert!(matches!(outcome, NodeOutcome::Done { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed_so_the_credential_goes_nowhere_else() {
        let fake = Arc::new(FakeEgress::new());
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::LOCATION,
            HeaderValue::from_static("https://evil.test/steal"),
        );
        fake.push_response(HttpResponse {
            status: 302,
            headers,
            body: Vec::new(),
        });
        let handler = HttpNodeHandler::new(fake.clone(), "acme");
        let mut fixture = Fixture::new();
        fixture.secrets.insert("sales_api", CANARY);
        let outcome = fixture
            .run(credentialed("https://api.test/x"), &handler)
            .await;
        assert!(matches!(outcome, NodeOutcome::Fail { .. }), "{outcome:?}");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "the redirect was followed");
        assert_eq!(calls[0].url, "https://api.test/x");
        assert!(!format!("{outcome:?}").contains(CANARY));
    }

    #[test]
    fn a_bad_allowed_host_fails_validation() {
        let config = HttpNodeConfig::from_json(
            "n1",
            &json!({ "url": "https://api.test/x", "allowed_hosts": ["https://evil.test/"] }),
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn a_retryable_status_retries_and_a_client_error_fails() {
        let fake = Arc::new(FakeEgress::new());
        fake.push_response(HttpResponse {
            status: 503,
            headers: HeaderMap::new(),
            body: Vec::new(),
        });
        let handler = HttpNodeHandler::new(fake.clone(), "acme");
        let mut fixture = Fixture::new();
        let outcome = fixture
            .run(json!({ "url": "https://api.test/x" }), &handler)
            .await;
        assert!(matches!(outcome, NodeOutcome::Retry { .. }), "{outcome:?}");

        let fake = Arc::new(FakeEgress::new());
        fake.push_response(HttpResponse {
            status: 404,
            headers: HeaderMap::new(),
            body: Vec::new(),
        });
        let handler = HttpNodeHandler::new(fake, "acme");
        let mut fixture = Fixture::new();
        let outcome = fixture
            .run(json!({ "url": "https://api.test/x" }), &handler)
            .await;
        match outcome {
            NodeOutcome::Fail { code, .. } => assert_eq!(code, "bpm_integration.http_status"),
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_policy_denial_takes_the_error_branch_and_never_retries() {
        let fake = Arc::new(FakeEgress::new());
        fake.push_error(IntegrationError::EgressDenied {
            code: String::from("egress.denied_by_policy"),
        });
        let handler = HttpNodeHandler::new(fake, "acme");
        let mut fixture = Fixture::new();
        let outcome = fixture
            .run(json!({ "url": "https://metadata.test/x" }), &handler)
            .await;
        match outcome {
            NodeOutcome::Fail { code, .. } => assert_eq!(code, "bpm_integration.egress_denied"),
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_transport_error_retries() {
        let fake = Arc::new(FakeEgress::new());
        fake.push_error(IntegrationError::TransportFailed {
            detail: String::from("reset"),
        });
        let handler = HttpNodeHandler::new(fake, "acme");
        let mut fixture = Fixture::new();
        let outcome = fixture
            .run(json!({ "url": "https://api.test/x" }), &handler)
            .await;
        assert!(matches!(outcome, NodeOutcome::Retry { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_malformed_config_fails_the_node() {
        let fake = Arc::new(FakeEgress::new());
        let handler = HttpNodeHandler::new(fake, "acme");
        let mut fixture = Fixture::new();
        let outcome = fixture.run(json!({ "nope": 1 }), &handler).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => {
                assert_eq!(code, "bpm_integration.config_invalid");
            }
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[test]
    fn the_kind_is_http() {
        let handler = HttpNodeHandler::new(Arc::new(FakeEgress::new()), "acme");
        assert_eq!(handler.kind(), IntegrationKind::Http);
    }

    #[test]
    fn the_method_maps_to_the_sender() {
        assert_eq!(HttpMethod::Post.to_egress(), spot_egress::Method::Post);
        let auth = CredentialAuth {
            credential_ref: crate::credential::CredentialRef::new("x"),
            scheme: AuthScheme::Header,
            header: String::from("X-Api-Key"),
            username: None,
        };
        assert_eq!(auth_header_name(&auth).unwrap().as_str(), "x-api-key");
    }
}
