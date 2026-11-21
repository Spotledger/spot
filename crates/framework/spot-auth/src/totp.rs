//! Time-based one-time passwords (RFC 6238) with Base32 secrets.
//!
//! The user enrols by scanning the provisioning URI built by
//! [`TotpSecret::provisioning_uri`]; codes are then verified with a small, explicit skew
//! window and a caller-maintained last-used step, so replays are refused.

use std::fmt;

use hmac::{Hmac, Mac};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use sha1::Sha1;
use sha2::{Sha256, Sha512};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

use spot_core::Clock;

use crate::error::AuthError;

/// The step size the RFC 6238 test vectors use, and the platform's configuration: 30 seconds.
pub const DEFAULT_STEP_SECONDS: u32 = 30;

/// The RFC-recommended default window: one accepted step in either direction.
pub const DEFAULT_SKEW_STEPS: u32 = 1;

/// The shortest binary secret this crate accepts, 16 bytes.
pub const SECRET_MIN_LENGTH: usize = 16;

/// The longest binary secret this crate accepts, 1,024 bytes.
pub const SECRET_MAX_LENGTH: usize = 1024;

// Provisioning URI labels escape everything outside RFC 3986 "unreserved" characters:
// controls, spaces and every delimiter that could confuse an authenticator's parser.
const LABEL_SAFE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'=')
    .add(b',')
    .add(b'!');

/// The hash the authenticator app and this crate agree to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TotpAlgorithm {
    /// HMAC-SHA1: the authenticator-app default.
    #[default]
    Sha1,
    /// HMAC-SHA256, per RFC 6238.
    Sha256,
    /// HMAC-SHA512, per RFC 6238.
    Sha512,
}

impl TotpAlgorithm {
    // RFC 6238 requires the secret to be at least as long as the HMAC output.
    fn digest_length(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha512 => 64,
        }
    }
}

/// The digit width of a one-time code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotpDigits {
    /// Six digits, the common case.
    Six,
    /// Eight digits, the stricter variant.
    Eight,
}

impl TotpDigits {
    /// The width as a number, for the provisioning URI and code formatting.
    #[must_use]
    pub fn count(self) -> u32 {
        match self {
            Self::Six => 6,
            Self::Eight => 8,
        }
    }
}

/// Parameters of the drift and formatting of a one-time code check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TotpConfig {
    /// Which hash to use; must match the enroling authenticator.
    pub algorithm: TotpAlgorithm,
    /// Digit width; must match the enroling authenticator.
    pub digits: TotpDigits,
    /// Step size in seconds; RFC 6238 default 30, see [`DEFAULT_STEP_SECONDS`].
    pub step_seconds: u32,
    /// Accepted drift in whole steps in either direction, usually [`DEFAULT_SKEW_STEPS`].
    pub skew_steps: u32,
}

/// The binary secret of a TOTP enrolment.
///
/// Generated at enrolment and shown to the user as Base32 ([`TotpSecret::to_base32`]) to
/// scan or type into an authenticator app. Redacted in `Debug`, zeroized on drop, no
/// `Display`.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct TotpSecret {
    bytes: Vec<u8>,
}

impl TotpSecret {
    /// A fresh 20-byte random secret: the size of the authenticator default (SHA-1) and of
    /// the RFC 6238 SHA-1 test vectors.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = vec![0u8; 20];
        crate::util::fill_os_random(&mut bytes);
        Self { bytes }
    }

    /// Wraps a binary secret, accepting [`SECRET_MIN_LENGTH`] through [`SECRET_MAX_LENGTH`]
    /// bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, AuthError> {
        if !(SECRET_MIN_LENGTH..=SECRET_MAX_LENGTH).contains(&bytes.len()) {
            return Err(AuthError::SecretLength);
        }
        Ok(Self { bytes })
    }

    /// Parses a Base32 secret produced by [`TotpSecret::to_base32`]; lower-case letters are
    /// accepted because authenticator apps may echo the secret either way.
    pub fn from_base32(text: &str) -> Result<Self, AuthError> {
        let bytes = base32_decode(text)?;
        Self::from_bytes(bytes)
    }

    /// The Base32 text (RFC 4648, upper case, no padding) the user enrols with.
    #[must_use]
    pub fn to_base32(&self) -> String {
        base32_encode(&self.bytes)
    }

    /// Builds an `otpauth://` URI for QR enrolment. The issuer and account labels are
    /// escaped per RFC 3986: authenticator apps are strict about delimiters.
    #[must_use]
    pub fn provisioning_uri(&self, config: &TotpConfig, issuer: &str, account: &str) -> String {
        let label = format!(
            "{}%3A{}",
            utf8_percent_encode(issuer, LABEL_SAFE),
            utf8_percent_encode(account, LABEL_SAFE)
        );
        let algorithm = match config.algorithm {
            TotpAlgorithm::Sha1 => "SHA1",
            TotpAlgorithm::Sha256 => "SHA256",
            TotpAlgorithm::Sha512 => "SHA512",
        };
        format!(
            "otpauth://totp/{label}?issuer={}&secret={}&algorithm={algorithm}&digits={}&period={}",
            utf8_percent_encode(issuer, LABEL_SAFE),
            self.to_base32(),
            config.digits.count(),
            config.step_seconds,
        )
    }

    // The formatted code for an explicit step; tests and the verify loop use this.
    fn code_at(&self, config: &TotpConfig, step: u64) -> Result<String, AuthError> {
        code_for(config, &self.bytes, step)
    }
}

impl fmt::Debug for TotpSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

// HMAC, dynamic truncation and digit formatting per RFC 4226 §5.3.
fn code_for(config: &TotpConfig, secret: &[u8], step: u64) -> Result<String, AuthError> {
    // RFC 6238 requires the secret to be at least as long as the HMAC output size.
    if secret.len() < config.algorithm.digest_length() {
        return Err(AuthError::SecretLength);
    }

    // The counter is a 64-bit big-endian step number, per the RFC.
    let counter = step.to_be_bytes();
    let digest = match config.algorithm {
        TotpAlgorithm::Sha1 => {
            let mut mac =
                Hmac::<Sha1>::new_from_slice(secret).map_err(|_| AuthError::SecretLength)?;
            mac.update(&counter);
            mac.finalize().into_bytes().to_vec()
        }
        TotpAlgorithm::Sha256 => {
            let mut mac =
                Hmac::<Sha256>::new_from_slice(secret).map_err(|_| AuthError::SecretLength)?;
            mac.update(&counter);
            mac.finalize().into_bytes().to_vec()
        }
        TotpAlgorithm::Sha512 => {
            let mut mac =
                Hmac::<Sha512>::new_from_slice(secret).map_err(|_| AuthError::SecretLength)?;
            mac.update(&counter);
            mac.finalize().into_bytes().to_vec()
        }
    };

    // Dynamic truncation per RFC 4226 §5.3: the low nibble of the last octet picks an
    // offset into the digest; the four octets from there mask to 31 bits.
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);

    let modulus = modulus_of(config.digits).ok_or(AuthError::CryptoInternal)?;
    Ok(format!(
        "{:0digits$}",
        binary % modulus,
        digits = usize::try_from(config.digits.count()).unwrap_or(0)
    ))
}

// The numeric base of a code: 10^6 or 10^8, both of which fit a u32.
fn modulus_of(digits: TotpDigits) -> Option<u32> {
    match digits {
        TotpDigits::Six => Some(1_000_000),
        TotpDigits::Eight => Some(100_000_000),
    }
}

impl TotpConfig {
    /// Verifies a code typed by the user against the steps allowed by the skew window,
    /// newest match first.
    ///
    /// Returns the matched step, so the caller can record it: pass it back next time as
    /// `last_used_step`, and any candidate step at or before that value is refused with
    /// [`AuthError::TotpReplayed`], whether or not it ever matched. Impossible shapes
    /// (wrong length, non-digits) return [`AuthError::TotpCodeInvalid`] before any
    /// cryptographic work; a well-formed code matching no window step is also
    /// [`AuthError::TotpCodeInvalid`].
    pub fn verify(
        &self,
        secret: &TotpSecret,
        code: &str,
        clock: &dyn Clock,
        last_used_step: Option<u64>,
    ) -> Result<u64, AuthError> {
        let width = usize::try_from(self.digits.count()).map_err(|_| AuthError::CryptoInternal)?;
        if code.len() != width || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AuthError::TotpCodeInvalid);
        }

        let now = current_step(self.step_seconds, clock.now_utc().unix_timestamp());
        // The window: `skew_steps` on either side, capped so the check always costs a
        // bounded number of HMACs no matter what a caller configures.
        let skew = u64::from(self.skew_steps.min(60));
        let mut matched: Option<u64> = None;
        for step in now.saturating_sub(skew)..=(now + skew) {
            if constant_time_eq(&secret.code_at(self, step)?.into_bytes(), code.as_bytes()) {
                matched = Some(step);
                break;
            }
        }
        let Some(matched) = matched else {
            return Err(AuthError::TotpCodeInvalid);
        };
        if matches!(last_used_step, Some(used) if matched <= used) {
            return Err(AuthError::TotpReplayed);
        }
        Ok(matched)
    }
}

// Splits a unix timestamp into whole steps; a zero step width behaves as one second,
// because a zero-width step would accept a code forever.
fn current_step(step_seconds: u32, unix_seconds: i64) -> u64 {
    if unix_seconds <= 0 {
        return 0;
    }
    let width = u64::from(step_seconds).max(1);
    u64::try_from(unix_seconds).unwrap_or(0) / width
}

// Byte comparison in constant time given equal lengths; lengths here are not secret: both
// sides are digit strings of the configured width, shape-checked before this is called.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

// The RFC 4648 Base32 alphabet, in order.
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Encodes RFC 4648 Base32, upper case, without padding, as authenticator apps read it.
#[must_use]
pub fn base32_encode(data: &[u8]) -> String {
    let mut text = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            text.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        // The final partial group is zero-padded to a full symbol, per RFC 4648.
        text.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    text
}

/// Decodes RFC 4648 Base32, with or without `=` padding, upper or lower case; anything not
/// produced by [`base32_encode`] — stray characters, impossible padding bits — is rejected.
pub fn base32_decode(text: &str) -> Result<Vec<u8>, AuthError> {
    let mut bytes: Vec<u8> = Vec::with_capacity(text.len() * 5 / 8 + 1);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for c in text.chars() {
        let index = match c {
            'A'..='Z' => u32::from(c) - u32::from('A'),
            'a'..='z' => u32::from(c) - u32::from('a'),
            '2'..='7' => u32::from(c) - u32::from('2') + 26,
            _ => return Err(AuthError::Base32Invalid),
        };
        buffer = (buffer << 5) | index;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
        }
    }
    // Trailing padding bits must be zero; anything else was not encoder output.
    if bits > 0 && (buffer & ((1 << bits) - 1)) != 0 {
        return Err(AuthError::Base32Invalid);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::expect_ok;
    use spot_core::ManualClock;
    use time::OffsetDateTime;

    fn clock_at(unix_seconds: i64) -> ManualClock {
        let start = match OffsetDateTime::from_unix_timestamp(unix_seconds) {
            Ok(time) => time,
            Err(_) => OffsetDateTime::UNIX_EPOCH,
        };
        ManualClock::new(start)
    }

    fn seed_secret(seed: &[u8]) -> TotpSecret {
        expect_ok(TotpSecret::from_bytes(seed.to_vec()), "rfc seed")
    }

    fn rfc_config(algorithm: TotpAlgorithm) -> TotpConfig {
        TotpConfig {
            algorithm,
            digits: TotpDigits::Eight,
            step_seconds: DEFAULT_STEP_SECONDS,
            skew_steps: 0,
        }
    }

    fn six_digit_config() -> TotpConfig {
        TotpConfig {
            algorithm: TotpAlgorithm::Sha1,
            digits: TotpDigits::Six,
            step_seconds: DEFAULT_STEP_SECONDS,
            skew_steps: 1,
        }
    }

    #[test]
    fn rfc_6238_vectors_pass() {
        // RFC 6238 Appendix B, 8-digit codes at the shared T values. The SHA-1 column is
        // reproduced from the RFC; the other columns are the same construction over the
        // RFC's longer ASCII seeds, computed independently and cross-checked.
        let vectors = [
            (59, TotpAlgorithm::Sha1, "94287082"),
            (1111111109, TotpAlgorithm::Sha1, "07081804"),
            (1111111111, TotpAlgorithm::Sha1, "14050471"),
            (1234567890, TotpAlgorithm::Sha1, "89005924"),
            (2000000000, TotpAlgorithm::Sha1, "69279037"),
            (20000000000, TotpAlgorithm::Sha1, "65353130"),
            (59, TotpAlgorithm::Sha256, "46119246"),
            (1111111109, TotpAlgorithm::Sha256, "68084774"),
            (1111111111, TotpAlgorithm::Sha256, "67062674"),
            (1234567890, TotpAlgorithm::Sha256, "91819424"),
            (2000000000, TotpAlgorithm::Sha256, "90698825"),
            (20000000000, TotpAlgorithm::Sha256, "77737706"),
            (59, TotpAlgorithm::Sha512, "90693936"),
            (1111111109, TotpAlgorithm::Sha512, "25091201"),
            (1111111111, TotpAlgorithm::Sha512, "99943326"),
            (1234567890, TotpAlgorithm::Sha512, "93441116"),
            (2000000000, TotpAlgorithm::Sha512, "38618901"),
            (20000000000, TotpAlgorithm::Sha512, "47863826"),
        ];
        for (time_seconds, algorithm, code) in vectors {
            let secret = seed_secret(match algorithm {
                TotpAlgorithm::Sha1 => &b"12345678901234567890"[..],
                TotpAlgorithm::Sha256 => &b"12345678901234567890123456789012"[..],
                TotpAlgorithm::Sha512 => {
                    &b"1234567890123456789012345678901234567890123456789012345678901234"[..]
                }
            });
            let config = rfc_config(algorithm);
            let step = u64::try_from(time_seconds).unwrap_or(0) / u64::from(DEFAULT_STEP_SECONDS);
            match config.verify(&secret, code, &clock_at(time_seconds), None) {
                Ok(matched) => {
                    assert_eq!(
                        matched, step,
                        "wrong step for {algorithm:?} at {time_seconds}"
                    )
                }
                Err(error) => {
                    panic!("RFC 6238 vector failed for {algorithm:?} at {time_seconds}: {error}")
                }
            }
        }
    }

    #[test]
    fn skew_window_covers_one_step_each_way() {
        let secret = seed_secret(b"12345678901234567890");
        let config = six_digit_config();
        // HOTP RFC 4226 Appendix D six-digit codes: step 2 is 359152, step 3 is 969429.
        let clock = clock_at(90); // step 3 is current; steps 2..=4 are in the window.
        assert_eq!(
            expect_ok(config.verify(&secret, "359152", &clock, None), "step -1"),
            2
        );
        assert_eq!(
            expect_ok(config.verify(&secret, "969429", &clock, None), "now"),
            3
        );
        let late = expect_ok(secret.code_at(&config, 4), "step +1");
        assert_eq!(
            expect_ok(config.verify(&secret, &late, &clock, None), "step +1"),
            4
        );
        let early = expect_ok(secret.code_at(&config, 1), "step -2");
        match config.verify(&secret, &early, &clock, None) {
            Err(AuthError::TotpCodeInvalid) => {}
            other => panic!("expected TotpCodeInvalid two steps early, got {other:?}"),
        }
        let later = expect_ok(secret.code_at(&config, 5), "step +2");
        match config.verify(&secret, &later, &clock, None) {
            Err(AuthError::TotpCodeInvalid) => {}
            other => panic!("expected TotpCodeInvalid two steps late, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_skew_window_accepts_only_the_current_step() {
        let secret = seed_secret(b"12345678901234567890");
        let config = rfc_config(TotpAlgorithm::Sha1);
        let clock = clock_at(59); // step 1
        let current = expect_ok(secret.code_at(&config, 1), "current");
        assert_eq!(
            expect_ok(config.verify(&secret, &current, &clock, None), "accept now"),
            1
        );
        let later = expect_ok(secret.code_at(&config, 2), "later");
        match config.verify(&secret, &later, &clock, None) {
            Err(AuthError::TotpCodeInvalid) => {}
            other => panic!("expected TotpCodeInvalid for a step ahead, got {other:?}"),
        }
    }

    #[test]
    fn replayed_steps_are_refused() {
        let secret = seed_secret(b"12345678901234567890");
        let config = six_digit_config();
        let clock = clock_at(60); // step 2
        let code = expect_ok(secret.code_at(&config, 2), "code");
        assert_eq!(
            expect_ok(config.verify(&secret, &code, &clock, None), "first use"),
            2
        );
        // The same code again must be refused, and so must any step at or before the
        // recorded one, matched or not.
        match config.verify(&secret, &code, &clock, Some(2)) {
            Err(AuthError::TotpReplayed) => {}
            other => panic!("expected TotpReplayed for reuse, got {other:?}"),
        }
        let older = expect_ok(secret.code_at(&config, 1), "older code");
        match config.verify(&secret, &older, &clock, Some(1)) {
            Err(AuthError::TotpReplayed) => {}
            other => panic!("expected TotpReplayed for an older step, got {other:?}"),
        }
    }

    #[test]
    fn malformed_codes_are_refused_before_any_hmac_work() {
        let secret = TotpSecret::generate();
        let config = six_digit_config();
        for bad in ["", "12345", "1234567", "abcdef", "12 456", "123456\n"] {
            match config.verify(&secret, bad, &clock_at(59), None) {
                Err(AuthError::TotpCodeInvalid) => {}
                other => panic!("expected TotpCodeInvalid for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn secret_length_bounds_are_enforced() {
        match TotpSecret::from_bytes(vec![0u8; SECRET_MIN_LENGTH - 1]) {
            Err(AuthError::SecretLength) => {}
            other => panic!("expected SecretLength below the minimum, got {other:?}"),
        }
        assert!(TotpSecret::from_bytes(vec![2u8; SECRET_MIN_LENGTH]).is_ok());
        assert!(TotpSecret::from_bytes(vec![3u8; SECRET_MAX_LENGTH]).is_ok());
        match TotpSecret::from_bytes(vec![0u8; SECRET_MAX_LENGTH + 1]) {
            Err(AuthError::SecretLength) => {}
            other => panic!("expected SecretLength above the maximum, got {other:?}"),
        }
        // A generated secret and its Base32 text round trip exactly.
        let secret = TotpSecret::generate();
        let restored = expect_ok(TotpSecret::from_base32(&secret.to_base32()), "round trip");
        assert_eq!(secret.to_base32(), restored.to_base32());
    }

    #[test]
    fn base32_matches_the_rfc_4648_test_vectors() {
        for (bytes, text) in [
            (&b""[..], ""),
            (&b"f"[..], "MY"),
            (&b"fo"[..], "MZXQ"),
            (&b"foo"[..], "MZXW6"),
            (&b"foob"[..], "MZXW6YQ"),
            (&b"fooba"[..], "MZXW6YTB"),
            (&b"foobar"[..], "MZXW6YTBOI"),
        ] {
            assert_eq!(base32_encode(bytes), text);
            let decoded = match base32_decode(text) {
                Ok(decoded) => decoded,
                Err(error) => panic!("decoding {text} failed: {error}"),
            };
            assert_eq!(decoded, bytes.to_vec());
        }
        // Lower case decodes to the same bytes; padding, non-zero padding bits and stray
        // characters are typed rejections.
        assert_eq!(
            expect_ok(base32_decode("mzxw6ytboi"), "lower case"),
            b"foobar".to_vec()
        );
        match base32_decode("MZ=") {
            Err(AuthError::Base32Invalid) => {}
            other => panic!("expected Base32Invalid for padding, got {other:?}"),
        }
        match base32_decode("MZ") {
            Err(AuthError::Base32Invalid) => {}
            other => panic!("expected Base32Invalid for non-zero padding bits, got {other:?}"),
        }
        match base32_decode("MZ0") {
            Err(AuthError::Base32Invalid) => {}
            other => panic!("expected Base32Invalid for a stray character, got {other:?}"),
        }
    }

    #[test]
    fn provisioning_uri_carries_the_params_and_escapes_the_labels() {
        let secret = expect_ok(
            TotpSecret::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
            "16-byte secret",
        );
        let config = six_digit_config();
        let uri = secret.provisioning_uri(&config, "Spotledger", "ada@example.org");
        assert!(
            uri.starts_with("otpauth://totp/Spotledger%3Aada%40example.org?"),
            "{uri}"
        );
        assert!(uri.contains("issuer=Spotledger&"), "{uri}");
        assert!(
            uri.contains("secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&"),
            "{uri}"
        );
        assert_eq!(
            uri.rsplit_once('&').map(|(_, tail)| tail),
            Some("period=30"),
            "{uri}"
        );
        assert!(uri.contains("algorithm=SHA1&digits=6&"), "{uri}");
    }

    #[test]
    fn debug_is_redacted() {
        let secret = TotpSecret::generate();
        assert_eq!(format!("{secret:?}"), "Secret(***)");
    }
}
