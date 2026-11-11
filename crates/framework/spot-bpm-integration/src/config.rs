//! Node configs: the typed shape of each integration node's config JSON.
//!
//! The engine hands a handler the node's config as canonical JSON. Each handler parses it into
//! one of these types with [`serde`], so a malformed config is a [`IntegrationError::ConfigInvalid`]
//! at run time and a `validate`-time check for the process validator. Every string that reaches
//! the outside world — a URL, a header value, a subject — may carry `{doc.<path>}` placeholders
//! (see [`crate::mapping::render_template`]); the mappings are validated statically where that is
//! possible.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value as Json;

use crate::credential::CredentialRef;
use crate::error::IntegrationError;
use crate::mapping::Mapping;

/// The HTTP method of a node's request. A closed set, like the egress sender's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpMethod {
    /// `GET`.
    #[default]
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl HttpMethod {
    /// The method as the egress sender spells it.
    #[must_use]
    pub fn to_egress(self) -> spot_egress::Method {
        match self {
            Self::Get => spot_egress::Method::Get,
            Self::Post => spot_egress::Method::Post,
            Self::Put => spot_egress::Method::Put,
            Self::Patch => spot_egress::Method::Patch,
            Self::Delete => spot_egress::Method::Delete,
        }
    }
}

/// How a credential is put on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    /// `Authorization: Bearer <value>`.
    #[default]
    Bearer,
    /// `Authorization: Basic base64(<username>:<value>)`.
    Basic,
    /// The value verbatim in the header named by [`CredentialAuth::header`].
    Header,
}

/// A credential used as request authentication. The node carries the reference only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialAuth {
    /// The credential to open at call time.
    pub credential_ref: CredentialRef,
    /// How the value is put on the request.
    #[serde(default)]
    pub scheme: AuthScheme,
    /// The header for [`AuthScheme::Header`]; ignored otherwise.
    #[serde(default = "default_auth_header")]
    pub header: String,
    /// The user name for [`AuthScheme::Basic`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// The default header a credential is written to.
fn default_auth_header() -> String {
    String::from("Authorization")
}

/// One status pattern: a class (`2xx`, `5xx`) or an exact code (`404`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPattern {
    /// A whole class, `100` to `599`.
    Class(u16),
    /// One exact status.
    Exact(u16),
}

impl StatusPattern {
    /// Parses `2xx` or `404`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        if let Some(prefix) = trimmed.strip_suffix("xx")
            && prefix.len() == 1
            && let Ok(class) = prefix.parse::<u16>()
            && (1..=5).contains(&class)
        {
            return Some(Self::Class(class * 100));
        }
        trimmed.parse::<u16>().ok().map(Self::Exact)
    }

    /// Whether `status` matches.
    #[must_use]
    pub fn matches(self, status: u16) -> bool {
        match self {
            Self::Class(class) => status / 100 == class / 100,
            Self::Exact(exact) => status == exact,
        }
    }

    /// The pattern's spelling.
    #[must_use]
    pub fn as_str(self) -> String {
        match self {
            Self::Class(class) => format!("{}xx", class / 100),
            Self::Exact(exact) => exact.to_string(),
        }
    }
}

impl Serialize for StatusPattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for StatusPattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!("`{text}` is not a status pattern (`2xx` or `404`)"))
        })
    }
}

/// What a response status means to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    /// The call succeeded.
    Success,
    /// The call may succeed on retry.
    Retry,
    /// The call failed for good: the error branch.
    Error,
}

/// The status-to-outcome rules of a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRules {
    /// Statuses that succeed. Default `2xx`.
    #[serde(default = "default_success_statuses")]
    pub success: Vec<StatusPattern>,
    /// Statuses that are retried. Default `408`, `429` and `5xx`.
    #[serde(default = "default_retry_statuses")]
    pub retry: Vec<StatusPattern>,
    /// Statuses that fail. Default `4xx`. A status matching no rule fails.
    #[serde(default = "default_error_statuses")]
    pub error: Vec<StatusPattern>,
}

impl Default for StatusRules {
    fn default() -> Self {
        Self {
            success: default_success_statuses(),
            retry: default_retry_statuses(),
            error: default_error_statuses(),
        }
    }
}

fn default_success_statuses() -> Vec<StatusPattern> {
    vec![StatusPattern::Class(200)]
}

fn default_retry_statuses() -> Vec<StatusPattern> {
    vec![
        StatusPattern::Exact(408),
        StatusPattern::Exact(429),
        StatusPattern::Class(500),
    ]
}

fn default_error_statuses() -> Vec<StatusPattern> {
    vec![StatusPattern::Class(400)]
}

impl StatusRules {
    /// Classifies a response status. A status matching no rule fails, which is the safe default.
    #[must_use]
    pub fn classify(&self, status: u16) -> StatusClass {
        if self.success.iter().any(|p| p.matches(status)) {
            StatusClass::Success
        } else if self.retry.iter().any(|p| p.matches(status)) {
            StatusClass::Retry
        } else {
            StatusClass::Error
        }
    }
}

/// A node's retry policy: how many attempts and how long to wait between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Attempts in total, including the first. `1` means no retry.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// The wait before a retry, in whole seconds.
    #[serde(default = "default_backoff_secs")]
    pub backoff_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff_secs: default_backoff_secs(),
        }
    }
}

fn default_max_attempts() -> u32 {
    3
}

fn default_backoff_secs() -> u64 {
    30
}

/// What the retry policy decided for one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Try again after this many whole seconds.
    Retry {
        /// The wait before the next attempt.
        backoff_secs: u64,
    },
    /// The attempts are exhausted: take the error branch.
    GiveUp,
}

impl RetryPolicy {
    /// Decides what to do after `attempt` (1 for the first try) failed transiently.
    #[must_use]
    pub fn decide(&self, attempt: u32) -> RetryDecision {
        if attempt < self.max_attempts.max(1) {
            RetryDecision::Retry {
                backoff_secs: self.backoff_secs,
            }
        } else {
            RetryDecision::GiveUp
        }
    }
}

/// The default timeout of an HTTP or webhook node, in whole seconds.
fn default_timeout_secs() -> u64 {
    30
}

/// The config of an HTTP node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpNodeConfig {
    /// The method.
    #[serde(default)]
    pub method: HttpMethod,
    /// The URL, possibly with `{doc.<path>}` placeholders.
    pub url: String,
    /// Request headers; each value may carry placeholders.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The credential to put on the request, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<CredentialAuth>,
    /// Bare host names (no scheme, port or path) the credential may go to besides the `url`
    /// template's own literal host, for a node whose host comes from Case data. Exact match
    /// only; ignored when the node has no credential.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    /// The body mapping, serialised as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Mapping>,
    /// The mapping applied to the JSON response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Mapping>,
    /// The status-to-outcome rules.
    #[serde(default)]
    pub status: StatusRules,
    /// The retry policy.
    #[serde(default)]
    pub retry: RetryPolicy,
    /// The request timeout, in whole seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl HttpNodeConfig {
    /// Parses the config.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] when the value is not this shape.
    pub fn from_json(node: &str, config: &Json) -> Result<Self, IntegrationError> {
        parse_config(node, config)
    }

    /// Validates what can be checked before a Case runs: the URL parses and its scheme is
    /// `http`/`https`, and every mapping's expressions parse.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::UrlInvalid`], [`IntegrationError::ConfigInvalid`] or
    /// [`IntegrationError::MappingInvalid`].
    pub fn validate(&self) -> Result<(), IntegrationError> {
        check_url(&self.url)?;
        crate::host::literal_hosts("http", &self.url, &self.allowed_hosts)?;
        if let Some(body) = &self.body {
            body.validate()?;
        }
        if let Some(response) = &self.response {
            response.validate()?;
        }
        Ok(())
    }
}

/// The config of an e-mail node. Every text field may carry `{doc.<path>}` placeholders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailNodeConfig {
    /// The sender address.
    pub from: String,
    /// Primary recipients.
    #[serde(default)]
    pub to: Vec<String>,
    /// Carbon-copy recipients.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<String>,
    /// The subject line.
    pub subject: String,
    /// The plain-text body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The HTML body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// The SMTP credential to open at call time, when the transport needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<CredentialRef>,
    /// The retry policy.
    #[serde(default)]
    pub retry: RetryPolicy,
}

impl MailNodeConfig {
    /// Parses the config.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] when the value is not this shape.
    pub fn from_json(node: &str, config: &Json) -> Result<Self, IntegrationError> {
        parse_config(node, config)
    }

    /// Validates what can be checked before a Case runs: at least one recipient and one body.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] when a recipient or a body is missing.
    pub fn validate(&self) -> Result<(), IntegrationError> {
        if self.to.is_empty() && self.cc.is_empty() {
            return Err(IntegrationError::ConfigInvalid {
                node: String::from("mail"),
                detail: String::from("a mail node needs at least one recipient"),
            });
        }
        if self.text.is_none() && self.html.is_none() {
            return Err(IntegrationError::ConfigInvalid {
                node: String::from("mail"),
                detail: String::from("a mail node needs `text` or `html`"),
            });
        }
        Ok(())
    }
}

/// The config of an outgoing-webhook node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookNodeConfig {
    /// The target URL, possibly with placeholders.
    pub url: String,
    /// The payload mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Mapping>,
    /// The signing credential, opened at call time.
    pub credential_ref: CredentialRef,
    /// Extra headers; the signing and delivery headers are added by the handler.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The retry policy.
    #[serde(default)]
    pub retry: RetryPolicy,
}

impl WebhookNodeConfig {
    /// Parses the config.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] when the value is not this shape.
    pub fn from_json(node: &str, config: &Json) -> Result<Self, IntegrationError> {
        parse_config(node, config)
    }

    /// Validates what can be checked before a Case runs.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::UrlInvalid`] or [`IntegrationError::MappingInvalid`].
    pub fn validate(&self) -> Result<(), IntegrationError> {
        check_url(&self.url)?;
        if let Some(payload) = &self.payload {
            payload.validate()?;
        }
        Ok(())
    }
}

/// The default exported entry point of a code node.
fn default_entry() -> String {
    String::from("main")
}

/// What one TypeScript execution may use. The defaults are the script engine's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeLimits {
    /// The runtime's memory limit, in bytes.
    #[serde(default = "default_memory_bytes")]
    pub memory_bytes: usize,
    /// The native stack the script may use, in bytes.
    #[serde(default = "default_stack_bytes")]
    pub stack_bytes: usize,
    /// The wall-clock deadline, in milliseconds.
    #[serde(default = "default_wall_clock_ms")]
    pub wall_clock_ms: u64,
    /// The instruction budget.
    #[serde(default = "default_instructions")]
    pub instructions: u64,
    /// The most host calls.
    #[serde(default = "default_host_calls")]
    pub host_calls: u32,
    /// The largest serialised result, in bytes.
    #[serde(default = "default_output_bytes")]
    pub output_bytes: usize,
}

impl Default for CodeLimits {
    fn default() -> Self {
        let limits = spot_ts::Limits::default();
        Self {
            memory_bytes: limits.memory_bytes,
            stack_bytes: limits.stack_bytes,
            wall_clock_ms: u64::try_from(limits.wall_clock.as_millis()).unwrap_or(u64::MAX),
            instructions: limits.instructions,
            host_calls: limits.host_calls,
            output_bytes: limits.output_bytes,
        }
    }
}

fn default_memory_bytes() -> usize {
    CodeLimits::default().memory_bytes
}

fn default_stack_bytes() -> usize {
    CodeLimits::default().stack_bytes
}

fn default_wall_clock_ms() -> u64 {
    CodeLimits::default().wall_clock_ms
}

fn default_instructions() -> u64 {
    CodeLimits::default().instructions
}

fn default_host_calls() -> u32 {
    CodeLimits::default().host_calls
}

fn default_output_bytes() -> usize {
    CodeLimits::default().output_bytes
}

impl CodeLimits {
    /// The limits as the script engine takes them.
    #[must_use]
    pub fn to_limits(&self) -> spot_ts::Limits {
        spot_ts::Limits {
            memory_bytes: self.memory_bytes,
            stack_bytes: self.stack_bytes,
            wall_clock: std::time::Duration::from_millis(self.wall_clock_ms),
            instructions: self.instructions,
            host_calls: self.host_calls,
            output_bytes: self.output_bytes,
        }
    }
}

/// The config of a TypeScript code node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeNodeConfig {
    /// The script source (an ES module with exported entry points).
    pub script: String,
    /// The exported function to call.
    #[serde(default = "default_entry")]
    pub entry: String,
    /// The execution limits.
    #[serde(default)]
    pub limits: CodeLimits,
    /// The mapping that builds the script's argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Mapping>,
    /// The mapping applied to the script's result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Mapping>,
}

impl CodeNodeConfig {
    /// Parses the config.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] when the value is not this shape.
    pub fn from_json(node: &str, config: &Json) -> Result<Self, IntegrationError> {
        parse_config(node, config)
    }

    /// Validates what can be checked before a Case runs.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] for an empty script or entry, or
    /// [`IntegrationError::MappingInvalid`] for a bad mapping.
    pub fn validate(&self) -> Result<(), IntegrationError> {
        if self.script.trim().is_empty() {
            return Err(IntegrationError::ConfigInvalid {
                node: String::from("code"),
                detail: String::from("a code node needs a `script`"),
            });
        }
        if self.entry.trim().is_empty() {
            return Err(IntegrationError::ConfigInvalid {
                node: String::from("code"),
                detail: String::from("a code node needs a non-empty `entry`"),
            });
        }
        if let Some(input) = &self.input {
            input.validate()?;
        }
        if let Some(output) = &self.output {
            output.validate()?;
        }
        Ok(())
    }
}

/// The config of a WASM connector node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorNodeConfig {
    /// The connector's name.
    pub connector: String,
    /// The operation to invoke.
    pub operation: String,
    /// The mapping that builds the operation's input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Mapping>,
    /// The credential the connector needs, resolved by the host at call time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<CredentialRef>,
    /// The mapping applied to the connector's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Mapping>,
    /// The retry policy for a connector's own error.
    #[serde(default)]
    pub retry: RetryPolicy,
}

impl ConnectorNodeConfig {
    /// Parses the config.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] when the value is not this shape.
    pub fn from_json(node: &str, config: &Json) -> Result<Self, IntegrationError> {
        parse_config(node, config)
    }

    /// Validates what can be checked before a Case runs.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] for an empty connector or operation, or
    /// [`IntegrationError::MappingInvalid`] for a bad mapping.
    pub fn validate(&self) -> Result<(), IntegrationError> {
        if self.connector.trim().is_empty() || self.operation.trim().is_empty() {
            return Err(IntegrationError::ConfigInvalid {
                node: String::from("connector"),
                detail: String::from("a connector node needs a `connector` and an `operation`"),
            });
        }
        if let Some(input) = &self.input {
            input.validate()?;
        }
        if let Some(output) = &self.output {
            output.validate()?;
        }
        Ok(())
    }
}

/// Parses a config value into a typed config.
fn parse_config<T: serde::de::DeserializeOwned>(
    node: &str,
    config: &Json,
) -> Result<T, IntegrationError> {
    serde_json::from_value(config.clone()).map_err(|e| IntegrationError::ConfigInvalid {
        node: node.to_owned(),
        detail: e.to_string(),
    })
}

/// Checks that a URL is absolute and `http`/`https`.
fn check_url(url: &str) -> Result<(), IntegrationError> {
    let parsed = url::Url::parse(url).map_err(|e| IntegrationError::UrlInvalid {
        detail: e.to_string(),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(IntegrationError::UrlInvalid {
            detail: String::from("the scheme must be http or https"),
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn http_config_parses_with_defaults() {
        let config = HttpNodeConfig::from_json(
            "n1",
            &json!({ "url": "https://api.test/orders/{doc.input.id}" }),
        )
        .unwrap();
        assert_eq!(config.method, HttpMethod::Get);
        assert_eq!(config.timeout_secs, 30);
        assert_eq!(config.status.classify(200), StatusClass::Success);
        assert_eq!(config.status.classify(429), StatusClass::Retry);
        assert_eq!(config.status.classify(503), StatusClass::Retry);
        assert_eq!(config.status.classify(404), StatusClass::Error);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn status_patterns_parse_and_match() {
        assert!(StatusPattern::parse("2xx").unwrap().matches(204));
        assert!(!StatusPattern::parse("2xx").unwrap().matches(304));
        assert!(StatusPattern::parse("404").unwrap().matches(404));
        assert!(!StatusPattern::parse("404").unwrap().matches(405));
        assert!(StatusPattern::parse("nonsense").is_none());
        assert!(StatusPattern::parse("6xx").is_none());
    }

    #[test]
    fn retry_policy_exhausts_at_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 2,
            backoff_secs: 5,
        };
        assert_eq!(policy.decide(1), RetryDecision::Retry { backoff_secs: 5 });
        assert_eq!(policy.decide(2), RetryDecision::GiveUp);
        assert_eq!(policy.decide(3), RetryDecision::GiveUp);
    }

    #[test]
    fn a_bad_url_is_refused() {
        let config = HttpNodeConfig::from_json("n1", &json!({ "url": "ftp://api.test" })).unwrap();
        assert_eq!(
            config.validate().unwrap_err().code(),
            "bpm_integration.url_invalid"
        );
        let config = HttpNodeConfig::from_json("n1", &json!({ "url": "not a url" })).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_unknown_config_field_is_refused() {
        let error = HttpNodeConfig::from_json("n1", &json!({ "url": "https://a.test", "wat": 1 }))
            .unwrap_err();
        assert_eq!(error.code(), "bpm_integration.config_invalid");
    }

    #[test]
    fn code_limits_round_trip_to_the_engine() {
        let limits = CodeLimits {
            memory_bytes: 1024,
            wall_clock_ms: 250,
            ..CodeLimits::default()
        };
        let engine = limits.to_limits();
        assert_eq!(engine.memory_bytes, 1024);
        assert_eq!(engine.wall_clock.as_millis(), 250);
    }

    #[test]
    fn mail_and_connector_configs_validate() {
        let mail = MailNodeConfig::from_json(
            "n1",
            &json!({ "from": "a@b.test", "to": ["c@d.test"], "subject": "hi", "text": "x" }),
        )
        .unwrap();
        assert!(mail.validate().is_ok());
        let missing_body = MailNodeConfig::from_json(
            "n1",
            &json!({ "from": "a@b.test", "to": ["c@d.test"], "subject": "hi" }),
        )
        .unwrap();
        assert!(missing_body.validate().is_err());

        let connector = ConnectorNodeConfig::from_json(
            "n1",
            &json!({ "connector": "as2", "operation": "send" }),
        )
        .unwrap();
        assert!(connector.validate().is_ok());
        let empty =
            ConnectorNodeConfig::from_json("n1", &json!({ "connector": "", "operation": "" }))
                .unwrap();
        assert!(empty.validate().is_err());
    }
}
