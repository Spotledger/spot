//! Renewals: swapping in a newer licence of the same grant, offline.
//!
//! [`apply_renewal`] is the one entry point. It compares the installed licence and the
//! incoming signed-and-verified one on the three invariants the brief names:
//!
//! 1. **same licence id** — a different id is a new grant, not a renewal
//!    ([`RenewalError::IdMismatch`];
//! 2. **same holder** — the tenant or installation id must match exactly
//!    ([`RenewalError::WrongTenant`]);
//! 3. **strictly newer** — the incoming `serial` must be higher than the installed one;
//!    an equal or lower serial is a replay ([`RenewalError::Stale`]).
//!
//! The signature and the trusted keys are *not* re-checked here: the contract is that
//! the caller has already run [`crate::envelope::verify`] on the incoming envelope
//! (the function takes [`Licence`], not [`SignedLicence`](crate::envelope::SignedLicence),
//! so a caller cannot accidentally skip verification by passing raw bytes). Structural
//! problems of the incoming licence itself surface as
//! [`RenewalError::Invalid`].
//!
//! On success the caller persists the returned [`Licence`] as the installed grant and
//! re-anchors any [`crate::guard::ClockGuard`] high-water mark: the brief's rule that a
//! suspect clock behaves like grace **until a licence renewal is applied**.

use crate::error::RenewalError;
use crate::model::Licence;

/// Applies an incoming verified licence as the renewal of the installed one.
///
/// # Errors
/// [`RenewalError::IdMismatch`] when the ids differ, [`RenewalError::WrongTenant`] when
/// the holders differ, [`RenewalError::Stale`] when the incoming serial is not strictly
/// higher, [`RenewalError::Invalid`] when the incoming licence fails its own structural
/// validation.
pub fn apply_renewal(current: &Licence, incoming: &Licence) -> Result<Licence, RenewalError> {
    if incoming.licence_id != current.licence_id {
        return Err(RenewalError::IdMismatch);
    }
    let holder = |licence: &Licence| licence.tenant.clone().or(licence.installation_id.clone());
    if holder(incoming) != holder(current) {
        return Err(RenewalError::WrongTenant);
    }
    if incoming.serial <= current.serial {
        return Err(RenewalError::Stale {
            current: current.serial,
            incoming: incoming.serial,
        });
    }
    Ok(incoming.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::apply_renewal;
    use crate::error::RenewalError;
    use crate::model::{
        DEFAULT_GRACE, Entitlements, Kind, Licence, Limit, PAYLOAD_VERSION, RawLicence,
    };

    fn licence(licence_id: &str, tenant: &str, serial: u64) -> Licence {
        Licence::new(RawLicence {
            v: PAYLOAD_VERSION,
            licence_id: licence_id.to_owned(),
            tenant: Some(tenant.to_owned()),
            installation_id: None,
            edition: crate::model::test_support::edition(),
            ent: Entitlements::from_lists(Some(Limit::Unlimited), vec![], vec![]),
            kind: Kind::Subscription,
            issued_at: 1_770_000_000,
            not_before: 1_770_000_000,
            expires_at: Some(1_772_683_200),
            renew_after: None,
            grace: DEFAULT_GRACE.as_secs(),
            serial,
            key_id: "issuer-2026-q3".to_owned(),
        })
        .unwrap_or_else(|error| panic!("{error:?}"))
    }

    #[test]
    fn a_renewal_of_the_same_grant_applies() {
        let current = licence("lic-1", "acme", 1);
        let incoming = licence("lic-1", "acme", 2);
        let applied =
            apply_renewal(&current, &incoming).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(applied.serial, 2);
    }

    #[test]
    fn an_equal_or_older_serial_is_stale() {
        let current = licence("lic-1", "acme", 3);
        let replayed = licence("lic-1", "acme", 3);
        assert!(matches!(
            apply_renewal(&current, &replayed),
            Err(RenewalError::Stale {
                current: 3,
                incoming: 3
            })
        ));
        let older = licence("lic-1", "acme", 2);
        assert!(matches!(
            apply_renewal(&current, &older),
            Err(RenewalError::Stale {
                current: 3,
                incoming: 2
            })
        ));
    }

    #[test]
    fn another_tenant_is_refused() {
        let current = licence("lic-1", "acme", 1);
        let incoming = licence("lic-1", "globex", 2);
        assert_eq!(
            apply_renewal(&current, &incoming).unwrap_err().code(),
            "licence.wrong_tenant"
        );
    }

    #[test]
    fn another_licence_id_is_a_new_grant() {
        let current = licence("lic-1", "acme", 1);
        let incoming = licence("lic-2", "acme", 2);
        assert_eq!(
            apply_renewal(&current, &incoming).unwrap_err().code(),
            "licence.licence_id_mismatch"
        );
    }
}
