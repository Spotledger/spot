//! [`StorageThrottleStore`]: sign-in throttle counters over records, not over memory.
//!
//! # The two counters
//!
//! - **Per account.** The platform app's `user` record already carries `failed_attempts`,
//!   `last_failed_at` and `locked_until` (read-only, no permission opens them), so the account
//!   counter is written where an operator and the audit already look for it: one short
//!   transaction that loads the row under a write lock, replaces the three counters and commits.
//!   The account is resolved through the **same name-or-email lookup** the user directory signs
//!   in with, so lockout and backoff engage whichever identifier is presented.
//!   A name that belongs to no user has no row to hold a counter, but it is **counted all the
//!   same**, through the same state machine, in a bounded per-process map
//!   ([`UNKNOWN_ACCOUNT_CAP`]): an unknown name backs off and locks exactly like a real one, so
//!   the response is no account-existence oracle. (The map is not shared between nodes or kept
//!   across restarts; the per-source counter and the durable real-account counter are.)
//! - **Per source.** There is no source-counter record, and inventing one is not in an owned
//!   path. The counter is therefore **derived** from the append-only `login_attempt` rows the
//!   service already writes for every attempt: the failures of one source digest inside the
//!   throttle window. `put` and `clear` for the source kind write nothing, because the attempts
//!   *are* the state; a success never washes out a source's failures (ADR-070 §7), so there is
//!   nothing to clear either.
//!
//! The window comes from [`ThrottleConfig`](spot_sessions::ThrottleConfig) at construction: the
//! derived counter counts only the failures of the last `window` seconds, so a quiet source
//! slides back to zero exactly as an in-memory counter would.
//!
//! No value here is a credential: the source is already a salted digest and the counters are
//! integers and instants.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_platform_app::doctype::{LOGIN_ATTEMPT, USER};
use spot_sessions::throttle::{FailureRecord, ThrottleError, ThrottleStore};
use spot_storage_contract::{
    DocumentStore, Expr, Fields, Name, Nulls, Order, OrderDir, Query, QueryExecutor, QueryPath,
    RowLock, Selection, StorageBackend, Transaction, Value, Wait,
};

use super::{LoginStores, instant, name, seconds, text};

/// The most attempt rows one derived source counter reads (a bound on the read, not on the
/// counter: a source with more failures than this inside a window is throttled anyway).
pub const SOURCE_WINDOW_ROWS: u32 = 512;

/// The most unknown sign-in names whose counters are held at once (a bound on memory, since an
/// attacker chooses the names).
pub const UNKNOWN_ACCOUNT_CAP: usize = 10_000;

/// The key of the one bucket every further unknown name shares once all
/// [`UNKNOWN_ACCOUNT_CAP`] slots hold a locked counter. It starts with a NUL, which a
/// normalised sign-in name cannot, so no real or invented name can address it.
const OVERFLOW_KEY: &str = "\u{0}overflow";

/// The bounded in-memory counters of names that belong to no user.
///
/// An attacker chooses the names, so the map is capped. When it is full, entries that have aged
/// out of the window go first, then the oldest *unlocked* entry; a locked entry is never evicted
/// to make room (a recorded security finding: evicting it would make a flood of names erase a lock, a weak oracle on
/// which names are real). If every entry is locked, new names share the single overflow bucket,
/// whose lock touches no real account because real accounts keep their counters on their own
/// records.
#[derive(Default)]
struct UnknownCounters {
    map: HashMap<String, FailureRecord>,
}

impl UnknownCounters {
    fn get(&self, account: &str) -> FailureRecord {
        let key = account.to_lowercase();
        if let Some(record) = self.map.get(&key) {
            return *record;
        }
        if self.map.len() >= UNKNOWN_ACCOUNT_CAP {
            // A full map: a name with no slot of its own is counted in the overflow bucket.
            return self.map.get(OVERFLOW_KEY).copied().unwrap_or_default();
        }
        FailureRecord::default()
    }

    fn put(&mut self, account: &str, record: FailureRecord, window: Duration) {
        let mut key = account.to_lowercase();
        if record == FailureRecord::default() {
            if self.map.contains_key(&key) {
                self.map.remove(&key);
            } else if self.map.len() >= UNKNOWN_ACCOUNT_CAP {
                self.map.remove(OVERFLOW_KEY);
            }
            return;
        }
        if !self.map.contains_key(&key) && self.map.len() >= UNKNOWN_ACCOUNT_CAP {
            let window = i64::try_from(window.as_secs()).unwrap_or(i64::MAX);
            let newest = self.map.values().map(|r| r.last_failure).max().unwrap_or(0);
            // Drop what has aged out of the window and is not locked.
            self.map.retain(|k, r| {
                k == OVERFLOW_KEY || r.locked_until > newest || newest - r.last_failure <= window
            });
            if self.map.len() >= UNKNOWN_ACCOUNT_CAP {
                // Still full: evict the oldest unlocked entry, never a locked one.
                let oldest = self
                    .map
                    .iter()
                    .filter(|(k, r)| k.as_str() != OVERFLOW_KEY && r.locked_until <= newest)
                    .min_by_key(|(_, r)| r.last_failure)
                    .map(|(k, _)| k.clone());
                match oldest {
                    Some(oldest) => {
                        self.map.remove(&oldest);
                    }
                    None => {
                        // Every entry is locked: share the one overflow bucket.
                        key = OVERFLOW_KEY.to_owned();
                    }
                }
            }
        }
        self.map.insert(key, record);
    }
}

/// Throttle counters over the platform app's records.
pub struct StorageThrottleStore<B: StorageBackend> {
    stores: Arc<LoginStores<B>>,
    window: Duration,
    unknown: Mutex<UnknownCounters>,
}

impl<B: StorageBackend> StorageThrottleStore<B> {
    /// A store over one site's engine and capability, reading source failures inside `window`.
    #[must_use]
    pub fn new(stores: Arc<LoginStores<B>>, window: Duration) -> Self {
        Self {
            stores,
            window,
            unknown: Mutex::new(UnknownCounters::default()),
        }
    }

    fn column(text: &str) -> Result<Name, ThrottleError> {
        name(text).map_err(|error| ThrottleError::StoreFailed(error.store_text()))
    }

    fn entity(text: &str) -> Result<Name, ThrottleError> {
        name(text).map_err(|error| ThrottleError::StoreFailed(error.store_text()))
    }

    fn label(suffix: &str) -> String {
        format!("spot_login_{suffix}")
    }

    fn int_at(fields: &Fields, field: &str) -> i64 {
        fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .and_then(|(_, value)| match value {
                Value::Int(number) => Some(*number),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn seconds_at(fields: &Fields, field: &str) -> i64 {
        fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .and_then(|(_, value)| seconds(value))
            .unwrap_or(0)
    }

    /// The id of the user whose sign-in name **or e-mail** is `account`, case-insensitively:
    /// the same lookup the user directory signs in through, so an account is throttled under
    /// whichever identifier it is presented with.
    async fn user_id(
        &self,
        account: &str,
    ) -> Result<Option<spot_storage_contract::RecordId>, ThrottleError> {
        super::user::find_user_id(&self.stores, "throttle_user", account)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))
    }

    /// The counter of a name that belongs to no user (see the module docs).
    fn unknown_get(&self, account: &str) -> FailureRecord {
        self.unknown
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account)
    }

    /// Replaces (or, for an all-zero record, removes) the counter of an unknown name.
    fn unknown_put(&self, account: &str, record: FailureRecord) {
        self.unknown
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .put(account, record, self.window);
    }

    /// The account counter, from the user's own fields.
    async fn account_record(&self, account: &str) -> Result<FailureRecord, ThrottleError> {
        let Some(id) = self.user_id(account).await? else {
            return Ok(self.unknown_get(account));
        };
        let mut tx = self
            .stores
            .begin(&Self::label("throttle_account"), false)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        let entity = Self::entity(USER)?;
        let loaded = tx.load(&entity, id, RowLock::None).await;
        let _ = tx.rollback().await;
        let Some(loaded) = loaded.map_err(|error| {
            ThrottleError::StoreFailed(super::failed_text("throttle load", &error))
        })?
        else {
            return Ok(FailureRecord::default());
        };
        let fields = &loaded.data.fields;
        Ok(FailureRecord {
            failures: u32::try_from(Self::int_at(fields, "failed_attempts")).unwrap_or(u32::MAX),
            window_start: Self::seconds_at(fields, "last_failed_at"),
            locked_until: Self::seconds_at(fields, "locked_until"),
            last_failure: Self::seconds_at(fields, "last_failed_at"),
        })
    }

    /// Writes the account counter back onto the user record.
    async fn write_account(
        &self,
        account: &str,
        record: FailureRecord,
    ) -> Result<(), ThrottleError> {
        let Some(id) = self.user_id(account).await? else {
            // No user record: the name is counted in the bounded unknown-name map, through
            // the same state machine, so it locks and backs off exactly like a real account.
            self.unknown_put(account, record);
            return Ok(());
        };
        let mut tx = self
            .stores
            .begin(&Self::label("throttle_write"), true)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        let entity = Self::entity(USER)?;
        let loaded = tx.load(&entity, id, RowLock::Update(Wait::Block)).await;
        let Some(mut loaded) = loaded.map_err(|error| {
            ThrottleError::StoreFailed(super::failed_text("throttle load", &error))
        })?
        else {
            let _ = tx.rollback().await;
            return Ok(());
        };
        let date = |seconds: i64| -> Result<Value, ThrottleError> {
            if seconds == 0 {
                return Ok(Value::Null);
            }
            instant(seconds)
                .map(Value::DateTime)
                .map_err(|error| ThrottleError::StoreFailed(error.store_text()))
        };
        loaded.data.fields.insert(
            Self::column("failed_attempts")?,
            Value::Int(i64::from(record.failures)),
        );
        loaded
            .data
            .fields
            .insert(Self::column("last_failed_at")?, date(record.last_failure)?);
        loaded
            .data
            .fields
            .insert(Self::column("locked_until")?, date(record.locked_until)?);
        let updated = tx.update(&loaded.data, loaded.version).await;
        match updated {
            Ok(_) => tx.commit().await.map_err(|error| {
                ThrottleError::StoreFailed(super::failed_text("commit throttle", &error))
            }),
            Err(error) => {
                let _ = tx.rollback().await;
                Err(ThrottleError::StoreFailed(super::failed_text(
                    "throttle write",
                    &error,
                )))
            }
        }
    }

    /// The source counter, derived from the `login_attempt` rows inside the window.
    async fn source_record(&self, key: &[u8]) -> Result<FailureRecord, ThrottleError> {
        let digest = source_digest_text(key);
        let entity = Self::entity(LOGIN_ATTEMPT)?;
        let alias = Self::column("a")?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select = vec![
            Selection {
                expr: Expr::field(&alias, &Self::column("at")?),
                alias: Self::column("at")?,
            },
            Selection {
                expr: Expr::field(&alias, &Self::column("result_code")?),
                alias: Self::column("result")?,
            },
        ];
        query.filter = Some(
            Expr::field(&alias, &Self::column("source_digest")?)
                .eq(Expr::value(Value::Text(digest))),
        );
        query.order_by = vec![Order {
            expr: Expr::field(&alias, &Self::column("at")?),
            dir: OrderDir::Desc,
            nulls: Nulls::Last,
        }];
        query.limit = Some(SOURCE_WINDOW_ROWS);
        let mut tx = self
            .stores
            .begin(&Self::label("throttle_source"), false)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        let rows = rows
            .map_err(|error| {
                ThrottleError::StoreFailed(super::failed_text("throttle source", &error))
            })?
            .rows;
        let mut failures = 0u32;
        let mut newest = 0i64;
        let mut oldest = 0i64;
        let window = i64::try_from(self.window.as_secs()).unwrap_or(i64::MAX);
        for row in rows {
            let at = row.first().and_then(seconds).unwrap_or(0);
            let result = row.get(1).and_then(text).unwrap_or_default();
            if at == 0 || result == crate::service::RESULT_OK {
                continue;
            }
            if newest == 0 {
                newest = at;
            }
            if newest - at > window {
                // Ordered newest first: everything after this is older still.
                break;
            }
            failures = failures.saturating_add(1);
            oldest = at;
        }
        Ok(FailureRecord {
            failures,
            // Measured from the newest failure, so an active source cannot age its failures
            // out while it keeps trying.
            window_start: newest,
            locked_until: 0,
            last_failure: if oldest == 0 { newest } else { oldest },
        })
    }
}

/// The `source_digest` text of a source key: the digest's first eight bytes in base16, exactly
/// as [`spot_sessions::SourceHash`]'s `Display` renders it for the audit row.
fn source_digest_text(key: &[u8]) -> String {
    super::hex(&key[..key.len().min(8)])
}

impl<B: StorageBackend> ThrottleStore for StorageThrottleStore<B> {
    fn get<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FailureRecord, ThrottleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if account {
                let name = account_text(key);
                self.account_record(&name).await
            } else {
                self.source_record(key).await
            }
        })
    }

    fn put<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
        record: FailureRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ThrottleError>> + Send + 'a>>
    {
        Box::pin(async move {
            if account {
                let name = account_text(key);
                self.write_account(&name, record).await
            } else {
                // The attempt rows are the source counter (see the module docs).
                Ok(())
            }
        })
    }

    fn clear<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ThrottleError>> + Send + 'a>>
    {
        Box::pin(async move {
            if account {
                let name = account_text(key);
                self.write_account(&name, FailureRecord::default()).await
            } else {
                Ok(())
            }
        })
    }
}

/// An account key's text (the normalised sign-in name the throttle keys on).
fn account_text(key: &[u8]) -> String {
    String::from_utf8_lossy(key).into_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spot_sessions::throttle::FailureRecord;

    use super::{OVERFLOW_KEY, UNKNOWN_ACCOUNT_CAP, UnknownCounters, source_digest_text};

    const WINDOW: Duration = Duration::from_secs(900);

    fn failed(at: i64) -> FailureRecord {
        FailureRecord {
            failures: 1,
            window_start: at,
            locked_until: 0,
            last_failure: at,
        }
    }

    fn locked(at: i64) -> FailureRecord {
        FailureRecord {
            failures: 10,
            window_start: at,
            locked_until: at + 1_800,
            last_failure: at,
        }
    }

    #[test]
    fn a_flood_of_unknown_names_does_not_evict_a_locked_name() {
        let mut counters = UnknownCounters::default();
        counters.put("victim", locked(1_000), WINDOW);
        // Twice the cap of invented names, each fresher than the locked one.
        for n in 0..(2 * UNKNOWN_ACCOUNT_CAP) {
            let at = 1_001 + i64::try_from(n % 100).unwrap_or(0);
            counters.put(&format!("flood-{n}"), failed(at), WINDOW);
        }
        assert_eq!(counters.get("victim"), locked(1_000), "the lock survives");
        assert!(counters.map.len() <= UNKNOWN_ACCOUNT_CAP + 1);
    }

    #[test]
    fn when_every_entry_is_locked_new_names_share_one_overflow_bucket() {
        let mut counters = UnknownCounters::default();
        for n in 0..UNKNOWN_ACCOUNT_CAP {
            counters.put(&format!("locked-{n}"), locked(1_000), WINDOW);
        }
        counters.put("newcomer-a", failed(1_100), WINDOW);
        // A different new name sees the same bucket, and no locked name was touched.
        assert_eq!(counters.get("newcomer-b"), failed(1_100));
        assert!(counters.map.contains_key(OVERFLOW_KEY));
        assert_eq!(counters.get("locked-0"), locked(1_000));
        assert!(counters.map.len() <= UNKNOWN_ACCOUNT_CAP + 1);
    }

    #[test]
    fn the_oldest_unlocked_entry_goes_first() {
        let mut counters = UnknownCounters::default();
        counters.put("oldest", failed(100), WINDOW);
        for n in 0..(UNKNOWN_ACCOUNT_CAP - 1) {
            counters.put(&format!("n-{n}"), failed(200), WINDOW);
        }
        counters.put("fresh", failed(250), WINDOW);
        assert_eq!(counters.get("oldest"), FailureRecord::default());
        assert_eq!(counters.get("fresh"), failed(250));
    }

    #[test]
    fn the_source_key_renders_the_audit_digest() {
        let key = [0xab, 0xcd, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0xff, 0xfe];
        assert_eq!(source_digest_text(&key), "abcd010203040506");
        assert_eq!(source_digest_text(&[1, 2]), "0102");
    }
}
