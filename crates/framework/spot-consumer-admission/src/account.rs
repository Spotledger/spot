//! [`fold_account`]: the one spelling of an account name that is counted and looked up.

use spot_throttle_coordinator::MAX_VALUE_BYTES;

use crate::error::AdmissionError;

/// The single spelling of an account name an admitted attempt is counted under, and the **only**
/// spelling a caller may use for the user lookup after admission.
///
/// It trims surrounding whitespace, then lower-cases with [`str::to_lowercase`] (Unicode,
/// locale independent): the same operation the staff `spot-login` applies. The result is
/// idempotent: `fold_account(fold_account(x)?)? == fold_account(x)?`.
///
/// No other normalisation is applied or promised: no NFC/NFD composition, no dot or plus-tag
/// removal, no confusable folding. Two names that differ in any way beyond case and surrounding
/// whitespace are two accounts and two buckets.
///
/// # Errors
/// [`AdmissionError::BadRequest`] for a blank name, a name over
/// [`MAX_VALUE_BYTES`](spot_throttle_coordinator::MAX_VALUE_BYTES) bytes before or after folding
/// (lower-casing can lengthen a string), and a name holding a control character.
pub fn fold_account(account: &str) -> Result<String, AdmissionError> {
    if account.trim().is_empty() {
        return Err(AdmissionError::BadRequest {
            reason: "an account name must not be blank",
        });
    }
    if account.len() > MAX_VALUE_BYTES {
        return Err(AdmissionError::BadRequest {
            reason: "an account name is too long",
        });
    }
    let folded = account.trim().to_lowercase();
    if folded.len() > MAX_VALUE_BYTES {
        return Err(AdmissionError::BadRequest {
            reason: "an account name is too long",
        });
    }
    if folded.chars().any(char::is_control) {
        return Err(AdmissionError::BadRequest {
            reason: "an account name must not contain a control character",
        });
    }
    Ok(folded)
}
