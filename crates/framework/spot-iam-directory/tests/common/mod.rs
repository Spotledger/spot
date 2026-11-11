//! Shared builders for the integration tests.
#![allow(dead_code, missing_docs)]

use spot_events_contract::Actor;
use spot_iam_directory::{User, UserType};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

/// 2026-10-08 00:00:00 UTC plus `seconds`.
pub fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_791_417_600 + seconds)
        .unwrap_or_else(|e| panic!("timestamp: {e}"))
}

pub fn id() -> RecordId {
    RecordId::new_v7()
}

pub fn user(user_type: UserType) -> User {
    User::new(id(), user_type, "ada.lovelace", "Ada Lovelace", at(0))
}

pub fn internal() -> User {
    user(UserType::Internal)
}

pub fn admin_actor() -> Actor {
    Actor::User {
        id: id(),
        via: None,
    }
}
