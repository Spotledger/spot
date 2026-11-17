//! The control plane's configuration. Unknown keys refuse the start (`deny_unknown_fields`
//! everywhere), and `validate` rejects values that would leave a listener half configured.

use std::path::PathBuf;

use serde::Deserialize;

/// A configuration problem.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("the configuration file could not be read")]
    Read,
    /// The text is not valid TOML of the documented shape (unknown keys included).
    #[error("the configuration is invalid: {0}")]
    Parse(String),
    /// A value is out of range or inconsistent.
    #[error("the configuration is invalid: {0}")]
    Invalid(&'static str),
}

/// The PostgreSQL control database (never a tenant database).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Server host.
    pub host: String,
    /// Server port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Database name.
    pub dbname: String,
    /// Login role.
    pub user: String,
    /// The name of the environment variable that holds the password (never the password itself).
    pub password_env: String,
    /// Require TLS to the database (default true).
    #[serde(default = "default_true")]
    pub tls: bool,
    /// Pool size.
    #[serde(default = "default_pool")]
    pub pool_max_size: usize,
}

fn default_port() -> u16 {
    5432
}
fn default_true() -> bool {
    true
}
fn default_pool() -> usize {
    16
}
fn default_validity() -> u32 {
    30
}

/// One mutual-TLS listener.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    /// The address to listen on, for example `127.0.0.1:8443`.
    pub listen: String,
    /// The server certificate chain (PEM).
    pub server_cert: PathBuf,
    /// The server key (PEM).
    pub server_key: PathBuf,
    /// The CA whose client certificates this listener accepts (PEM). For the host listener this
    /// is the control plane's own host CA certificate.
    pub client_ca: PathBuf,
}

/// An operator on the allow-list.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorConfig {
    /// The certificate subject (RFC 2253 text), for example `CN=alice`.
    pub subject: String,
    /// The principal name recorded in the audit.
    pub principal: String,
    /// The tenants this operator may touch: `["*"]` or a list of tenant ids.
    pub tenants: Vec<String>,
}

/// The host certificate authority.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCaConfig {
    /// The CA certificate (PEM); the host listener's `client_ca` normally points at the same file.
    pub cert: PathBuf,
    /// The CA key (PEM, file mode 0600).
    pub key: PathBuf,
    /// The CA certificate's common name (the issuer name of issued certificates).
    pub common_name: String,
    /// Validity of issued certificates in days (default 30).
    #[serde(default = "default_validity")]
    pub validity_days: u32,
    /// A file of revoked certificate fingerprints, one SHA-256 hex per line (optional).
    pub revocation_list: Option<PathBuf>,
}

/// What `main` wires into the modules (`WP-CTL-01.T9`): the names of the environment variables
/// that hold secrets, never the secrets themselves.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WiringConfig {
    /// The environment variable that holds the seed of the **deployment signing key** (64 hex
    /// characters). The `deploy` module signs each deployment step with it; agents trust it for
    /// `deploy.*` only. It must be a key of its own: never an administrator key and never the
    /// licence key. Without the entry the `deploy` module is not wired and reports not ready; with
    /// the entry and no usable key the start is refused.
    pub deployment_key_env: Option<String>,
}

/// The whole configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The control database.
    pub database: DatabaseConfig,
    /// The region this control plane serves.
    pub region: String,
    /// The audience of signed operator commands.
    pub audience: String,
    /// The operator listener.
    pub operator_listener: ListenerConfig,
    /// The tenant host and agent listener.
    pub host_listener: ListenerConfig,
    /// The host CA.
    pub host_ca: HostCaConfig,
    /// The operator allow-list with scopes.
    pub operators: Vec<OperatorConfig>,
    /// Administrator public keys (Ed25519, 64 hex characters each).
    pub administrator_keys: Vec<String>,
    /// What `main` wires into the modules (optional section `[wiring]`).
    #[serde(default)]
    pub wiring: WiringConfig,
}

impl Config {
    /// Parses and validates configuration text.
    ///
    /// # Errors
    /// [`ConfigError`] for unknown keys, bad TOML or inconsistent values.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: Self =
            toml::from_str(text).map_err(|error| ConfigError::Parse(error.message().to_owned()))?;
        config.validate()?;
        Ok(config)
    }

    /// Reads, parses and validates a file.
    ///
    /// # Errors
    /// [`ConfigError`].
    pub fn from_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|_| ConfigError::Read)?;
        Self::from_toml(&text)
    }

    /// Checks cross-field rules.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] naming the rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.region.is_empty() || self.audience.is_empty() {
            return Err(ConfigError::Invalid(
                "region and audience must not be empty",
            ));
        }
        if self.operators.is_empty() {
            return Err(ConfigError::Invalid("at least one operator is required"));
        }
        if self.administrator_keys.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one administrator key is required",
            ));
        }
        if self
            .administrator_keys
            .iter()
            .any(|key| decode_key(key).is_none())
        {
            return Err(ConfigError::Invalid(
                "an administrator key is not 64 hex characters",
            ));
        }
        if self
            .operators
            .iter()
            .any(|operator| operator.tenants.is_empty())
        {
            return Err(ConfigError::Invalid(
                "an operator has an empty tenant scope",
            ));
        }
        if self.operator_listener.listen == self.host_listener.listen {
            return Err(ConfigError::Invalid(
                "the two listeners need different addresses",
            ));
        }
        if self.operator_listener.client_ca == self.host_listener.client_ca {
            return Err(ConfigError::Invalid(
                "the operator and host listeners must trust different CAs",
            ));
        }
        if self.host_ca.validity_days == 0 || self.host_ca.validity_days > 400 {
            return Err(ConfigError::Invalid(
                "host_ca.validity_days must be 1 to 400",
            ));
        }
        if self.database.pool_max_size == 0 {
            return Err(ConfigError::Invalid(
                "database.pool_max_size must be positive",
            ));
        }
        if self
            .wiring
            .deployment_key_env
            .as_deref()
            .is_some_and(|name| name.is_empty() || name.contains(['=', '\0']))
        {
            return Err(ConfigError::Invalid(
                "wiring.deployment_key_env must name an environment variable",
            ));
        }
        Ok(())
    }
}

/// Decodes a 64-hex-character Ed25519 public key.
#[must_use]
pub fn decode_key(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, pair) in bytes.chunks(2).enumerate() {
        let high = char::from(*pair.first()?).to_digit(16)?;
        let low = char::from(*pair.get(1)?).to_digit(16)?;
        *out.get_mut(index)? = u8::try_from(high * 16 + low).ok()?;
    }
    Some(out)
}
