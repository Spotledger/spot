//! Secrets: the generated password of a reset account, and where it goes.
//!
//! A reset account's new password is **generated on the agent** and handed to the
//! operating system and to the sealed result channel; it is never an operation argument,
//! never a handler outcome and never a log line. [`Secret`] is the type that carries it:
//! its `Debug` is redacted, it has no `Display`, no `Serialize` and no `Clone`, and the
//! only way to read it back is the crate-private [`Secret::expose`], which the two
//! [`crate::api::WinApi`] implementations use.
//!
//! [`PasswordSource`] is the entropy seam: [`OsPasswordSource`] fills from the OS CSPRNG
//! (the same helper `spot-agent-enroll` uses for key seeds and tokens) and
//! [`FixedPasswordSource`] is the deterministic test double. [`SealedResultChannel`] is
//! where the secret leaves the module: the adapter for the agent's sealed result channel,
//! which seals the value to the console's key. Until that channel exists in
//! `spot-agent-core` this crate defines the trait so the wiring point is explicit (see the
//! PR's Open questions).

use std::fmt;

use crate::error::WindowsError;

/// A generated secret: a password on its way to the operating system and the sealed
/// result channel, and nowhere else.
///
/// The value is private. `Debug` prints `Secret([redacted])`, there is no `Display`, no
/// `Serialize` and no `Clone`, and [`Secret::expose`] is crate-private, so a secret cannot
/// escape through a log, a wire format or a copied value. On drop the buffer is
/// best-effort overwritten; a guaranteed wipe would need the `zeroize` crate, which the
/// brief does not allow (see the PR's Open questions).
pub struct Secret {
    value: String,
}

impl Secret {
    /// Wraps a freshly generated secret.
    ///
    /// Called by a [`PasswordSource`]; the value must already be secret material, never
    /// an operation argument.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }

    /// The secret's bytes, for the operating-system call that installs it.
    ///
    /// Crate-private on purpose: only the [`crate::api::WinApi`] implementations in this
    /// crate ever need the value, and nothing else may read it. On Linux only the fake
    /// uses it, so the dead-code lint is quiet there.
    #[cfg_attr(not(windows), allow(dead_code))]
    #[must_use]
    pub(crate) fn expose(&self) -> &str {
        &self.value
    }

    /// The secret's length in bytes, without revealing it. Useful for assertions and for a
    /// channel that reports how much it delivered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// Whether the secret is empty (never true for a generated password).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Best effort: overwrite every byte in place, then release the buffer. Without
        // `unsafe` (or the `zeroize` crate) a guaranteed wipe cannot be promised, but a
        // stale copy in freed memory is reduced to zeroes here.
        let zeros = "\0".repeat(self.value.len());
        self.value.replace_range(.., &zeros);
        self.value.clear();
    }
}

/// Where a module gets a new password.
///
/// Injected so tests are deterministic and the real host chooses the entropy source. The
/// only shipped implementation is [`OsPasswordSource`]; [`FixedPasswordSource`] is the
/// test double.
pub trait PasswordSource: Send + Sync {
    /// Generates one fresh secret.
    fn generate(&self) -> Secret;
}

/// Generates passwords from the OS CSPRNG.
///
/// Every password is 24 characters over a 64-symbol alphabet that includes upper case,
/// lower case, digits and punctuation, and the first four characters are forced to cover
/// all four classes, so the result always satisfies the Windows complexity policy. The
/// 64-symbol alphabet divides 256 evenly, so the modulo mapping is unbiased.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsPasswordSource;

/// Upper case letters.
const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// Lower case letters.
const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
/// Digits.
const DIGITS: &[u8] = b"0123456789";
/// Punctuation that Windows accepts in a password.
const SYMBOLS: &[u8] = b"!@";
/// The whole alphabet: 64 symbols, so `byte % 64` is uniform.
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@";
/// The generated password length.
const PASSWORD_LEN: usize = 24;

impl PasswordSource for OsPasswordSource {
    fn generate(&self) -> Secret {
        let mut random = [0u8; PASSWORD_LEN];
        spot_agent_enroll::util::fill_os_random(&mut random);
        let mut value = String::with_capacity(PASSWORD_LEN);
        let classes = [UPPER, LOWER, DIGITS, SYMBOLS];
        for (index, byte) in random.iter().enumerate() {
            // The first four characters cover one class each; the rest are free.
            let pool = classes.get(index).copied().unwrap_or(ALPHABET);
            let symbol = pool[usize::from(*byte) % pool.len()];
            value.push(char::from(symbol));
        }
        Secret::new(value)
    }
}

/// A deterministic [`PasswordSource`] for tests and local experiments.
///
/// It returns a fresh [`Secret`] holding the fixed value on every call, so a test can
/// assert that a password reached the operating system and the sealed channel without
/// depending on the OS CSPRNG.
#[derive(Clone, Debug)]
pub struct FixedPasswordSource {
    value: String,
}

impl FixedPasswordSource {
    /// A source that always yields `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }
}

impl PasswordSource for FixedPasswordSource {
    fn generate(&self) -> Secret {
        Secret::new(self.value.clone())
    }
}

/// What a delivered secret is about: which operation produced it and for which account.
///
/// Carries no secret; it is the routing information the sealed channel needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretLabel {
    /// The operation id that generated the secret.
    pub op: String,
    /// The local account the secret belongs to.
    pub account: String,
}

/// The sealed result channel a generated secret is delivered through.
///
/// The adapter seals the [`Secret`] to the console's key and ships it with the operation
/// result; it must never log the value. Returning [`WindowsError::SecretDelivery`] means
/// the channel refused, and the module then treats the operation as failed (a password
/// that was set but not delivered is worse than a refusal).
pub trait SealedResultChannel: Send + Sync {
    /// Delivers `secret`, tagged by `label`.
    ///
    /// # Errors
    /// [`WindowsError::SecretDelivery`] when the channel could not take the secret.
    fn deliver(&self, label: &SecretLabel, secret: Secret) -> Result<(), WindowsError>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_secret_never_prints_its_value() {
        let secret = Secret::new("Hunter2!Hunter2!Hunter2!");
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "Secret([redacted])");
        assert!(!rendered.contains("Hunter2"));
        assert_eq!(secret.len(), 24);
    }

    #[test]
    fn the_os_source_generates_a_complex_password_of_the_right_length() {
        let secret = OsPasswordSource.generate();
        assert_eq!(secret.len(), PASSWORD_LEN);
        let value = secret.expose();
        assert!(value.chars().any(|c| c.is_ascii_uppercase()));
        assert!(value.chars().any(|c| c.is_ascii_lowercase()));
        assert!(value.chars().any(|c| c.is_ascii_digit()));
        assert!(value.chars().any(|c| SYMBOLS.contains(&(c as u8))));
    }

    #[test]
    fn the_os_source_does_not_repeat_itself() {
        let first = OsPasswordSource.generate();
        let second = OsPasswordSource.generate();
        assert_ne!(first.expose(), second.expose());
    }

    #[test]
    fn the_fixed_source_is_deterministic() {
        let source = FixedPasswordSource::new("Fixed-Passw0rd!Fixed-Pass");
        assert_eq!(source.generate().expose(), source.generate().expose());
    }
}
