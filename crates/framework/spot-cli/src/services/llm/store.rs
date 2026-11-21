//! The durable state of the LLM service: tenant configurations with their sealed keys, usage per
//! tenant, task and day, and the record of what was sent.
//!
//! Three tables of the site's own database (`spot_llm_tenant`, `spot_llm_usage`, `spot_llm_send`)
//! are created by [`Store::prepare`] (idempotent) and every statement carries the site name. A
//! store error is a fixed sentence: it never carries a row, a key or a driver message (which can
//! quote a value). [`Store::memory`] is the same API over a mutex, for tests and for a host with
//! no database; nothing in it survives the process.
//!
//! Counts are `bigint` columns; a count that cannot fit saturates rather than wrapping.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use deadpool_postgres::Pool;

use super::config::TenantConfig;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS spot_llm_tenant (
    site        text   NOT NULL,
    tenant      text   NOT NULL,
    config_json text   NOT NULL,
    key_sealed  bytea,
    updated_at  bigint NOT NULL,
    PRIMARY KEY (site, tenant)
);
CREATE TABLE IF NOT EXISTS spot_llm_usage (
    site          text   NOT NULL,
    tenant        text   NOT NULL,
    task          text   NOT NULL,
    day           bigint NOT NULL,
    provider      text   NOT NULL,
    model         text   NOT NULL,
    input_tokens  bigint NOT NULL DEFAULT 0,
    output_tokens bigint NOT NULL DEFAULT 0,
    cache_read    bigint NOT NULL DEFAULT 0,
    cache_write   bigint NOT NULL DEFAULT 0,
    requests      bigint NOT NULL DEFAULT 0,
    cost_micro    bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (site, tenant, task, day, provider, model)
);
CREATE TABLE IF NOT EXISTS spot_llm_send (
    site          text   NOT NULL,
    request_id    text   NOT NULL,
    tenant        text   NOT NULL,
    task          text   NOT NULL,
    at_ms         bigint NOT NULL,
    endpoint      text   NOT NULL,
    provider      text   NOT NULL,
    model         text   NOT NULL,
    digest        text   NOT NULL,
    size_bytes    bigint NOT NULL,
    doctypes      text   NOT NULL,
    classes       text   NOT NULL,
    principal     text   NOT NULL,
    purpose       text   NOT NULL,
    outcome       text   NOT NULL,
    input_tokens  bigint NOT NULL DEFAULT 0,
    output_tokens bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (site, request_id)
);
CREATE INDEX IF NOT EXISTS spot_llm_send_tenant_at ON spot_llm_send (site, tenant, at_ms);
";

/// What the store could not do. The text is fixed; it never quotes stored data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreError(pub &'static str);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for StoreError {}

fn pool_err<E>(_: E) -> StoreError {
    StoreError("the database is not available")
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn from_i64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// One row of settled usage: a tenant's tokens and cost for one task, day, provider and model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageRow {
    /// The tenant.
    pub tenant: String,
    /// The task the calls served.
    pub task: String,
    /// The UTC day, as days since the Unix epoch.
    pub day: u64,
    /// The provider id.
    pub provider: String,
    /// The model.
    pub model: String,
    /// Fresh prompt tokens.
    pub input: u64,
    /// Generated tokens.
    pub output: u64,
    /// Prompt tokens read from the provider's cache.
    pub cache_read: u64,
    /// Prompt tokens written to the provider's cache.
    pub cache_write: u64,
    /// How many calls the row adds up.
    pub requests: u64,
    /// The cost in micro-units, an exact integer.
    pub cost_micro: u64,
}

impl UsageRow {
    fn key(&self) -> (String, String, u64, String, String) {
        (
            self.tenant.clone(),
            self.task.clone(),
            self.day,
            self.provider.clone(),
            self.model.clone(),
        )
    }
}

/// The record of one model call: what went where, never the content and never the key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SendRecord {
    /// The call's request id (`tenant:task:caller`).
    pub request_id: String,
    /// The tenant.
    pub tenant: String,
    /// The task.
    pub task: String,
    /// When the call was made, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The endpoint as `scheme://host[:port]`: no path, no query.
    pub endpoint: String,
    /// The provider id.
    pub provider: String,
    /// The model.
    pub model: String,
    /// The SHA-256 of the request as it was built, lower-case hex.
    pub digest: String,
    /// The size of the serialised request in bytes.
    pub size_bytes: u64,
    /// The DocTypes whose fields were included, sorted.
    pub doctypes: Vec<String>,
    /// How many fields of each class were included (`plain`, `personal`) and stripped
    /// (`stripped_protected`, `stripped_sealed`, `stripped_personal`).
    pub classes: BTreeMap<String, u64>,
    /// The user the call was made for.
    pub principal: String,
    /// The purpose the caller gave.
    pub purpose: String,
    /// `sent`, or `refused_<reason>` / `failed_<reason>` when nothing (more) was sent.
    pub outcome: String,
    /// Input tokens the provider reported (0 when none).
    pub input_tokens: u64,
    /// Output tokens the provider reported (0 when none).
    pub output_tokens: u64,
}

#[derive(Default)]
struct MemState {
    tenants: BTreeMap<String, (String, Option<Vec<u8>>)>,
    usage: BTreeMap<(String, String, u64, String, String), UsageRow>,
    sends: BTreeMap<String, SendRecord>,
}

#[derive(Clone)]
enum Backing {
    Pg { pool: Pool, site: String },
    Memory(Arc<Mutex<MemState>>),
}

/// The store: PostgreSQL in a running site, a mutex in a test.
#[derive(Clone)]
pub struct Store {
    backing: Backing,
}

impl Store {
    /// A store over `pool` for `site`.
    #[must_use]
    pub fn pg(pool: Pool, site: &str) -> Self {
        Self {
            backing: Backing::Pg {
                pool,
                site: site.to_owned(),
            },
        }
    }

    /// An in-memory store.
    #[must_use]
    pub fn memory() -> Self {
        Self {
            backing: Backing::Memory(Arc::new(Mutex::new(MemState::default()))),
        }
    }

    fn mem(state: &Arc<Mutex<MemState>>) -> std::sync::MutexGuard<'_, MemState> {
        state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Creates the tables if they are not there.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the database cannot be reached or refuses the DDL.
    pub async fn prepare(&self) -> Result<(), StoreError> {
        match &self.backing {
            Backing::Memory(_) => Ok(()),
            Backing::Pg { pool, .. } => {
                let client = pool.get().await.map_err(pool_err)?;
                client
                    .batch_execute(DDL)
                    .await
                    .map_err(|_| StoreError("the LLM tables could not be created"))
            }
        }
    }

    /// Stores a tenant's configuration (the key, if any, is kept).
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the configuration cannot be encoded or written.
    pub async fn put_config(&self, config: &TenantConfig, now_ms: u64) -> Result<(), StoreError> {
        let text = serde_json::to_string(config)
            .map_err(|_| StoreError("the configuration could not be encoded"))?;
        match &self.backing {
            Backing::Memory(state) => {
                let mut state = Self::mem(state);
                let key = state
                    .tenants
                    .get(&config.tenant)
                    .and_then(|(_, key)| key.clone());
                state.tenants.insert(config.tenant.clone(), (text, key));
                Ok(())
            }
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                client
                    .execute(
                        "INSERT INTO spot_llm_tenant (site, tenant, config_json, updated_at) \
                         VALUES ($1, $2, $3, $4) \
                         ON CONFLICT (site, tenant) DO UPDATE SET config_json = $3, updated_at = $4",
                        &[site, &config.tenant, &text, &to_i64(now_ms)],
                    )
                    .await
                    .map(|_| ())
                    .map_err(|_| StoreError("the configuration could not be written"))
            }
        }
    }

    /// A tenant's configuration, if it has one. A stored row that does not parse is an error, not
    /// a guess.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails or the row is not one this service wrote.
    pub async fn config(&self, tenant: &str) -> Result<Option<TenantConfig>, StoreError> {
        let text: Option<String> = match &self.backing {
            Backing::Memory(state) => Self::mem(state)
                .tenants
                .get(tenant)
                .map(|(config, _)| config.clone()),
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                client
                    .query_opt(
                        "SELECT config_json FROM spot_llm_tenant WHERE site = $1 AND tenant = $2",
                        &[site, &tenant],
                    )
                    .await
                    .map_err(|_| StoreError("the configuration could not be read"))?
                    .map(|row| row.get::<_, String>(0))
            }
        };
        match text {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|_| StoreError("a stored configuration is not valid")),
        }
    }

    /// The tenants that have a configuration, in name order.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails.
    pub async fn tenants(&self) -> Result<Vec<String>, StoreError> {
        match &self.backing {
            Backing::Memory(state) => Ok(Self::mem(state).tenants.keys().cloned().collect()),
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                let rows = client
                    .query(
                        "SELECT tenant FROM spot_llm_tenant WHERE site = $1 ORDER BY tenant",
                        &[site],
                    )
                    .await
                    .map_err(|_| StoreError("the tenants could not be read"))?;
                Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
            }
        }
    }

    /// Stores the sealed key of a tenant that already has a configuration.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the tenant has no configuration or the write fails.
    pub async fn put_key(
        &self,
        tenant: &str,
        sealed: &[u8],
        now_ms: u64,
    ) -> Result<(), StoreError> {
        match &self.backing {
            Backing::Memory(state) => {
                let mut state = Self::mem(state);
                match state.tenants.get_mut(tenant) {
                    Some(entry) => {
                        entry.1 = Some(sealed.to_vec());
                        Ok(())
                    }
                    None => Err(StoreError("the tenant has no configuration")),
                }
            }
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                let sealed_vec = sealed.to_vec();
                let changed = client
                    .execute(
                        "UPDATE spot_llm_tenant SET key_sealed = $3, updated_at = $4 \
                         WHERE site = $1 AND tenant = $2",
                        &[site, &tenant, &sealed_vec, &to_i64(now_ms)],
                    )
                    .await
                    .map_err(|_| StoreError("the key could not be written"))?;
                if changed == 0 {
                    return Err(StoreError("the tenant has no configuration"));
                }
                Ok(())
            }
        }
    }

    /// Deletes the sealed key of a tenant (the configuration is kept). Returns whether a key was
    /// there.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the write fails.
    pub async fn delete_key(&self, tenant: &str, now_ms: u64) -> Result<bool, StoreError> {
        match &self.backing {
            Backing::Memory(state) => {
                let mut state = Self::mem(state);
                Ok(state
                    .tenants
                    .get_mut(tenant)
                    .and_then(|entry| entry.1.take())
                    .is_some())
            }
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                let changed = client
                    .execute(
                        "UPDATE spot_llm_tenant SET key_sealed = NULL, updated_at = $3 \
                         WHERE site = $1 AND tenant = $2 AND key_sealed IS NOT NULL",
                        &[site, &tenant, &to_i64(now_ms)],
                    )
                    .await
                    .map_err(|_| StoreError("the key could not be removed"))?;
                Ok(changed > 0)
            }
        }
    }

    /// The sealed key of a tenant (`None` when no key was ever written).
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails.
    pub async fn key(&self, tenant: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match &self.backing {
            Backing::Memory(state) => Ok(Self::mem(state)
                .tenants
                .get(tenant)
                .and_then(|(_, key)| key.clone())),
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                let row = client
                    .query_opt(
                        "SELECT key_sealed FROM spot_llm_tenant WHERE site = $1 AND tenant = $2",
                        &[site, &tenant],
                    )
                    .await
                    .map_err(|_| StoreError("the key could not be read"))?;
                Ok(row.and_then(|r| r.get::<_, Option<Vec<u8>>>(0)))
            }
        }
    }

    /// Adds one settled call to its usage row (created on first use).
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the write fails.
    pub async fn add_usage(&self, row: &UsageRow) -> Result<(), StoreError> {
        match &self.backing {
            Backing::Memory(state) => {
                let mut state = Self::mem(state);
                let entry = state.usage.entry(row.key()).or_insert_with(|| UsageRow {
                    tenant: row.tenant.clone(),
                    task: row.task.clone(),
                    day: row.day,
                    provider: row.provider.clone(),
                    model: row.model.clone(),
                    ..UsageRow::default()
                });
                entry.input = entry.input.saturating_add(row.input);
                entry.output = entry.output.saturating_add(row.output);
                entry.cache_read = entry.cache_read.saturating_add(row.cache_read);
                entry.cache_write = entry.cache_write.saturating_add(row.cache_write);
                entry.requests = entry.requests.saturating_add(row.requests);
                entry.cost_micro = entry.cost_micro.saturating_add(row.cost_micro);
                Ok(())
            }
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                client
                    .execute(
                        "INSERT INTO spot_llm_usage (site, tenant, task, day, provider, model, \
                         input_tokens, output_tokens, cache_read, cache_write, requests, cost_micro) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                         ON CONFLICT (site, tenant, task, day, provider, model) DO UPDATE SET \
                         input_tokens = spot_llm_usage.input_tokens + $7, \
                         output_tokens = spot_llm_usage.output_tokens + $8, \
                         cache_read = spot_llm_usage.cache_read + $9, \
                         cache_write = spot_llm_usage.cache_write + $10, \
                         requests = spot_llm_usage.requests + $11, \
                         cost_micro = spot_llm_usage.cost_micro + $12",
                        &[
                            site,
                            &row.tenant,
                            &row.task,
                            &to_i64(row.day),
                            &row.provider,
                            &row.model,
                            &to_i64(row.input),
                            &to_i64(row.output),
                            &to_i64(row.cache_read),
                            &to_i64(row.cache_write),
                            &to_i64(row.requests),
                            &to_i64(row.cost_micro),
                        ],
                    )
                    .await
                    .map(|_| ())
                    .map_err(|_| StoreError("the usage could not be written"))
            }
        }
    }

    /// A tenant's usage rows from `first_day` (inclusive) on.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails.
    pub async fn usage_since(
        &self,
        tenant: &str,
        first_day: u64,
    ) -> Result<Vec<UsageRow>, StoreError> {
        match &self.backing {
            Backing::Memory(state) => Ok(Self::mem(state)
                .usage
                .values()
                .filter(|r| r.tenant == tenant && r.day >= first_day)
                .cloned()
                .collect()),
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                let rows = client
                    .query(
                        "SELECT task, day, provider, model, input_tokens, output_tokens, \
                         cache_read, cache_write, requests, cost_micro \
                         FROM spot_llm_usage WHERE site = $1 AND tenant = $2 AND day >= $3 \
                         ORDER BY day, task, provider, model",
                        &[site, &tenant, &to_i64(first_day)],
                    )
                    .await
                    .map_err(|_| StoreError("the usage could not be read"))?;
                Ok(rows
                    .iter()
                    .map(|r| UsageRow {
                        tenant: tenant.to_owned(),
                        task: r.get(0),
                        day: from_i64(r.get(1)),
                        provider: r.get(2),
                        model: r.get(3),
                        input: from_i64(r.get(4)),
                        output: from_i64(r.get(5)),
                        cache_read: from_i64(r.get(6)),
                        cache_write: from_i64(r.get(7)),
                        requests: from_i64(r.get(8)),
                        cost_micro: from_i64(r.get(9)),
                    })
                    .collect())
            }
        }
    }

    /// Writes (or, for the same request id, replaces) the record of one call.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the write fails.
    pub async fn put_send(&self, record: &SendRecord) -> Result<(), StoreError> {
        let doctypes = record.doctypes.join(",");
        let classes = serde_json::to_string(&record.classes)
            .map_err(|_| StoreError("the send record could not be encoded"))?;
        match &self.backing {
            Backing::Memory(state) => {
                Self::mem(state)
                    .sends
                    .insert(record.request_id.clone(), record.clone());
                Ok(())
            }
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                client
                    .execute(
                        "INSERT INTO spot_llm_send (site, request_id, tenant, task, at_ms, endpoint, \
                         provider, model, digest, size_bytes, doctypes, classes, principal, purpose, \
                         outcome, input_tokens, output_tokens) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17) \
                         ON CONFLICT (site, request_id) DO UPDATE SET outcome = $15, \
                         input_tokens = $16, output_tokens = $17",
                        &[
                            site,
                            &record.request_id,
                            &record.tenant,
                            &record.task,
                            &to_i64(record.at_ms),
                            &record.endpoint,
                            &record.provider,
                            &record.model,
                            &record.digest,
                            &to_i64(record.size_bytes),
                            &doctypes,
                            &classes,
                            &record.principal,
                            &record.purpose,
                            &record.outcome,
                            &to_i64(record.input_tokens),
                            &to_i64(record.output_tokens),
                        ],
                    )
                    .await
                    .map(|_| ())
                    .map_err(|_| StoreError("the send record could not be written"))
            }
        }
    }

    /// A tenant's send records, newest first, at most `limit`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails or a row does not parse.
    pub async fn sends(&self, tenant: &str, limit: u32) -> Result<Vec<SendRecord>, StoreError> {
        let limit = limit.clamp(1, 1_000);
        match &self.backing {
            Backing::Memory(state) => {
                let mut all: Vec<SendRecord> = Self::mem(state)
                    .sends
                    .values()
                    .filter(|r| r.tenant == tenant)
                    .cloned()
                    .collect();
                all.sort_by(|a, b| {
                    b.at_ms
                        .cmp(&a.at_ms)
                        .then_with(|| b.request_id.cmp(&a.request_id))
                });
                all.truncate(limit as usize);
                Ok(all)
            }
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                let rows = client
                    .query(
                        "SELECT request_id, task, at_ms, endpoint, provider, model, digest, \
                         size_bytes, doctypes, classes, principal, purpose, outcome, \
                         input_tokens, output_tokens \
                         FROM spot_llm_send WHERE site = $1 AND tenant = $2 \
                         ORDER BY at_ms DESC, request_id DESC LIMIT $3",
                        &[site, &tenant, &i64::from(limit)],
                    )
                    .await
                    .map_err(|_| StoreError("the send records could not be read"))?;
                rows.iter()
                    .map(|r| {
                        let doctypes: String = r.get(8);
                        let classes: String = r.get(9);
                        let classes: BTreeMap<String, u64> = serde_json::from_str(&classes)
                            .map_err(|_| StoreError("a stored send record is not valid"))?;
                        Ok(SendRecord {
                            request_id: r.get(0),
                            tenant: tenant.to_owned(),
                            task: r.get(1),
                            at_ms: from_i64(r.get(2)),
                            endpoint: r.get(3),
                            provider: r.get(4),
                            model: r.get(5),
                            digest: r.get(6),
                            size_bytes: from_i64(r.get(7)),
                            doctypes: if doctypes.is_empty() {
                                Vec::new()
                            } else {
                                doctypes.split(',').map(str::to_owned).collect()
                            },
                            classes,
                            principal: r.get(10),
                            purpose: r.get(11),
                            outcome: r.get(12),
                            input_tokens: from_i64(r.get(13)),
                            output_tokens: from_i64(r.get(14)),
                        })
                    })
                    .collect()
            }
        }
    }

    /// Deletes the send records older than `before_ms`; returns how many went.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the delete fails.
    pub async fn purge_sends_before(&self, before_ms: u64) -> Result<u64, StoreError> {
        match &self.backing {
            Backing::Memory(state) => {
                let mut state = Self::mem(state);
                let before = state.sends.len();
                state.sends.retain(|_, r| r.at_ms >= before_ms);
                Ok((before - state.sends.len()) as u64)
            }
            Backing::Pg { pool, site } => {
                let client = pool.get().await.map_err(pool_err)?;
                client
                    .execute(
                        "DELETE FROM spot_llm_send WHERE site = $1 AND at_ms < $2",
                        &[site, &to_i64(before_ms)],
                    )
                    .await
                    .map_err(|_| StoreError("the old send records could not be deleted"))
            }
        }
    }

    /// A readiness probe: the tenant table answers.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when it does not.
    pub async fn probe(&self) -> Result<(), StoreError> {
        self.tenants().await.map(|_| ())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::config::{Kind, PriceConfig};
    use super::*;

    fn cfg(tenant: &str) -> TenantConfig {
        TenantConfig {
            tenant: tenant.into(),
            kind: Kind::Ollama,
            endpoint: "http://llm.test".into(),
            model: "m".into(),
            allowed_hosts: vec!["llm.test".into()],
            price: PriceConfig::default(),
            context_window: 10_000,
            max_output: 1_000,
            default_max_output_tokens: 100,
            tasks: vec!["chat".into()],
            budget: None,
            embedding_model: None,
            embedding_dimensions: None,
        }
    }

    #[tokio::test]
    async fn a_deleted_key_is_gone_and_the_configuration_stays() {
        let store = Store::memory();
        store.put_config(&cfg("a"), 1).await.unwrap();
        assert!(!store.delete_key("a", 2).await.unwrap());
        store.put_key("a", b"sealed", 3).await.unwrap();
        assert!(store.delete_key("a", 4).await.unwrap());
        assert!(store.key("a").await.unwrap().is_none());
        assert!(store.config("a").await.unwrap().is_some());
        assert!(!store.delete_key("a", 5).await.unwrap());
        assert!(!store.delete_key("missing", 5).await.unwrap());
    }

    #[tokio::test]
    async fn a_new_configuration_keeps_the_key() {
        let store = Store::memory();
        store.put_config(&cfg("a"), 1).await.unwrap();
        store.put_key("a", b"sealed", 2).await.unwrap();
        store.put_config(&cfg("a"), 3).await.unwrap();
        assert_eq!(
            store.key("a").await.unwrap().as_deref(),
            Some(&b"sealed"[..])
        );
        assert!(store.put_key("missing", b"x", 4).await.is_err());
    }

    #[tokio::test]
    async fn usage_adds_up_per_row() {
        let store = Store::memory();
        let row = UsageRow {
            tenant: "a".into(),
            task: "t".into(),
            day: 5,
            provider: "p".into(),
            model: "m".into(),
            input: 10,
            output: 4,
            requests: 1,
            cost_micro: 7,
            ..UsageRow::default()
        };
        store.add_usage(&row).await.unwrap();
        store.add_usage(&row).await.unwrap();
        let rows = store.usage_since("a", 0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].input, rows[0].requests, rows[0].cost_micro),
            (20, 2, 14)
        );
        assert!(store.usage_since("a", 6).await.unwrap().is_empty());
        assert!(store.usage_since("b", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn old_send_records_are_purged() {
        let store = Store::memory();
        for (id, at) in [("a:t:1", 10u64), ("a:t:2", 20)] {
            store
                .put_send(&SendRecord {
                    request_id: id.into(),
                    tenant: "a".into(),
                    at_ms: at,
                    ..SendRecord::default()
                })
                .await
                .unwrap();
        }
        assert_eq!(store.purge_sends_before(15).await.unwrap(), 1);
        let left = store.sends("a", 10).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].request_id, "a:t:2");
    }
}
