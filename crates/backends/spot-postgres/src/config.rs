//! Connection settings.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use spot_storage_contract::{ErrorKind, StorageError, StorageResult};
use tokio_postgres::config::{Host, SslMode};

/// Whether the connection to the server is encrypted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tls {
    /// Plain TCP. For local development and loopback test servers only.
    Disable,
    /// TLS with the platform's root certificates; the server certificate is verified.
    Require,
}

/// Where and how to connect to one tenant's PostgreSQL database.
#[derive(Clone, PartialEq, Eq)]
pub struct PgConfig {
    /// Server host name or address.
    pub host: String,
    /// Server TCP port.
    pub port: u16,
    /// Database name.
    pub dbname: String,
    /// Login role.
    pub user: String,
    /// Password; redacted in `Debug` output.
    pub password: String,
    /// Reported to the server (`pg_stat_activity.application_name`); transactions append their label.
    pub application_name: String,
    /// Most connections the pool opens.
    pub pool_max_size: usize,
    /// Longest time to open one connection.
    pub connect_timeout: Duration,
    /// Longest time `begin` waits for a free connection before failing with `Unavailable`.
    pub pool_wait_timeout: Duration,
    /// Encryption of the connection.
    pub tls: Tls,
}

impl PgConfig {
    /// A config for `dbname` on `host:port` with the documented defaults: 16 pooled connections,
    /// 5 s connect timeout, 5 s pool wait, no TLS.
    pub fn new(host: &str, port: u16, dbname: &str, user: &str, password: &str) -> Self {
        Self {
            host: host.to_owned(),
            port,
            dbname: dbname.to_owned(),
            user: user.to_owned(),
            password: password.to_owned(),
            application_name: "spot".to_owned(),
            pool_max_size: 16,
            connect_timeout: Duration::from_secs(5),
            pool_wait_timeout: Duration::from_secs(5),
            tls: Tls::Disable,
        }
    }

    /// Parses a `postgres://user:password@host:port/dbname` URL.
    ///
    /// Understood parameters: `application_name`, `connect_timeout` (seconds) and `sslmode`.
    /// Only `sslmode=require` switches [`Tls::Require`] on; every other value, and no value, means
    /// [`Tls::Disable`]. Unix-socket hosts and several hosts are refused.
    pub fn from_url(url: &str) -> StorageResult<Self> {
        let parsed = tokio_postgres::Config::from_str(url).map_err(|_| {
            StorageError::new(
                ErrorKind::InvalidValue,
                "the connection URL is not a valid postgres:// URL",
            )
        })?;
        let host = match parsed.get_hosts() {
            [Host::Tcp(h)] if !h.starts_with('/') => h.clone(),
            _ => {
                return Err(StorageError::new(
                    ErrorKind::InvalidValue,
                    "the connection URL must name exactly one TCP host",
                ));
            }
        };
        let port = parsed.get_ports().first().copied().unwrap_or(5432);
        let mut config = Self::new(
            &host,
            port,
            parsed.get_dbname().unwrap_or("postgres"),
            parsed.get_user().unwrap_or("postgres"),
            &parsed
                .get_password()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default(),
        );
        if let Some(name) = parsed.get_application_name() {
            config.application_name = name.to_owned();
        }
        if let Some(t) = parsed.get_connect_timeout() {
            config.connect_timeout = *t;
        }
        if url_has_param(url, "sslmode") && parsed.get_ssl_mode() == SslMode::Require {
            config.tls = Tls::Require;
        }
        Ok(config)
    }

    /// The same config pointing at another database on the same server.
    pub fn with_dbname(mut self, dbname: &str) -> Self {
        dbname.clone_into(&mut self.dbname);
        self
    }

    /// Checks the pool settings.
    pub(crate) fn validate(&self) -> StorageResult<()> {
        if self.pool_max_size == 0 {
            return Err(StorageError::new(
                ErrorKind::InvalidValue,
                "pool_max_size must be at least 1",
            ));
        }
        Ok(())
    }

    /// The driver's view of this config.
    pub(crate) fn driver_config(&self) -> tokio_postgres::Config {
        let mut c = tokio_postgres::Config::new();
        c.host(&self.host)
            .port(self.port)
            .dbname(&self.dbname)
            .user(&self.user)
            .password(&self.password)
            .application_name(&self.application_name)
            .connect_timeout(self.connect_timeout);
        c
    }
}

/// Whether the URL's query string sets `name`; `tokio-postgres` reports `Prefer` for "absent".
fn url_has_param(url: &str, name: &str) -> bool {
    url.split_once('?').is_some_and(|(_, query)| {
        query
            .split('&')
            .any(|kv| kv.split_once('=').map_or(kv, |(k, _)| k) == name)
    })
}

impl fmt::Debug for PgConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("dbname", &self.dbname)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("application_name", &self.application_name)
            .field("pool_max_size", &self.pool_max_size)
            .field("connect_timeout", &self.connect_timeout)
            .field("pool_wait_timeout", &self.pool_wait_timeout)
            .field("tls", &self.tls)
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_the_password() {
        let c = PgConfig::new("h", 1, "d", "u", "hunter2");
        let text = format!("{c:?}");
        assert!(!text.contains("hunter2"));
        assert!(text.contains("<redacted>"));
    }

    #[test]
    fn parses_a_url() {
        let c = PgConfig::from_url(
            "postgres://spot:p%40ss@db.example:6543/ledger?application_name=api&connect_timeout=3&sslmode=require",
        )
        .unwrap();
        assert_eq!(c.host, "db.example");
        assert_eq!(c.port, 6543);
        assert_eq!(c.dbname, "ledger");
        assert_eq!(c.user, "spot");
        assert_eq!(c.password, "p@ss");
        assert_eq!(c.application_name, "api");
        assert_eq!(c.connect_timeout, Duration::from_secs(3));
        assert_eq!(c.tls, Tls::Require);
        assert_eq!(c.pool_max_size, 16);
    }

    #[test]
    fn url_defaults_and_plain_tls() {
        let c = PgConfig::from_url("postgres://u:p@localhost/db").unwrap();
        assert_eq!(c.port, 5432);
        assert_eq!(c.tls, Tls::Disable);
        assert_eq!(c.pool_wait_timeout, Duration::from_secs(5));
    }

    #[test]
    fn refuses_bad_urls() {
        assert!(PgConfig::from_url("not a url").is_err());
        assert!(PgConfig::from_url("postgres://u@/db?host=/var/run/postgresql").is_err());
        assert!(PgConfig::from_url("postgres://u@a,b/db").is_err());
    }
}
