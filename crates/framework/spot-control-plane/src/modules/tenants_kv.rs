//! The small control-schema store of the `tenants` module (`WP-CTL-01.T7`): key-value rows, leases and gates over PostgreSQL (`cp_tenants_kv`, `cp_tenants_lease`), with an in-memory twin for tests. Split from `tenants.rs` for the 1,000-line gate.

use crate::CpCode;
use crate::CpError;
use deadpool_postgres::Pool;
use serde::Serialize;
use serde::de::DeserializeOwned;
use spot_mgmt_api::ports::BoxFuture;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

// ---------------------------------------------------------------------------------------------
// The module's small control-schema store
// ---------------------------------------------------------------------------------------------

/// A held gate: serialises the count-and-insert of one edition.
pub struct Gate(Box<dyn std::any::Any + Send>);

/// The key-value rows, leases and gates the module keeps in the control schema.
pub trait Kv: Send + Sync {
    /// Inserts when the key is absent; `false` when it already exists.
    fn put_new<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, Result<bool, CpError>>;
    /// Inserts or replaces.
    fn put<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, Result<(), CpError>>;
    /// Reads one row.
    fn get<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, CpError>>;
    /// Removes a row (absent is success).
    fn del<'a>(&'a self, ns: &'a str, key: &'a str) -> BoxFuture<'a, Result<(), CpError>>;
    /// Every row of a namespace, ordered by key.
    fn list<'a>(&'a self, ns: &'a str) -> BoxFuture<'a, Result<Vec<(String, String)>, CpError>>;
    /// Takes the lease `id` for `holder` until `now + ttl` when it is free, expired or already ours.
    fn lease<'a>(
        &'a self,
        id: &'a str,
        holder: &'a str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>>;
    /// Releases the lease when `holder` holds it.
    fn unlease<'a>(&'a self, id: &'a str, holder: &'a str) -> BoxFuture<'a, Result<(), CpError>>;
    /// Enters the named gate, waiting for the holder to leave.
    fn gate<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Gate, CpError>>;
    /// Leaves the gate.
    fn ungate(&self, gate: Gate) -> BoxFuture<'_, ()>;
}

pub(crate) fn store_failed() -> CpError {
    CpError::with_reason(CpCode::Internal, "tenants.store_failed")
}

/// The in-memory [`Kv`], for tests and the development profile.
#[derive(Default)]
pub struct MemoryKv {
    rows: Mutex<BTreeMap<(String, String), String>>,
    leases: Mutex<BTreeMap<String, (String, i64)>>,
    gates: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl MemoryKv {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Kv for MemoryKv {
    fn put_new<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, Result<bool, CpError>> {
        let result = self
            .rows
            .lock()
            .map_err(|_| store_failed())
            .map(|mut rows| {
                let slot = (ns.to_owned(), key.to_owned());
                if let std::collections::btree_map::Entry::Vacant(e) = rows.entry(slot) {
                    e.insert(value.to_owned());
                    true
                } else {
                    false
                }
            });
        Box::pin(std::future::ready(result))
    }
    fn put<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, Result<(), CpError>> {
        let result = self
            .rows
            .lock()
            .map_err(|_| store_failed())
            .map(|mut rows| {
                rows.insert((ns.to_owned(), key.to_owned()), value.to_owned());
            });
        Box::pin(std::future::ready(result))
    }
    fn get<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, CpError>> {
        let result = self
            .rows
            .lock()
            .map_err(|_| store_failed())
            .map(|rows| rows.get(&(ns.to_owned(), key.to_owned())).cloned());
        Box::pin(std::future::ready(result))
    }
    fn del<'a>(&'a self, ns: &'a str, key: &'a str) -> BoxFuture<'a, Result<(), CpError>> {
        let result = self
            .rows
            .lock()
            .map_err(|_| store_failed())
            .map(|mut rows| {
                rows.remove(&(ns.to_owned(), key.to_owned()));
            });
        Box::pin(std::future::ready(result))
    }
    fn list<'a>(&'a self, ns: &'a str) -> BoxFuture<'a, Result<Vec<(String, String)>, CpError>> {
        let result = self.rows.lock().map_err(|_| store_failed()).map(|rows| {
            rows.iter()
                .filter(|((n, _), _)| n == ns)
                .map(|((_, k), v)| (k.clone(), v.clone()))
                .collect()
        });
        Box::pin(std::future::ready(result))
    }
    fn lease<'a>(
        &'a self,
        id: &'a str,
        holder: &'a str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>> {
        let result = self
            .leases
            .lock()
            .map_err(|_| store_failed())
            .map(|mut leases| {
                let free = leases
                    .get(id)
                    .is_none_or(|(owner, until)| owner == holder || *until < now_ms);
                if free {
                    leases.insert(
                        id.to_owned(),
                        (holder.to_owned(), now_ms.saturating_add(ttl_ms)),
                    );
                }
                free
            });
        Box::pin(std::future::ready(result))
    }
    fn unlease<'a>(&'a self, id: &'a str, holder: &'a str) -> BoxFuture<'a, Result<(), CpError>> {
        let result = self
            .leases
            .lock()
            .map_err(|_| store_failed())
            .map(|mut leases| {
                if leases.get(id).is_some_and(|(owner, _)| owner == holder) {
                    leases.remove(id);
                }
            });
        Box::pin(std::future::ready(result))
    }
    fn gate<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Gate, CpError>> {
        let mutex = self
            .gates
            .lock()
            .map_err(|_| store_failed())
            .map(|mut gates| Arc::clone(gates.entry(name.to_owned()).or_default()));
        Box::pin(async move {
            let guard = mutex?.lock_owned().await;
            Ok(Gate(Box::new(guard)))
        })
    }
    fn ungate(&self, gate: Gate) -> BoxFuture<'_, ()> {
        drop(gate);
        Box::pin(std::future::ready(()))
    }
}

/// The PostgreSQL [`Kv`] over the control pool (`cp_tenants_kv`, `cp_tenants_lease`).
pub struct PgKv {
    pool: Pool,
}

impl PgKv {
    /// A store over the control pool.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl Kv for PgKv {
    fn put_new<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, Result<bool, CpError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| store_failed())?;
            let n = client
                .execute("INSERT INTO cp_tenants_kv (ns, k, v) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING", &[&ns, &key, &value])
                .await
                .map_err(|_| store_failed())?;
            Ok(n == 1)
        })
    }
    fn put<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, Result<(), CpError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| store_failed())?;
            client
                .execute("INSERT INTO cp_tenants_kv (ns, k, v) VALUES ($1, $2, $3) ON CONFLICT (ns, k) DO UPDATE SET v = EXCLUDED.v", &[&ns, &key, &value])
                .await
                .map_err(|_| store_failed())?;
            Ok(())
        })
    }
    fn get<'a>(
        &'a self,
        ns: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, CpError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| store_failed())?;
            let row = client
                .query_opt(
                    "SELECT v FROM cp_tenants_kv WHERE ns = $1 AND k = $2",
                    &[&ns, &key],
                )
                .await
                .map_err(|_| store_failed())?;
            row.map(|r| r.try_get::<_, String>(0).map_err(|_| store_failed()))
                .transpose()
        })
    }
    fn del<'a>(&'a self, ns: &'a str, key: &'a str) -> BoxFuture<'a, Result<(), CpError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| store_failed())?;
            client
                .execute(
                    "DELETE FROM cp_tenants_kv WHERE ns = $1 AND k = $2",
                    &[&ns, &key],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(())
        })
    }
    fn list<'a>(&'a self, ns: &'a str) -> BoxFuture<'a, Result<Vec<(String, String)>, CpError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| store_failed())?;
            let rows = client
                .query(
                    "SELECT k, v FROM cp_tenants_kv WHERE ns = $1 ORDER BY k",
                    &[&ns],
                )
                .await
                .map_err(|_| store_failed())?;
            rows.iter()
                .map(|r| {
                    Ok((
                        r.try_get::<_, String>(0).map_err(|_| store_failed())?,
                        r.try_get::<_, String>(1).map_err(|_| store_failed())?,
                    ))
                })
                .collect()
        })
    }
    fn lease<'a>(
        &'a self,
        id: &'a str,
        holder: &'a str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| store_failed())?;
            let until = now_ms.saturating_add(ttl_ms);
            let n = client
                .execute(
                    "INSERT INTO cp_tenants_lease (id, holder, until) VALUES ($1, $2, $3) \
                     ON CONFLICT (id) DO UPDATE SET holder = EXCLUDED.holder, until = EXCLUDED.until \
                     WHERE cp_tenants_lease.until < $4 OR cp_tenants_lease.holder = EXCLUDED.holder",
                    &[&id, &holder, &until, &now_ms],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(n == 1)
        })
    }
    fn unlease<'a>(&'a self, id: &'a str, holder: &'a str) -> BoxFuture<'a, Result<(), CpError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| store_failed())?;
            client
                .execute(
                    "DELETE FROM cp_tenants_lease WHERE id = $1 AND holder = $2",
                    &[&id, &holder],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(())
        })
    }
    fn gate<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Gate, CpError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| store_failed())?;
            client
                .query(
                    "SELECT pg_advisory_lock(hashtext($1::text)::bigint)",
                    &[&name],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(Gate(Box::new((client, name.to_owned()))))
        })
    }
    fn ungate(&self, gate: Gate) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Ok(held) = gate.0.downcast::<(deadpool_postgres::Object, String)>() {
                let (client, name) = *held;
                let unlocked = client
                    .query(
                        "SELECT pg_advisory_unlock(hashtext($1::text)::bigint)",
                        &[&name],
                    )
                    .await
                    .is_ok();
                if !unlocked {
                    // A connection that may still hold the lock must not return to the pool.
                    drop(deadpool_postgres::Object::take(client));
                }
            }
        })
    }
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String, CpError> {
    serde_json::to_string(value).map_err(|_| store_failed())
}

pub(crate) fn from_json<T: DeserializeOwned>(text: &str) -> Result<T, CpError> {
    serde_json::from_str(text).map_err(|_| store_failed())
}
