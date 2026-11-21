//! The licence model: the canonical, versioned payload every other module works on.
//!
//! A [`Licence`] is data, not behaviour. Nothing here interprets which edition enables
//! which feature — editions and entitlement names are raw strings ([`Edition`]) by
//! design, so the control plane can mint licences for new editions without touching this
//! crate. The state machine ([`crate::state`]) reads only the time fields and the
//! [`Licence::kind`]; the entitlement check reads only [`Entitlements`].
//!
//! # Canonical form
//!
//! A licence round-trips as JSON with a **fixed field order and no whitespace**
//! ([`Licence::to_canonical_json`] / [`Licence::from_json`]): that exact byte sequence is
//! what is signed ([`crate::envelope`]), and a re-serialisation must reproduce it, which
//! is what verification's canonicity check asserts. The order is the one of
//! [`RawLicence`]'s fields, `v` first.
//!
//! # Time fields
//!
//! All instants are UTC seconds since the Unix epoch (no fractions): JSON-safe, stable
//! over the wire, and a [`spot_core::Clock`] moves them to and from
//! [`time::OffsetDateTime`]. `not_before`, `expires_at` and `renew_after` point at
//! absolute instants; `grace` is a duration in seconds. The lifecycle meanings live in
//! the crate docs' state table.
//!
//! # Serial
//!
//! [`Licence::serial`] is the licence's per-`licence_id` version counter: 1 for the
//! first issued licence and strictly increasing after that
//! ([`crate::renewal`](crate::renewal::apply_renewal) refuses anything that is not
//! strictly newer). A re-presented older licence is a replay, detectable by the serial
//! alone, with no storage needed on the consumer side.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::LicenceError;

/// Major version of the licence payload format that this crate mints and understands.
/// Anything else is [`LicenceError::UnknownVersion`] at the envelope layer.
pub const PAYLOAD_VERSION: u32 = 1;

/// The grace period a subscription gets when the issuer left the field at zero.
///
/// Issuers normally set `grace` explicitly; this default exists so the acceptance
/// criterion holds even for terse issuers: *a desktop install activates and renews
/// offline within the grace period*, with seven days to do it in.
///
/// Only [`Kind::Subscription`] grants a grace period. Trials are strictly over at
/// expiry ([`Kind::Trial`]); a perpetual does not expire.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The longest grace period the format mints, 90 days.
///
/// A grace period is an escrow: something the platform grants after expiry, not
/// something a licence can demand, and normal operation renews on a scale of days.
pub const MAX_GRACE: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// The latest instant the format expresses. Year 2230, 64-bit seconds on purpose: a
/// licence is a decade-scale document, and 32-bit epoch seconds run out in 2038.
pub(crate) const MAX_INSTANT_SECONDS: i64 = 8_210_266_800;

/// The longest `licence_id`, `installation_id`, `tenant` or `issuer_key_id` string, 128
/// bytes; identifiers are short on purpose (they appear in logs, file names and URLs).
pub(crate) const MAX_ID_BYTES: usize = 128;

/// The longest edition string, 64 bytes.
pub(crate) const MAX_EDITION_BYTES: usize = 64;

/// One licence: the signed payload — edition, entitlements, validity times, kind, serial.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Licence {
    /// The payload format version; only [`PAYLOAD_VERSION`] is accepted.
    pub version: u32,
    /// The stable identifier the grant travels under, forever: renewals of the same
    /// licence differ only in [`Licence::serial`]. A UUID string is normal; this crate
    /// requires 1..=[`MAX_ID_BYTES`] non-control bytes.
    pub licence_id: String,
    /// Whose licence this is: a cloud tenant (a site id). Set exactly when
    /// [`Licence::installation_id`] is not.
    pub tenant: Option<String>,
    /// Whose licence this is: one desktop or standalone install. Set exactly when
    /// [`Licence::tenant`] is not.
    pub installation_id: Option<String>,
    /// The edition as the issuer minted it. Raw data on purpose, see [`Edition`].
    pub edition: Edition,
    /// The named limits and flags this licence grants.
    pub entitlements: Entitlements,
    /// What kind of grant this is; trials get the strictest end of life.
    pub kind: Kind,
    /// The instant the issuer signed this payload, UTC seconds since the Unix epoch.
    /// Verification requires the signing key's window to cover it.
    pub issued_at: i64,
    /// The first instant this payload is valid (the activation window start), UTC
    /// seconds; may precede `issued_at` when the issuer pre-signs a scheduled start.
    /// [`Status::NotYetValid`](crate::state::Status::NotYetValid) applies before it.
    pub not_before: i64,
    /// The last instant the licence grants full use, UTC seconds; grace counts from the
    /// second after it. `None` only for [`Kind::Perpetual`].
    pub expires_at: Option<i64>,
    /// The instant the licence asks to be renewed: [`Status::RenewalDue`](crate::state::Status::RenewalDue)
    /// applies at and after it while the licence is still valid.
    pub renew_after: Option<i64>,
    /// How long after `expires_at` the install keeps full function with a visible
    /// warning ([`Status::Grace`](crate::state::Status::Grace)) before dropping to
    /// read-only. Zero for trials: they expire into
    /// [`Status::Expired`](crate::state::Status::Expired) with no grace.
    pub grace: Duration,
    /// The monotonic sequence of this grant within its `licence_id`, starting at 1;
    /// renewals must strictly increase it.
    pub serial: u64,
    /// Which trusted key the issuer signed with; the envelope carries the same id.
    pub issuer_key_id: String,
}

impl Licence {
    /// Validates and builds a licence from its raw fields.
    ///
    /// The single funnel for constructing a [`Licence`]: [`Licence::from_json`], the
    /// `issue` feature and any hand-built licence go through it, so every instance
    /// upholds the invariants below.
    ///
    /// # Errors
    /// The structural errors of [`LicenceError`]: the format version, the id lengths,
    /// exactly one holder, the serial, the instant ranges and the consistency of the
    /// validity window with the kind.
    pub fn new(raw: RawLicence) -> Result<Self, LicenceError> {
        if raw.v != PAYLOAD_VERSION {
            return Err(LicenceError::UnknownVersion { version: raw.v });
        }
        check_text(&raw.licence_id, "licence_id").map_err(|_| LicenceError::LicenceIdInvalid)?;
        match (raw.tenant.as_deref(), raw.installation_id.as_deref()) {
            (Some(tenant), None) => {
                check_text(tenant, "tenant").map_err(|_| LicenceError::HolderInvalid)?;
            }
            (None, Some(installation)) => {
                check_text(installation, "installation_id")
                    .map_err(|_| LicenceError::HolderInvalid)?;
            }
            _ => return Err(LicenceError::HolderInvalid),
        }
        check_text(&raw.key_id, "issuer_key_id").map_err(|_| LicenceError::KeyIdInvalid)?;
        if raw.serial == 0 {
            return Err(LicenceError::SerialZero);
        }
        for instant in [
            raw.issued_at,
            raw.not_before,
            raw.expires_at.unwrap_or_default(),
            raw.renew_after.unwrap_or_default(),
        ] {
            // The `unwrap_or_default` placeholders are filtered by the `is_some` checks
            // around each field's own range check below; this loop only rejects the
            // negative instants that every field shares.
            if instant < 0 {
                return Err(LicenceError::InstantRange);
            }
        }
        for (set, instant) in [
            (raw.expires_at.is_some(), raw.expires_at.unwrap_or_default()),
            (
                raw.renew_after.is_some(),
                raw.renew_after.unwrap_or_default(),
            ),
            (true, raw.not_before),
        ] {
            if set && instant > MAX_INSTANT_SECONDS {
                return Err(LicenceError::InstantRange);
            }
        }
        if raw
            .expires_at
            .is_some_and(|expires_at| expires_at < raw.not_before)
        {
            return Err(LicenceError::WindowInvalid);
        }
        if raw
            .renew_after
            .is_some_and(|renew_after| renew_after > raw.expires_at.unwrap_or(MAX_INSTANT_SECONDS))
        {
            return Err(LicenceError::WindowInvalid);
        }
        if raw
            .renew_after
            .is_some_and(|renew_after| renew_after < raw.not_before)
        {
            return Err(LicenceError::WindowInvalid);
        }
        if raw.grace > MAX_GRACE.as_secs() {
            return Err(LicenceError::GraceTooLong {
                maximum_days: u32::try_from(MAX_GRACE.as_secs() / 86_400).unwrap_or(u32::MAX),
            });
        }
        let grace = Duration::from_secs(raw.grace);
        match raw.kind {
            Kind::Trial => {
                if raw.expires_at.is_none() {
                    return Err(LicenceError::KindInvalid);
                }
                if raw.grace != 0 {
                    return Err(LicenceError::KindInvalid);
                }
            }
            Kind::Subscription => {
                if raw.expires_at.is_none() {
                    return Err(LicenceError::KindInvalid);
                }
            }
            Kind::Perpetual => {
                if raw.expires_at.is_some() {
                    return Err(LicenceError::KindInvalid);
                }
            }
        }
        Ok(Self {
            version: raw.v,
            licence_id: raw.licence_id,
            tenant: raw.tenant,
            installation_id: raw.installation_id,
            edition: raw.edition,
            entitlements: raw.ent,
            kind: raw.kind,
            issued_at: raw.issued_at,
            not_before: raw.not_before,
            expires_at: raw.expires_at,
            renew_after: raw.renew_after,
            grace,
            serial: raw.serial,
            issuer_key_id: raw.key_id,
        })
    }

    /// The canonical JSON form of the licence: the exact bytes that are signed.
    ///
    /// # Errors
    /// [`LicenceError::Json`] when serde cannot produce them, which for a validated
    /// licence means a bug in this crate, not bad input.
    pub fn to_canonical_json(&self) -> Result<String, LicenceError> {
        serde_json::to_string(self).map_err(|error| LicenceError::Json {
            message: error.to_string(),
        })
    }

    /// Serialises into canonical bytes, the exact bytes that are signed.
    ///
    /// # Errors
    /// [`LicenceError::Json`] as for [`Licence::to_canonical_json`].
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, LicenceError> {
        self.to_canonical_json().map(String::into_bytes)
    }

    /// Parses the canonical JSON produced by [`Licence::to_canonical_json`].
    ///
    /// The strict, structural canonicity of the text (fixed field order, no whitespace,
    /// no duplicate keys, no odd number spellings) is **not** enforced here — that is
    /// [`crate::envelope::verify`]'s job, where a *signature* stands behind the bytes.
    /// This constructor parses any well-formed JSON object that deserialises as a
    /// [`Licence`], so tooling can read hand-edited files too.
    ///
    /// # Errors
    /// [`LicenceError::Json`] when serde cannot parse it into a [`Licence`]; every
    /// validation rule of [`Licence::new`] applies to the parsed fields.
    pub fn from_json(text: &str) -> Result<Self, LicenceError> {
        serde_json::from_str(text).map_err(|error| LicenceError::Json {
            message: error.to_string(),
        })
    }
}

/// The fields of a [`Licence`] as a plain struct, the one serde boundary of the format.
/// Renaming anything here is a breaking format change and takes a `version` bump.
///
/// Serde ordering of serde-derive structs is declaration order: this order **is** the
/// canonical field order, `v` first, `key_id` last.
#[derive(Clone, Serialize, Deserialize)]
pub struct RawLicence {
    /// The format version.
    pub v: u32,
    /// The stable identifier the grant travels under.
    pub licence_id: String,
    /// Which cloud tenant holds the licence; `null` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Which install holds the licence; `null` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    /// The edition string.
    pub edition: Edition,
    /// The entitlements object.
    pub ent: Entitlements,
    /// The kind.
    pub kind: Kind,
    /// Issuance instant, UTC seconds.
    pub issued_at: i64,
    /// Validity start, UTC seconds.
    pub not_before: i64,
    /// Expiry instant, UTC seconds; absent for a perpetual licence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Renewal prompt point, UTC seconds; absent when never asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renew_after: Option<i64>,
    /// Grace seconds, absent when zero.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub grace: u64,
    /// The monotonic serial of this grant within its `licence_id`.
    pub serial: u64,
    /// The issuer's key id.
    pub key_id: String,
}

/// Serde helper for omitting a zero grace from the canonical form.
fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// The edition as raw, issuer-controlled data.
///
/// Deliberately not an enum: the blueprint calls editions **data** (`starter`, `business`,
/// `enterprise` are values, not variants), so the control plane can add a `scale` edition
/// without a release of this crate. This crate never decides what an edition unlocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edition(String);

impl Edition {
    /// Wraps an edition string after a shape check: 1..=[`MAX_EDITION_BYTES`] bytes,
    /// ASCII letters, digits, `_` and `-`, starting with a letter. Comparison is exact
    /// (`business` and `Business` are different editions).
    ///
    /// # Errors
    /// [`LicenceError::EditionInvalid`] when the shape is wrong.
    pub fn new(name: impl Into<String>) -> Result<Self, LicenceError> {
        let name = name.into();
        let bytes = name.as_bytes();
        let well_formed = !name.is_empty()
            && name.len() <= MAX_EDITION_BYTES
            && bytes[0].is_ascii_alphabetic()
            && bytes[1..]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-');
        if !well_formed {
            return Err(LicenceError::EditionInvalid);
        }
        Ok(Self(name))
    }

    /// The edition string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Edition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A string in canonical JSON, validated on the way in.
impl Serialize for Edition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Edition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        Edition::new(text).map_err(serde::de::Error::custom)
    }
}

/// Whether a named limit and flag set is granted.
///
/// Names are data, like editions: `users` is typed ([`Limit`]), `apps` and `features`
/// are plain name lists, and uninterpreted extras ride along in
/// [`Entitlements::extra`] so the vocabulary can grow ahead of this crate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entitlements {
    /// The named user limit, when the issuer set one; see
    /// [`Entitlements::users`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Limit>,
    /// The app ids this licence may install and run, sorted; an empty list leaves the
    /// question open (consumer's call, typically unrestricted for the platform's own
    /// apps and decided per app elsewhere).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    /// The feature flags this licence turns on, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Extra named entitlements — limits, flags and names — that this generation of the
    /// platform does not interpret. Kept verbatim, sorted by name, so an issuer can
    /// extend the vocabulary without a coordinated release of this crate.
    #[serde(flatten)]
    pub extra: BTreeMap<String, ExtraEntitlement>,
}

impl Entitlements {
    /// Builds entitlements granting nothing: no user limit, no apps, no features.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Builds entitlements with a user limit and app and feature lists.
    #[must_use]
    pub fn from_lists(users: Option<Limit>, apps: Vec<String>, features: Vec<String>) -> Self {
        Self {
            users,
            apps,
            features,
            extra: BTreeMap::new(),
        }
    }

    /// The user limit, [`Limit::Unlimited`] by default.
    #[must_use]
    pub fn users(&self) -> Limit {
        self.users.unwrap_or(Limit::Unlimited)
    }

    /// The app ids.
    #[must_use]
    pub fn apps(&self) -> &[String] {
        &self.apps
    }

    /// The feature flags.
    #[must_use]
    pub fn features(&self) -> &[String] {
        &self.features
    }
}

/// A named limit: bounded by a count or effectively unlimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// Up to this many.
    UpTo(u32),
    /// No bound.
    Unlimited,
}

impl Limit {
    /// The bounded count, with [`Limit::Unlimited`] reported as `u64::MAX` for callers
    /// that must show a number (a seat-count column).
    #[must_use]
    pub fn or_unlimited(self) -> u64 {
        match self {
            Self::UpTo(bound) => u64::from(bound),
            Self::Unlimited => u64::MAX,
        }
    }
}

/// The canonical JSON rendering of a [`Limit`]: a bare number, or the string
/// `"unlimited"`.
impl Serialize for Limit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::UpTo(bound) => serializer.serialize_u32(*bound),
            Self::Unlimited => serializer.serialize_str("unlimited"),
        }
    }
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct LimitVisitor;

        impl<'de> serde::de::Visitor<'de> for LimitVisitor {
            type Value = Limit;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a non-negative count or the string \"unlimited\"")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == 0 {
                    return Err(serde::de::Error::custom(
                        "a limit of 0 grants nothing; omit it",
                    ));
                }
                u32::try_from(value)
                    .map(Limit::UpTo)
                    .map_err(|_| serde::de::Error::custom("a limit is at most u32::MAX"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == "unlimited" {
                    Ok(Limit::Unlimited)
                } else {
                    Err(serde::de::Error::custom(
                        "a limit is a count or the string \"unlimited\"",
                    ))
                }
            }
        }

        deserializer.deserialize_any(LimitVisitor)
    }
}

/// An extra entitlement value: a number, a flag or a name — the raw shapes an issuer may
/// mint under a name this crate does not interpret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtraEntitlement {
    /// A count or size, as a raw JSON number.
    Number(u64),
    /// An on/off flag.
    Flag(bool),
    /// A name or tag.
    Name(String),
}

/// Whether the licence is a `trial`, a recurring `subscription` or a one-time
/// `perpetual` grant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// Time-boxed trial: strictly over at `expires_at`, **no** grace
    /// ([`Status::Expired`](crate::state::Status::Expired)).
    #[default]
    Trial,
    /// Recurring grant: `expires_at` plus a grace period; full function through grace
    /// with a warning, then read-only ([`Status::Restricted`](crate::state::Status::Restricted))
    /// until the next renewal lands.
    Subscription,
    /// One-time purchase: no `expires_at`, never expires on its own; `renew_after`, when
    /// set, asks for an update window rather than survival.
    Perpetual,
}

impl Kind {
    /// Whether this kind grants a grace period after [`Licence::expires_at`]: yes for
    /// subscriptions, no for trials (they expire hard) and for perpetuals (nothing to be
    /// graceful about).
    #[must_use]
    pub fn grants_grace(self) -> bool {
        matches!(self, Self::Subscription)
    }
}

/// The canonical JSON spelling of a [`Licence`]: the fixed field order of
/// [`RawLicence`], optional fields omitted when unset, no whitespace.
impl Serialize for Licence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        RawLicence {
            v: self.version,
            licence_id: self.licence_id.clone(),
            tenant: self.tenant.clone(),
            installation_id: self.installation_id.clone(),
            edition: self.edition.clone(),
            ent: self.entitlements.clone(),
            kind: self.kind,
            issued_at: self.issued_at,
            not_before: self.not_before,
            expires_at: self.expires_at,
            renew_after: self.renew_after,
            grace: self.grace.as_secs(),
            serial: self.serial,
            key_id: self.issuer_key_id.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Licence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawLicence::deserialize(deserializer)?;
        Licence::new(raw).map_err(serde::de::Error::custom)
    }
}

/// The accepted shape of an identifier string: 1..=`max` bytes, every byte one of ASCII
/// letters, digits, `-`, `_` and `.`, no leading or trailing punctuation.
pub(crate) fn check_text(text: &str, what: &str) -> Result<(), &'static str> {
    let _ = what;
    let well_formed = !text.is_empty()
        && text.len() <= MAX_ID_BYTES
        && !text.starts_with(['-', '_', '.'])
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if well_formed {
        Ok(())
    } else {
        Err("identifier is empty, too long, or has a byte outside the alphabet")
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Test-only fixtures shared across the crate's unit-test modules.

    use super::Edition;

    /// A fixed test edition, the same string everywhere.
    pub(crate) const EDITION: &str = "business";

    /// The test edition, through the valid path; the string is a module constant, so a
    /// failure here is a bug in the model's own rules, not in a test.
    pub(crate) fn edition() -> Edition {
        match Edition::new(EDITION) {
            Ok(edition) => edition,
            Err(error) => panic!("the fixed test edition must parse: {error:?}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        DEFAULT_GRACE, Entitlements, Kind, Licence, Limit, MAX_GRACE, MAX_INSTANT_SECONDS,
        PAYLOAD_VERSION, RawLicence,
    };
    use crate::model::test_support::edition;

    /// A licence that passes every rule: a subscription for one site, one serial in.
    fn sample() -> RawLicence {
        RawLicence {
            v: PAYLOAD_VERSION,
            licence_id: "0198c7a2-license-id".to_owned(),
            tenant: Some("acme".to_owned()),
            installation_id: None,
            edition: edition(),
            ent: Entitlements::from_lists(
                Some(Limit::UpTo(25)),
                vec!["bookkeeping".to_owned()],
                vec!["api".to_owned()],
            ),
            kind: Kind::Subscription,
            issued_at: 1_770_000_000,
            not_before: 1_770_000_000,
            expires_at: Some(1_772_683_200),  // 31 days after issue
            renew_after: Some(1_772_071_200), // 24 days after issue
            grace: DEFAULT_GRACE.as_secs(),
            serial: 1,
            key_id: "issuer-2026-q3".to_owned(),
        }
    }

    #[test]
    fn sample_satisfies_every_rule() {
        let licence = Licence::new(sample());
        assert!(licence.is_ok(), "{licence:?}");
    }

    #[test]
    fn every_kind_has_its_own_rules() {
        let trial = RawLicence {
            grace: 0,
            kind: Kind::Trial,
            ..sample()
        };
        assert!(Licence::new(trial.clone()).is_ok());
        let trial_no_expiry = RawLicence {
            expires_at: None,
            renew_after: None,
            ..trial
        };
        assert_eq!(
            Licence::new(trial_no_expiry).unwrap_err().code(),
            "licence.kind_invalid"
        );
        let perpetual = RawLicence {
            expires_at: None,
            renew_after: None,
            grace: 0,
            kind: Kind::Perpetual,
            ..sample()
        };
        assert!(Licence::new(perpetual).is_ok());
    }

    #[test]
    fn exactly_one_holder() {
        let installation = RawLicence {
            tenant: None,
            installation_id: Some("install-7f3a".to_owned()),
            ..sample()
        };
        assert!(Licence::new(installation).is_ok());
        let both = RawLicence {
            installation_id: Some("install-1".to_owned()),
            ..sample()
        };
        assert_eq!(
            Licence::new(both).unwrap_err().code(),
            "licence.holder_invalid"
        );
        let neither = RawLicence {
            tenant: None,
            ..sample()
        };
        assert_eq!(
            Licence::new(neither).unwrap_err().code(),
            "licence.holder_invalid"
        );
    }

    #[test]
    fn window_must_be_consistent() {
        let before_expiry = RawLicence {
            not_before: 1_773_000_000, // after the sample expiry
            ..sample()
        };
        assert_eq!(
            Licence::new(before_expiry).unwrap_err().code(),
            "licence.window_invalid"
        );
        let renew_after_expiry = RawLicence {
            renew_after: Some(1_773_000_000), // after the sample expiry
            ..sample()
        };
        assert_eq!(
            Licence::new(renew_after_expiry).unwrap_err().code(),
            "licence.window_invalid"
        );
        let negative = RawLicence {
            issued_at: -1,
            ..sample()
        };
        assert_eq!(
            Licence::new(negative).unwrap_err().code(),
            "licence.instant_range"
        );
        let beyond_range = RawLicence {
            not_before: MAX_INSTANT_SECONDS + 1,
            ..sample()
        };
        assert_eq!(
            Licence::new(beyond_range).unwrap_err().code(),
            "licence.instant_range"
        );
    }

    #[test]
    fn grace_is_capped() {
        let exactly = Licence::new(RawLicence {
            grace: MAX_GRACE.as_secs(),
            ..sample()
        });
        assert!(exactly.is_ok(), "{exactly:?}");
        let over = Licence::new(RawLicence {
            grace: MAX_GRACE.as_secs() + 1,
            ..sample()
        });
        assert_eq!(over.unwrap_err().code(), "licence.grace_too_long");
    }

    #[test]
    fn serials_start_at_one() {
        let zero = Licence::new(RawLicence {
            serial: 0,
            ..sample()
        });
        assert_eq!(zero.unwrap_err().code(), "licence.serial_zero");
    }

    #[test]
    fn canonical_round_trip_is_verbatim() {
        let licence = Licence::new(sample()).unwrap_or_else(|error| panic!("{error:?}"));
        let canonical = licence
            .to_canonical_json()
            .unwrap_or_else(|error| panic!("{error:?}"));
        let parsed = Licence::from_json(&canonical).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(licence, parsed);
        // A second round trip is a fixed point.
        let again = parsed
            .to_canonical_json()
            .unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(canonical, again);
    }

    #[test]
    fn canonical_form_sets_the_field_order() {
        let licence = Licence::new(sample()).unwrap_or_else(|error| panic!("{error:?}"));
        let canonical = licence
            .to_canonical_json()
            .unwrap_or_else(|error| panic!("{error:?}"));
        let names: Vec<String> = canonical
            .trim_start_matches('{')
            .split(',')
            .map(|member| {
                member
                    .split_once(':')
                    .map_or_else(|| member.to_owned(), |(key, _)| key.to_owned())
            })
            .map(|key| key.trim_matches('"').to_owned())
            .collect();
        assert!(
            names.starts_with(&["v".to_owned(), "licence_id".to_owned()]),
            "{canonical}"
        );
    }
}
