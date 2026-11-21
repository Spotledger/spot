//! Recovery codes over `recovery_code` records (WP-PLT-02.T10).
//!
//! A user's recovery codes are one top-level record each: the user link, the code's SHA-256
//! digest as a **lookup secret** (sealed, blind-indexed, the same kind of value as a session
//! token digest), `created_at` and `used_at`. No digest has a plain column, no read path returns
//! one to anyone but this crate, and no code is ever stored.
//!
//! # Order and identity
//!
//! The directory port names a code by its index in `UserRecord::recovery_digests`. That list is
//! every row of the user, **used ones included**, ordered by `(created_at, id)`, so an index is
//! stable until the batch is replaced. A presented code that matches a used row is found like any
//! other and then refused by `consume` as `login.recovery_code_used`, so reuse is counted and
//! audited as a failed sign-in, not silently skipped.
//!
//! # Exactly once
//!
//! `consume` loads the row under a write lock, checks that it belongs to the user and that
//! `used_at` is empty, and sets it in the same transaction: a compare-and-set on `used_at`. Two
//! concurrent uses of one code serialise on the lock; the second sees the first's `used_at` and
//! is refused.
//!
//! # Replacing the batch
//!
//! `replace_batch` deletes every row of the user in one transaction and then issues the new
//! rows. A crash between the two leaves the user with a second factor and no codes, which fails
//! closed (they regenerate); it never leaves two valid batches. A disabled factor deletes the
//! batch too.

use spot_app_api::Document;
use spot_auth::StoredDigest;
use spot_platform_app::doctype::RECOVERY_CODE;
use spot_secrets::SecretString;
use spot_storage_contract::{
    DocumentStore, Expr, Nulls, Order, OrderDir, Query, QueryExecutor, QueryPath, RecordId,
    RowLock, Selection, StorageBackend, Transaction, Value, Wait,
};

use super::{LoginStores, failed_at, hex, instant, name, record, store_failed, unhex};
use crate::error::{LoginError, LoginErrorKind};

/// The most recovery codes one user may hold (a bound on what a caller can ask the store to write).
pub const MAX_CODES: usize = 32;

fn label(suffix: &str) -> String {
    format!("spot_login_{suffix}")
}

/// The ids of every recovery code of `user`, in the stable `(created_at, id)` order.
pub(super) async fn ids_for<B: StorageBackend>(
    stores: &LoginStores<B>,
    user: RecordId,
) -> Result<Vec<RecordId>, LoginError> {
    let entity = name(RECOVERY_CODE)?;
    let alias = name("c")?;
    let mut query = Query::from(&entity, &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &name("id")?),
        alias: name("v")?,
    }];
    query.filter = Some(Expr::field(&alias, &name("user")?).eq(Expr::value(Value::Id(user))));
    query.order_by = vec![
        Order {
            expr: Expr::field(&alias, &name("created_at")?),
            dir: OrderDir::Asc,
            nulls: Nulls::First,
        },
        Order {
            expr: Expr::field(&alias, &name("id")?),
            dir: OrderDir::Asc,
            nulls: Nulls::First,
        },
    ];
    query.limit = Some(u32::try_from(MAX_CODES * 4).unwrap_or(u32::MAX));
    let mut tx = stores.begin(&label("recovery_ids"), false).await?;
    let rows = tx.query(&query).await;
    let _ = tx.rollback().await;
    Ok(rows
        .map_err(|error| failed_at("list recovery codes", &error))?
        .rows
        .into_iter()
        .flatten()
        .filter_map(|value| record(&value))
        .collect())
}

/// The digests of every recovery code of `user` (used ones included), in the stable order.
pub(super) async fn digests_for<B: StorageBackend>(
    stores: &LoginStores<B>,
    user: RecordId,
) -> Result<Vec<StoredDigest>, LoginError> {
    let mut out = Vec::new();
    for id in ids_for(stores, user).await? {
        let opened = stores
            .engine()
            .open_secret(stores.access(), RECOVERY_CODE, id, "digest")
            .await
            .map_err(|error| failed_at("open recovery digest", &error))?;
        // A row whose digest cannot be read is a store failure, never a skipped code: skipping
        // would shift every later index.
        let secret = opened.ok_or_else(|| store_failed("recovery_code: no digest"))?;
        let bytes = unhex(secret.expose())
            .ok_or_else(|| store_failed("recovery_code: digest is not base16"))?;
        out.push(
            StoredDigest::from_bytes(&bytes)
                .map_err(|_| store_failed("recovery_code: digest is not a digest"))?,
        );
    }
    Ok(out)
}

/// Spends the code at `index` of `user`'s list, exactly once.
///
/// # Errors
///
/// `login.recovery_code_invalid` when there is no code at `index` (the batch changed under the
/// caller), `login.recovery_code_used` when it was already spent (including by a concurrent
/// attempt), or `login.store_failed`.
pub(super) async fn consume<B: StorageBackend>(
    stores: &LoginStores<B>,
    user: RecordId,
    index: usize,
) -> Result<(), LoginError> {
    let ids = ids_for(stores, user).await?;
    let id = *ids.get(index).ok_or_else(|| {
        LoginError::new(
            LoginErrorKind::RecoveryCodeInvalid,
            "there is no recovery code at this position",
        )
    })?;
    let now = stores.clock().now_utc().unix_timestamp();
    let entity = name(RECOVERY_CODE)?;
    let mut tx = stores.begin(&label("consume_recovery"), true).await?;
    let loaded = tx.load(&entity, id, RowLock::Update(Wait::Block)).await;
    let Some(mut loaded) = loaded.map_err(|error| failed_at("load recovery code", &error))? else {
        let _ = tx.rollback().await;
        return Err(LoginError::new(
            LoginErrorKind::RecoveryCodeInvalid,
            "the recovery code is gone",
        ));
    };
    let field = |loaded: &spot_storage_contract::DocumentData, field: &str| {
        loaded
            .fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .map(|(_, value)| value.clone())
    };
    // The row must belong to the user (the lookup is by position within the user's own list, so
    // this is a second guard, not the first), and be unused.
    let owner = field(&loaded.data, "user").and_then(|value| record(&value));
    if owner != Some(user) {
        let _ = tx.rollback().await;
        return Err(LoginError::new(
            LoginErrorKind::RecoveryCodeInvalid,
            "the recovery code is not this user's",
        ));
    }
    if matches!(field(&loaded.data, "used_at"), Some(Value::DateTime(_))) {
        let _ = tx.rollback().await;
        return Err(LoginError::new(
            LoginErrorKind::RecoveryCodeUsed,
            "the recovery code was already used",
        ));
    }
    loaded
        .data
        .fields
        .insert(name("used_at")?, Value::DateTime(instant(now)?));
    let updated = tx.update(&loaded.data, loaded.version).await;
    match updated {
        Ok(_) => tx
            .commit()
            .await
            .map_err(|error| failed_at("commit recovery code", &error)),
        Err(error) => {
            let _ = tx.rollback().await;
            Err(failed_at("spend recovery code", &error))
        }
    }
}

/// Replaces `user`'s batch with `digests` (an empty list just deletes the batch).
///
/// # Errors
///
/// `login.store_failed` for more than `MAX_CODES` digests or a storage failure.
pub(super) async fn replace_batch<B: StorageBackend>(
    stores: &LoginStores<B>,
    user: RecordId,
    digests: &[StoredDigest],
) -> Result<(), LoginError> {
    if digests.len() > MAX_CODES {
        return Err(store_failed("recovery codes: too many in one batch"));
    }
    delete_batch(stores, user).await?;
    let now = instant(stores.clock().now_utc().unix_timestamp())?;
    for digest in digests {
        let mut doc = Document::new(name(RECOVERY_CODE)?, RecordId::new_v7());
        doc.set(name("user")?, Value::Id(user));
        doc.set(name("created_at")?, Value::DateTime(now));
        stores
            .engine()
            .create_with_secrets(
                stores.access(),
                doc,
                vec![(
                    "digest".to_owned(),
                    SecretString::new(hex(digest.as_bytes())),
                )],
            )
            .await
            .map_err(|error| failed_at("issue recovery code", &error))?;
    }
    Ok(())
}

/// Deletes every recovery code of `user`, in one transaction.
pub(super) async fn delete_batch<B: StorageBackend>(
    stores: &LoginStores<B>,
    user: RecordId,
) -> Result<(), LoginError> {
    let ids = ids_for(stores, user).await?;
    if ids.is_empty() {
        return Ok(());
    }
    let entity = name(RECOVERY_CODE)?;
    let mut tx = stores.begin(&label("delete_recovery"), true).await?;
    for id in ids {
        let loaded = tx.load(&entity, id, RowLock::Update(Wait::Block)).await;
        let Some(loaded) = loaded.map_err(|error| failed_at("load recovery code", &error))? else {
            continue;
        };
        if let Err(error) = tx.delete(&entity, id, loaded.version).await {
            let _ = tx.rollback().await;
            return Err(failed_at("delete recovery code", &error));
        }
    }
    tx.commit()
        .await
        .map_err(|error| failed_at("commit recovery delete", &error))
}
