//! The contract every [`CursorStore`] and [`SeenStore`] must pass (feature `testing`): the
//! in-memory stores and `spot-mail-intake-pg` run the same cases.

use spot_coordinator::FencingToken;
use spot_mail::MailboxCursor;
use time::OffsetDateTime;

use crate::ports::{CursorStore, SeenStore, StoreError};

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn cursor(uid_validity: u32, last_uid: u32) -> MailboxCursor {
    MailboxCursor {
        uid_validity,
        last_uid,
    }
}

/// Runs the cursor-store cases against `store`. `account_prefix` keeps two runs against one
/// shared database apart; it must be a storage name.
///
/// # Panics
///
/// Panics, naming the case, when the store breaks the contract.
pub async fn cursor_store_contract(store: &dyn CursorStore, account_prefix: &str) {
    let a = format!("{account_prefix}_a");
    let b = format!("{account_prefix}_b");

    assert_eq!(
        store.load(&a).await,
        Ok(None),
        "an unknown account has no cursor"
    );

    assert_eq!(
        store.store(&a, cursor(7, 10), FencingToken(2)).await,
        Ok(())
    );
    assert_eq!(
        store.load(&a).await,
        Ok(Some(cursor(7, 10))),
        "a stored cursor is loaded back"
    );
    assert_eq!(
        store.load(&b).await,
        Ok(None),
        "accounts do not share a cursor"
    );

    assert_eq!(
        store.store(&a, cursor(7, 99), FencingToken(1)).await,
        Err(StoreError::StaleFence),
        "a stale fencing token is refused"
    );
    assert_eq!(
        store.load(&a).await,
        Ok(Some(cursor(7, 10))),
        "a refused write changes nothing"
    );

    assert_eq!(
        store.store(&a, cursor(7, 11), FencingToken(2)).await,
        Ok(()),
        "the same token may write again"
    );
    assert_eq!(
        store.store(&a, cursor(8, 0), FencingToken(5)).await,
        Ok(()),
        "a newer token may write, and a new validity replaces the old"
    );
    assert_eq!(store.load(&a).await, Ok(Some(cursor(8, 0))));
    assert_eq!(
        store.store(&a, cursor(8, 3), FencingToken(4)).await,
        Err(StoreError::StaleFence),
        "once a newer token wrote, the older one stays refused"
    );
}

/// Runs the seen-store cases against `store`. `account_prefix` as in
/// [`cursor_store_contract`].
///
/// # Panics
///
/// Panics, naming the case, when the store breaks the contract.
pub async fn seen_store_contract(store: &dyn SeenStore, account_prefix: &str) {
    let a = format!("{account_prefix}_a");
    let b = format!("{account_prefix}_b");
    let digest = "ab".repeat(32);
    let other = "cd".repeat(32);

    assert_eq!(store.contains(&a, &digest).await, Ok(false));
    assert_eq!(
        store.first_time(&a, &digest, at(1_000)).await,
        Ok(true),
        "the first record is new"
    );
    assert_eq!(store.contains(&a, &digest).await, Ok(true));
    assert_eq!(
        store.first_time(&a, &digest, at(2_000)).await,
        Ok(false),
        "the second record is not"
    );
    assert_eq!(
        store.contains(&b, &digest).await,
        Ok(false),
        "accounts do not share a seen set"
    );
    assert_eq!(store.first_time(&a, &other, at(3_000)).await, Ok(true));

    // Purge removes what was recorded before the cutoff, and only that. The first record's time
    // (1_000) stays, so the second `first_time` at 2_000 did not refresh it.
    let removed = store.purge_older_than(at(1_500)).await;
    assert_eq!(removed, Ok(1), "one digest is older than the cutoff");
    assert_eq!(store.contains(&a, &digest).await, Ok(false));
    assert_eq!(store.contains(&a, &other).await, Ok(true));
}
