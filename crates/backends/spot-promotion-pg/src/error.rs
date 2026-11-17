//! Errors: the trait crate's own [`PromotionError`], with a fixed sentence and only a code (the
//! bridge's or the database's SQLSTATE) as the variable part.

use spot_pg_bridge::BridgeError;
use spot_promotion::PromotionError;

/// What a failure says without any value: the bridge code, or the SQLSTATE of the database.
pub(crate) fn cause(error: &BridgeError<tokio_postgres::Error>) -> String {
    match error {
        BridgeError::Store(e) => e.code().map_or_else(
            || "the database call failed".to_owned(),
            |state| format!("SQLSTATE {}", state.code()),
        ),
        other => other
            .code()
            .map_or_else(|| "the call failed".to_owned(), str::to_owned),
    }
}

/// `promotion.store_failed` for a bridge refusal or a database error.
pub(crate) fn store_failed(error: &BridgeError<tokio_postgres::Error>) -> PromotionError {
    PromotionError::StoreFailed {
        message: format!(
            "the promotion store could not be read or written ({})",
            cause(error)
        ),
    }
}

/// `promotion.store_failed` for a store-side limit or a setup refusal.
pub(crate) fn store_note(message: &str) -> PromotionError {
    PromotionError::StoreFailed {
        message: message.to_owned(),
    }
}

/// `promotion_pg.snapshot_invalid`: a stored row is not a valid promotion. Names no content.
pub(crate) fn row_invalid(message: &str) -> PromotionError {
    PromotionError::SnapshotInvalid {
        message: message.to_owned(),
    }
}
