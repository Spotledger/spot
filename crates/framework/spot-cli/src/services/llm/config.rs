//! The per-tenant provider configuration: what a tenant brought (provider kind, endpoint, model),
//! what it may reach (the allowed hosts), what it costs (prices, a budget) and the checks that
//! refuse a bad one before it is stored.
//!
//! The key is **not** part of this record. It is written once through
//! [`LlmGateway::set_key`](super::gateway::LlmGateway::set_key), sealed by the secrets vault and
//! kept in its own column; nothing here, in an error or in `Debug` output can name it.

use serde::{Deserialize, Serialize};
use url::Url;

/// The longest tenant id: it becomes the prefix of a request id and a table key.
pub const MAX_TENANT_LEN: usize = 64;

/// The provider family a tenant's endpoint speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The Anthropic Messages API (bearer key in `x-api-key`).
    Anthropic,
    /// OpenRouter (OpenAI-compatible, bearer key).
    Openrouter,
    /// Any other OpenAI-compatible endpoint with a bearer key (OpenAI, vLLM, llama.cpp server).
    Openai,
    /// A local Ollama server: no key, and loopback or private addresses only when the operator's
    /// egress policy explicitly allows that host.
    Ollama,
}

impl Kind {
    /// The stable id the router and the price table use for the provider.
    #[must_use]
    pub fn provider_id(self) -> &'static str {
        match self {
            Kind::Anthropic => "anthropic",
            Kind::Openrouter => "openrouter",
            Kind::Openai => "openai",
            Kind::Ollama => "ollama",
        }
    }

    /// Whether calls need a stored key.
    #[must_use]
    pub fn needs_key(self) -> bool {
        !matches!(self, Kind::Ollama)
    }
}

/// How often a budget starts again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    /// A UTC day.
    Day,
    /// A UTC calendar month.
    Month,
}

/// A tenant's spending cap, in micro-units (millionths of a main currency unit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// The cap for one period. A call whose worst case would pass it is refused before sending.
    pub limit_micro: u64,
    /// When the spend starts again from zero.
    pub period: Period,
    /// The percent of the cap a crossing of which is logged as a warning; `0` for none.
    #[serde(default)]
    pub warn_at_percent: u64,
}

/// The price of the tenant's model: micro-units per million tokens, integers only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceConfig {
    /// Per million input tokens (cache reads and writes included).
    pub input_micro_per_million: u64,
    /// Per million output tokens.
    pub output_micro_per_million: u64,
}

fn default_context_window() -> u64 {
    128_000
}

fn default_max_output() -> u64 {
    8_192
}

fn default_call_output() -> u32 {
    1_024
}

fn default_tasks() -> Vec<String> {
    vec!["chat".to_owned()]
}

/// One tenant's provider: everything but the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    /// The tenant id (`[a-z0-9_-]`, 1..=64 characters, no `:`).
    pub tenant: String,
    /// The provider family.
    pub kind: Kind,
    /// The endpoint's base URL (`https://host[:port]`, an optional path prefix). No credentials,
    /// query or fragment.
    pub endpoint: String,
    /// The model id as the endpoint spells it.
    pub model: String,
    /// The hosts the tenant allows its model calls to reach: exact names or `*.suffix`. An
    /// endpoint outside the list is refused before anything is sent. The operator's egress policy
    /// (purpose `llm`) is checked as well, after DNS resolution; both must allow.
    pub allowed_hosts: Vec<String>,
    /// What the model costs. Required when a budget is set.
    #[serde(default)]
    pub price: PriceConfig,
    /// The model's context window in tokens.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// The model's largest generation in tokens.
    #[serde(default = "default_max_output")]
    pub max_output: u64,
    /// The output cap a call without its own gets, so a budget estimate is never a single token.
    #[serde(default = "default_call_output")]
    pub default_max_output_tokens: u32,
    /// The tasks this tenant may run (`summarise`, `chat`): a call names one of them and the
    /// budget and the usage are counted per task as well as per tenant.
    #[serde(default = "default_tasks")]
    pub tasks: Vec<String>,
    /// The spending cap; `None` is no cap (usage is still counted).
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
}

/// Why a configuration or a name is refused. The text is fixed and never quotes a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigProblem(pub &'static str);

impl std::fmt::Display for ConfigProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ConfigProblem {}

/// Checks a tenant id.
///
/// # Errors
///
/// [`ConfigProblem`] when it is empty, too long or has a character outside `[a-z0-9_-]`.
pub fn check_tenant(tenant: &str) -> Result<(), ConfigProblem> {
    if tenant.is_empty() || tenant.len() > MAX_TENANT_LEN {
        return Err(ConfigProblem("the tenant id must be 1 to 64 characters"));
    }
    if !tenant
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(ConfigProblem(
            "the tenant id may only hold a-z, 0-9, underscore and hyphen",
        ));
    }
    Ok(())
}

/// Checks a task name (it is the second part of a request id: no `:`, no whitespace).
///
/// # Errors
///
/// [`ConfigProblem`] when it is empty, too long or has a character outside `[a-z0-9_-]`.
pub fn check_task(task: &str) -> Result<(), ConfigProblem> {
    if task.is_empty() || task.len() > MAX_TENANT_LEN {
        return Err(ConfigProblem("the task name must be 1 to 64 characters"));
    }
    if !task
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(ConfigProblem(
            "the task name may only hold a-z, 0-9, underscore and hyphen",
        ));
    }
    Ok(())
}

/// Whether `host` is covered by one allow-list entry: an exact name or `*.suffix` (the suffix
/// itself is not covered by the wildcard). Case-insensitive.
#[must_use]
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let host = host.trim().to_ascii_lowercase();
    if pattern.is_empty() || host.is_empty() {
        return false;
    }
    match pattern.strip_prefix("*.") {
        Some(suffix) => {
            !suffix.is_empty()
                && host.len() > suffix.len() + 1
                && host.ends_with(suffix)
                && host.as_bytes().get(host.len() - suffix.len() - 1) == Some(&b'.')
        }
        None => pattern == host,
    }
}

/// The parsed endpoint of a configuration.
///
/// # Errors
///
/// [`ConfigProblem`] when the URL does not parse, is not http or https, has no host, carries
/// credentials, a query or a fragment (a key must never ride in a URL).
pub fn parse_endpoint(endpoint: &str) -> Result<Url, ConfigProblem> {
    let url = Url::parse(endpoint).map_err(|_| ConfigProblem("the endpoint is not a valid URL"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(ConfigProblem("the endpoint must be an http or https URL"));
    }
    if url.host_str().is_none() {
        return Err(ConfigProblem("the endpoint names no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigProblem("the endpoint must not carry credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigProblem(
            "the endpoint must not carry a query or a fragment",
        ));
    }
    Ok(url)
}

/// The `scheme://host[:port]` of an endpoint: what the send record names. Never the path or a
/// query.
#[must_use]
pub fn origin_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

impl TenantConfig {
    /// Checks everything that can be checked without the network.
    ///
    /// # Errors
    ///
    /// The first [`ConfigProblem`]: a bad tenant id, an endpoint that is not acceptable, an empty
    /// model, an empty allow-list or an endpoint host the list does not cover, a budget without a
    /// price or with a zero limit, or limits that do not fit.
    pub fn validate(&self) -> Result<(), ConfigProblem> {
        check_tenant(&self.tenant)?;
        let url = parse_endpoint(&self.endpoint)?;
        if self.model.trim().is_empty() || self.model.len() > 200 {
            return Err(ConfigProblem("the model must be 1 to 200 characters"));
        }
        if self.allowed_hosts.is_empty() {
            return Err(ConfigProblem(
                "the allow-list is empty: name the hosts this tenant's model calls may reach",
            ));
        }
        if self.allowed_hosts.len() > 32
            || self
                .allowed_hosts
                .iter()
                .any(|h| h.trim().is_empty() || h.len() > 253)
        {
            return Err(ConfigProblem(
                "the allow-list has an empty or oversized entry or more than 32 entries",
            ));
        }
        let host = url.host_str().unwrap_or_default();
        if !self.allowed_hosts.iter().any(|p| host_matches(p, host)) {
            return Err(ConfigProblem(
                "the endpoint host is not on the tenant's allow-list",
            ));
        }
        if self.context_window == 0 || self.max_output == 0 {
            return Err(ConfigProblem(
                "the context window and the maximum output must be above zero",
            ));
        }
        if self.default_max_output_tokens == 0 {
            return Err(ConfigProblem("the default output cap must be above zero"));
        }
        if self.tasks.is_empty() || self.tasks.len() > 32 {
            return Err(ConfigProblem("a tenant needs 1 to 32 tasks"));
        }
        for task in &self.tasks {
            check_task(task)?;
        }
        if let Some(budget) = &self.budget {
            if budget.limit_micro == 0 {
                return Err(ConfigProblem("the budget limit must be above zero"));
            }
            if budget.warn_at_percent > 100 {
                return Err(ConfigProblem("the warning percent must be 0 to 100"));
            }
            if self.price.input_micro_per_million == 0 && self.price.output_micro_per_million == 0 {
                return Err(ConfigProblem(
                    "a budget needs a price: an unpriced model would never stop",
                ));
            }
        }
        Ok(())
    }

    /// The parsed endpoint (validated).
    ///
    /// # Errors
    ///
    /// See [`parse_endpoint`].
    pub fn endpoint_url(&self) -> Result<Url, ConfigProblem> {
        parse_endpoint(&self.endpoint)
    }

    /// Whether the endpoint's host is on the tenant's own allow-list.
    #[must_use]
    pub fn endpoint_allowed(&self) -> bool {
        match self.endpoint_url() {
            Ok(url) => {
                let host = url.host_str().unwrap_or_default();
                self.allowed_hosts.iter().any(|p| host_matches(p, host))
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn good() -> TenantConfig {
        TenantConfig {
            tenant: "acme".into(),
            kind: Kind::Anthropic,
            endpoint: "https://api.anthropic.test".into(),
            model: "model-a".into(),
            allowed_hosts: vec!["api.anthropic.test".into()],
            price: PriceConfig {
                input_micro_per_million: 3_000_000,
                output_micro_per_million: 15_000_000,
            },
            context_window: 100_000,
            max_output: 4_096,
            default_max_output_tokens: 512,
            tasks: vec!["chat".into()],
            budget: None,
        }
    }

    #[test]
    fn a_good_configuration_validates() {
        assert!(good().validate().is_ok());
    }

    #[test]
    fn tenant_ids_and_tasks_are_closed_sets() {
        assert!(check_tenant("acme-1_b").is_ok());
        assert!(check_tenant("").is_err());
        assert!(check_tenant("a:b").is_err());
        assert!(check_tenant("Acme").is_err());
        assert!(check_tenant(&"a".repeat(65)).is_err());
        assert!(check_task("summarise").is_ok());
        assert!(check_task("a b").is_err());
    }

    #[test]
    fn an_endpoint_with_credentials_query_or_odd_scheme_is_refused() {
        for bad in [
            "https://user:pw@api.test",
            "https://api.test/?key=1",
            "https://api.test/#frag",
            "ftp://api.test",
            "not a url",
        ] {
            let mut cfg = good();
            cfg.endpoint = bad.into();
            assert!(cfg.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn the_allow_list_covers_exact_names_and_wildcards_only() {
        assert!(host_matches("api.x.test", "API.X.TEST"));
        assert!(host_matches("*.x.test", "api.x.test"));
        assert!(!host_matches("*.x.test", "x.test"));
        assert!(!host_matches("*.x.test", "evilx.test"));
        assert!(!host_matches("*.x.test", "api.evilx.test"));
        assert!(!host_matches("", "x.test"));
        let mut cfg = good();
        cfg.allowed_hosts = vec!["other.test".into()];
        assert!(cfg.validate().is_err());
        assert!(!cfg.endpoint_allowed());
    }

    #[test]
    fn a_budget_needs_a_price_and_a_limit() {
        let mut cfg = good();
        cfg.budget = Some(BudgetConfig {
            limit_micro: 100,
            period: Period::Day,
            warn_at_percent: 80,
        });
        assert!(cfg.validate().is_ok());
        cfg.price = PriceConfig::default();
        assert!(cfg.validate().is_err());
        cfg.price.input_micro_per_million = 1;
        cfg.budget = Some(BudgetConfig {
            limit_micro: 0,
            period: Period::Day,
            warn_at_percent: 0,
        });
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn the_origin_leaves_out_the_path() {
        let url = Url::parse("https://models.test:8443/v1/prefix").unwrap();
        assert_eq!(origin_of(&url), "https://models.test:8443");
        let url = Url::parse("https://models.test/v1").unwrap();
        assert_eq!(origin_of(&url), "https://models.test");
    }

    #[test]
    fn unknown_keys_are_refused_when_reading() {
        let text = r#"{"tenant":"a","kind":"ollama","endpoint":"http://h","model":"m","allowed_hosts":["h"],"extra":1}"#;
        assert!(serde_json::from_str::<TenantConfig>(text).is_err());
    }
}
