//! Between a [`SendReceipt`] and the row that holds it.
//!
//! The row keeps what can be written down without losing information: the message id (in the
//! key), the accepted addresses, the rejected addresses and one reply code per rejected address.
//! The description a transport gave for a rejection is not stored, so it is rebuilt as empty; see
//! the crate docs for why.

use std::str::FromStr;

use spot_mail::{Address, SendReceipt, SmtpReply};
use time::OffsetDateTime;

use crate::error::unreadable;

/// A receipt as the three array columns hold it: the addresses as text, the reply codes as
/// integers (an SMTP code is three digits; `integer` leaves no conversion that can fail).
pub(crate) struct StoredReceipt {
    /// The accepted recipients, in the transport's order.
    pub(crate) accepted: Vec<String>,
    /// The rejected recipients, in the transport's order.
    pub(crate) rejected: Vec<String>,
    /// The reply code of each rejected recipient, position for position.
    pub(crate) rejected_code: Vec<i32>,
}

impl StoredReceipt {
    /// The columns of `receipt`, ready to bind.
    pub(crate) fn of(receipt: &SendReceipt) -> Self {
        Self {
            accepted: receipt
                .accepted
                .iter()
                .map(|address| address.to_string())
                .collect(),
            rejected: receipt
                .rejected
                .iter()
                .map(|(address, _)| address.to_string())
                .collect(),
            rejected_code: receipt
                .rejected
                .iter()
                .map(|(_, reply)| i32::from(reply.code))
                .collect(),
        }
    }
}

/// Rebuilds the receipt of `message_id` from the three columns.
///
/// A stored address that no longer parses means the row was not written by this store: the
/// receipt is refused as unreadable rather than returned with an address missing, because the
/// caller's next decision is whether the message is already out.
pub(crate) fn receipt_from_row(
    message_id: &str,
    accepted: Vec<String>,
    rejected: Vec<String>,
    rejected_code: Vec<i32>,
) -> Result<SendReceipt, spot_mail_queue::Unavailable> {
    if rejected.len() != rejected_code.len() {
        return Err(unreadable());
    }
    let mut accepted_addresses = Vec::with_capacity(accepted.len());
    for value in &accepted {
        accepted_addresses.push(Address::from_str(value).map_err(|_| unreadable())?);
    }
    let mut rejected_addresses = Vec::with_capacity(rejected.len());
    for (value, code) in rejected.iter().zip(&rejected_code) {
        let address = Address::from_str(value).map_err(|_| unreadable())?;
        let code = u16::try_from(*code).map_err(|_| unreadable())?;
        rejected_addresses.push((address, SmtpReply::new(code, "")));
    }
    Ok(SendReceipt {
        message_id: message_id.to_owned(),
        accepted: accepted_addresses,
        rejected: rejected_addresses,
    })
}

/// An instant as Unix microseconds, the unit `recorded_at` holds.
///
/// # Errors
/// An instant outside the microsecond range of `i64` (about 292,000 years either side of 1970) is
/// refused rather than clamped: a cutoff that cannot be represented is a caller bug, and clamping
/// it would silently delete receipts or none at all.
pub(crate) fn micros(at: OffsetDateTime) -> Result<i64, spot_mail_queue::Unavailable> {
    let micros = at.unix_timestamp_nanos() / 1_000;
    i64::try_from(micros).map_err(|_| unreadable())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn receipt() -> SendReceipt {
        SendReceipt {
            message_id: "abc@example.test".to_owned(),
            accepted: vec!["a@customer.test".parse().unwrap()],
            rejected: vec![(
                "b@customer.test".parse().unwrap(),
                SmtpReply::new(550, "5.1.1 no such user"),
            )],
        }
    }

    #[test]
    fn a_receipt_round_trips_through_the_columns() {
        let original = receipt();
        let stored = StoredReceipt::of(&original);
        let rebuilt = receipt_from_row(
            &original.message_id,
            stored.accepted,
            stored.rejected,
            stored.rejected_code,
        )
        .unwrap();
        assert_eq!(rebuilt.message_id, original.message_id);
        assert_eq!(rebuilt.accepted, original.accepted);
        assert_eq!(rebuilt.rejected.len(), 1);
        assert_eq!(rebuilt.rejected[0].0, original.rejected[0].0);
        assert_eq!(rebuilt.rejected[0].1.code, 550);
        assert_eq!(
            rebuilt.rejected[0].1.text, "",
            "the server's reply text is not kept"
        );
    }

    #[test]
    fn an_unpaired_rejection_is_refused() {
        let refused = receipt_from_row(
            "abc@example.test",
            Vec::new(),
            vec!["b@customer.test".to_owned()],
            Vec::new(),
        );
        assert!(refused.is_err());
    }

    #[test]
    fn an_address_that_no_longer_parses_is_unreadable() {
        let refused = receipt_from_row(
            "abc@example.test",
            vec!["not an address".to_owned()],
            Vec::new(),
            Vec::new(),
        );
        assert!(refused.is_err());
    }

    #[test]
    fn an_instant_in_microseconds_is_exact() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_789).unwrap();
        assert_eq!(micros(at).unwrap(), 1_700_000_000_123_456);
    }
}
