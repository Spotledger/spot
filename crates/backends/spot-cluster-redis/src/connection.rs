//! The connection to a Redis/Valkey server: one reconnecting manager, shared by every component.
//!
//! [`RedisConnection`] owns a [`redis::aio::ConnectionManager`] (from the `connection-manager`
//! feature), which clones cheaply, multiplexes commands and **reconnects with exponential
//! backoff** on its own once a connection drops. A `rediss://` URL is served over TLS by the
//! `tokio-rustls-comp` feature; a `redis://` URL is plain. Nothing here panics: a failure to
//! reach the server is a [`RedisError`] with the `cluster_redis.unavailable` code.
//!
//! The initial connect is retried a few times with a growing delay, because a plugin that starts
//! beside a server that is still coming up should wait rather than fail the process.

use std::time::Duration;

use redis::Client;
use redis::aio::ConnectionManager;

use crate::error::{RedisError, RedisResult};
use crate::keys::KeySpace;

impl std::fmt::Debug for RedisConfig {
    /// The URL may carry a password (`redis://:secret@host`), so it is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConfig")
            .field("url", &"[redacted]")
            .field("keyspace", &self.keyspace)
            .field("connect_attempts", &self.connect_attempts)
            .field("connect_backoff", &self.connect_backoff)
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

/// How to reach the server and how the plugin names its keys.
#[derive(Clone)]
pub struct RedisConfig {
    /// The server URL: `redis://host:port` or `rediss://host:port` for TLS.
    pub url: String,
    /// The key prefix shared by every component.
    pub keyspace: KeySpace,
    /// How many times the initial connect is tried before giving up.
    pub connect_attempts: u32,
    /// The first retry delay; each later retry doubles it (bounded by [`MAX_BACKOFF`]).
    pub connect_backoff: Duration,
    /// The longest the whole initial connect (every attempt and every retry delay) may take. The
    /// driver retries inside one attempt, so without this bound an unreachable server takes about
    /// 48 s to refuse; with it the start fails in this time.
    pub connect_timeout: Duration,
}

/// The default [`RedisConfig::connect_timeout`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest a retry waits.
pub const MAX_BACKOFF: Duration = Duration::from_secs(2);

impl RedisConfig {
    /// A config for `url`, with the default key space and retry policy.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            keyspace: KeySpace::default(),
            connect_attempts: 5,
            connect_backoff: Duration::from_millis(50),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// The same config under a different key prefix (a second logical cluster on one server).
    #[must_use]
    pub fn with_keyspace(mut self, keyspace: KeySpace) -> Self {
        self.keyspace = keyspace;
        self
    }

    /// The same config with a different initial-connect retry policy.
    #[must_use]
    pub fn with_connect_attempts(mut self, attempts: u32, backoff: Duration) -> Self {
        self.connect_attempts = attempts.max(1);
        self.connect_backoff = backoff;
        self
    }

    /// The same config with a different bound on the total initial-connect time (at least 1 ms).
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    fn client(&self) -> RedisResult<Client> {
        Client::open(self.url.as_str())
            .map_err(|error| RedisError::unavailable("opening the Redis URL", error))
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let mut delay = self.connect_backoff;
        for _ in 1..attempt {
            delay = delay.saturating_mul(2).min(MAX_BACKOFF);
        }
        delay
    }
}

/// A live connection to the server, shared by the cache, the coordinator, the bus and presence.
#[derive(Clone)]
pub struct RedisConnection {
    manager: ConnectionManager,
    client: Client,
    keyspace: KeySpace,
}

impl std::fmt::Debug for RedisConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConnection")
            .field("keyspace", &self.keyspace)
            .finish_non_exhaustive()
    }
}

impl RedisConnection {
    /// Connects, retrying the initial attempt with backoff. The returned manager reconnects on
    /// its own after that.
    ///
    /// # Errors
    ///
    /// `cluster_redis.unavailable` when every attempt failed or
    /// [`RedisConfig::connect_timeout`] passed first.
    pub async fn connect(config: &RedisConfig) -> RedisResult<Self> {
        let client = config.client()?;
        tokio::time::timeout(
            config.connect_timeout,
            Self::connect_attempts(config, client),
        )
        .await
        .unwrap_or_else(|_| {
            Err(RedisError::unavailable(
                "connecting to Redis",
                format!("no connection within {:?}", config.connect_timeout),
            ))
        })
    }

    async fn connect_attempts(config: &RedisConfig, client: Client) -> RedisResult<Self> {
        let mut last: Option<redis::RedisError> = None;
        for attempt in 1..=config.connect_attempts {
            match ConnectionManager::new(client.clone()).await {
                Ok(manager) => {
                    return Ok(Self {
                        manager,
                        client,
                        keyspace: config.keyspace.clone(),
                    });
                }
                Err(error) => {
                    last = Some(error);
                    if attempt < config.connect_attempts {
                        tokio::time::sleep(config.backoff(attempt)).await;
                    }
                }
            }
        }
        Err(RedisError::unavailable(
            "connecting to Redis",
            last.map_or_else(
                || "no attempt was made".to_owned(),
                |error| error.to_string(),
            ),
        ))
    }

    /// The reconnecting command manager. Clone it freely; clones share one connection.
    #[must_use]
    pub fn manager(&self) -> ConnectionManager {
        self.manager.clone()
    }

    /// A fresh client, for a pub/sub connection the manager cannot serve.
    #[must_use]
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// The key prefix every component shares.
    #[must_use]
    pub fn keyspace(&self) -> &KeySpace {
        &self.keyspace
    }

    /// `PING`s the server.
    ///
    /// # Errors
    ///
    /// `cluster_redis.unavailable` when the server does not answer.
    pub async fn ping(&self) -> RedisResult<()> {
        let mut manager = self.manager();
        redis::cmd("PING")
            .query_async::<String>(&mut manager)
            .await
            .map(|_| ())
            .map_err(|error| RedisError::unavailable("PING", error))
    }
}
