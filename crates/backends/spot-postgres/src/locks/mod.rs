//! `Locks` for [`PgTransaction`]: named guard locks on PostgreSQL's transaction-level advisory
//! locks (WP-STO-04.T1, ADR-46, ADR-053 "Concurrency").
//!
//! A guard lock is `pg_advisory_xact_lock[_shared](int4, int4)`. PostgreSQL releases it when the
//! transaction ends, by commit or rollback, never earlier, and there is no unlock call. Taking a
//! lock the transaction already holds succeeds at once, and a shared lock is upgraded to
//! exclusive when no other transaction holds it (PostgreSQL counts the requests of one
//! transaction separately from everyone else's).
//!
//! # Waiting
//!
//! - [`Wait::Block`] waits up to the transaction's `lock_timeout` (set when it began), then fails
//!   with `LockTimeout`; a deadlock between guards (or with row locks) is `Deadlock`.
//! - [`Wait::NoWait`] uses the `pg_try_` variant and fails at once with `LockNotAvailable`. That
//!   failure comes from the backend, not from the server, so it never aborts the PostgreSQL
//!   transaction: the caller may carry on.
//! - [`Wait::SkipLocked`] only means something for queries; here it is `InvalidQuery`.
//!
//! # The key
//!
//! The two 32-bit halves of one 64-bit hash of `namespace`, a separator and `key` ([`guard_key`]).
//! The two-integer form of advisory locks is a key space of its own, separate from the single
//! `bigint` form that `prepare` uses for its schema lock, so a guard can never block a schema
//! change.
//!
//! **Collisions.** 64 bits make an accidental collision between two of `n` live guards about
//! `n² / 2^65` (for a million live keys, about one in forty million). A collision is *safe*: it
//! makes two unrelated keys exclude each other, which costs some waiting, and it can never let
//! two holders of the same key in, because equal keys always hash equally. The hash is not
//! collision resistant against someone who chooses keys on purpose, so guard keys must be built
//! from the application's own ids, as the contract says, not from untrusted text. There is no
//! lock-name table: it would need DDL this module does not own, and PostgreSQL's
//! `pg_locks` already shows `classid` and `objid`, which [`guard_key`] reproduces for diagnosis.

use spot_storage_contract::{ErrorKind, LockKey, LockMode, StorageError, StorageResult, Wait};

use crate::PgTransaction;

/// Takes a transaction-scoped guard lock.
pub(crate) async fn lock_key(
    tx: &mut PgTransaction,
    key: &LockKey,
    mode: LockMode,
    wait: Wait,
) -> StorageResult<()> {
    if wait == Wait::SkipLocked {
        return Err(tx.fail_storage(StorageError::new(
            ErrorKind::InvalidQuery,
            "Wait::SkipLocked is only valid on queries, not on guard locks",
        )));
    }
    let (class, object) = guard_key(key);
    let shared = mode == LockMode::Shared;
    if wait == Wait::NoWait {
        let sql = if shared {
            "SELECT pg_try_advisory_xact_lock_shared($1, $2)"
        } else {
            "SELECT pg_try_advisory_xact_lock($1, $2)"
        };
        let client = tx.client()?;
        let result = client.query_one(sql, &[&class, &object]).await;
        let row = result.map_err(|e| tx.fail(&e, Some(Wait::NoWait)))?;
        if row.get::<_, bool>(0) {
            return Ok(());
        }
        return Err(tx.fail_storage(StorageError::new(
            ErrorKind::LockNotAvailable,
            format!(
                "the guard `{}`/`{}` is held by another transaction",
                key.namespace, key.key
            ),
        )));
    }
    let sql = if shared {
        "SELECT pg_advisory_xact_lock_shared($1, $2)"
    } else {
        "SELECT pg_advisory_xact_lock($1, $2)"
    };
    let client = tx.client()?;
    let result = client.execute(sql, &[&class, &object]).await;
    result.map_err(|e| tx.fail(&e, Some(Wait::Block)))?;
    Ok(())
}

/// The `(classid, objid)` pair of the advisory lock standing for `key`: the high and low halves
/// of a stable 64-bit hash (FNV-1a over `namespace`, a zero byte and `key`, then the `fmix64`
/// finaliser of MurmurHash3). Stable across versions, platforms and processes by construction:
/// guards of different processes must agree, so nothing here may depend on a random seed or on
/// the standard library's hasher.
pub(crate) fn guard_key(key: &LockKey) -> (i32, i32) {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let bytes = key
        .namespace
        .as_str()
        .bytes()
        .chain(std::iter::once(0))
        .chain(key.key.bytes());
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;
    // Reinterpreting each half as signed keeps all 64 bits; `as` is a plain bit copy here.
    #[allow(clippy::cast_possible_truncation)]
    let (class, object) = ((hash >> 32) as u32 as i32, hash as u32 as i32);
    (class, object)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashSet;

    use spot_storage_contract::Name;

    use super::*;

    fn key(namespace: &str, key: &str) -> LockKey {
        LockKey {
            namespace: Name::new(namespace).unwrap(),
            key: key.to_owned(),
        }
    }

    #[test]
    fn the_derivation_is_stable() {
        // Fixed vectors: a change here would let two releases disagree about a guard.
        let pinned = guard_key(&key("stock_balance", "item-1/wh-1"));
        assert_eq!(
            pinned,
            (PINNED.0, PINNED.1),
            "update PINNED only with a migration plan"
        );
    }

    /// The pair for `stock_balance` / `item-1/wh-1`, computed once and pinned.
    const PINNED: (i32, i32) = (-903_106_261, -1_445_256_795);

    #[test]
    fn namespace_and_key_do_not_run_together() {
        assert_ne!(guard_key(&key("ab", "c")), guard_key(&key("a", "bc")));
        assert_ne!(guard_key(&key("a", "")), guard_key(&key("a", "\0")));
    }

    #[test]
    fn many_keys_do_not_collide() {
        let mut seen = HashSet::new();
        for i in 0..200_000 {
            assert!(seen.insert(guard_key(&key(
                "stock_balance",
                &format!("item-{i}/wh-{}", i % 7)
            ))));
        }
    }
}
