//! The password policy every login form runs before hashing.
//!
//! `PasswordPolicy` carries the length bounds for this login and the smallest accepted
//! strength score. All violated rules are reported together as [`PolicyViolation`] values
//! with stable codes, so a UI can show every reason at once; messages contain no secret
//! material and no echo of the password itself.

use crate::password::Password;

/// The rules a password must satisfy before it may be accepted for hashing.
///
/// Defaults: 10–1,024 characters, strength score at least 3 of 4, and no occurrence of a
/// context string (an email local-part, site name or similar the attacker already knows).
/// All violations are reported, never only the first one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasswordPolicy {
    /// Smallest accepted length, in Unicode scalar values.
    pub min_length: u32,
    /// Largest accepted length: also bounds the cost of hashing and of strength estimation.
    pub max_length: u32,
    /// Smallest accepted zxcvbn strength score, 0 (weakest) to 4 (strongest). Zero disables
    /// the strength check entirely (tests, login-only checks).
    pub min_strength: u8,
    /// Whether occurrence of any context string of four or more characters,
    /// case-insensitively, is forbidden.
    pub forbid_context: bool,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_length: 10,
            max_length: 1024,
            min_strength: 3,
            forbid_context: true,
        }
    }
}

impl PasswordPolicy {
    /// Stable code for a password below the minimum length.
    pub const CODE_TOO_SHORT: &'static str = "auth.password.too_short";
    /// Stable code for a password above the maximum length.
    pub const CODE_TOO_LONG: &'static str = "auth.password.too_long";
    /// Stable code for a password the strength estimator rates too weak.
    pub const CODE_TOO_WEAK: &'static str = "auth.password.too_weak";
    /// Stable code for a password containing a forbidden context string.
    pub const CODE_CONTAINS_CONTEXT: &'static str = "auth.password.contains_context";

    /// Checks every rule and reports all violated ones together.
    ///
    /// `context` is what is known about the user outside the password — the account's email
    /// (or its local part), the account name, the site name. Strings shorter than four
    /// characters are ignored as context: too common to be a rule worth enforcing.
    pub fn check(&self, password: &Password, context: &[&str]) -> Result<(), Vec<PolicyViolation>> {
        let mut violations = Vec::new();
        let chars = u32::try_from(password.character_count()).unwrap_or(u32::MAX);

        if chars < self.min_length {
            violations.push(PolicyViolation {
                code: Self::CODE_TOO_SHORT,
                message: format!(
                    "password must be at least {} characters long, and is {}",
                    self.min_length, chars
                ),
            });
        }
        if chars > self.max_length {
            violations.push(PolicyViolation {
                code: Self::CODE_TOO_LONG,
                message: format!(
                    "password must be at most {} characters long, and is {}",
                    self.max_length, chars
                ),
            });
        }
        if self.forbid_context && contained_context(password.as_str(), context) {
            violations.push(PolicyViolation {
                code: Self::CODE_CONTAINS_CONTEXT,
                message:
                    "password contains personal or site context; choose words unrelated to you"
                        .to_owned(),
            });
        }
        self.check_strength(password, chars, context, &mut violations);

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    fn check_strength(
        &self,
        password: &Password,
        chars: u32,
        context: &[&str],
        violations: &mut Vec<PolicyViolation>,
    ) {
        // Over-long passwords are rejected on length alone, which keeps the strength
        // estimation cost bounded by `max_length`.
        if self.min_strength == 0 || chars > self.max_length {
            return;
        }
        let estimate = zxcvbn::zxcvbn(password.as_str(), &user_inputs(context));
        if u8::from(estimate.score()) < self.min_strength {
            violations.push(PolicyViolation {
                code: Self::CODE_TOO_WEAK,
                message: weak_message(&estimate),
            });
        }
    }
}

/// One violated password rule.
#[derive(Debug)]
pub struct PolicyViolation {
    /// Stable code for API responses and logging, e.g. `auth.password.too_weak`.
    pub code: &'static str,
    /// Plain-language message for display; never contains the password itself.
    pub message: String,
}

impl std::fmt::Display for PolicyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // "code: message" keeps logs and API payloads compact and greppable.
        f.write_fmt(format_args!("{}: {}", self.code, self.message))
    }
}

// Turns the estimator's help into one actionable sentence. The estimator's own texts are
// used because they are already safe, plain and free of the password.
fn weak_message(estimate: &zxcvbn::Entropy) -> String {
    let mut hints: Vec<String> = Vec::new();
    if let Some(feedback) = estimate.feedback() {
        if let Some(warning) = feedback.warning() {
            hints.push(warning.to_string());
        }
        for suggestion in feedback.suggestions() {
            hints.push(suggestion.to_string());
        }
    }
    if hints.is_empty() {
        "password is too predictable; use a longer phrase or add unrelated words".to_owned()
    } else {
        format!("password is too predictable: {}", hints.join("; "))
    }
}

// A context substring counts once it reaches this many characters; shorter strings are too
// common to be a meaningful rule.
const CONTEXT_MIN_CHARS: usize = 4;

// Case-insensitively searches the password for any context part of sufficient length.
// Context strings are tokenized on every non-alphanumeric character (whitespace and the
// punctuation of names and addresses), so "Ada.Lovelace@example.org" matches both
// "lovelace" and "example" inside a password.
fn contained_context(password: &str, context: &[&str]) -> bool {
    let needle = password.to_lowercase();
    context
        .iter()
        .flat_map(|input| input.split(|c: char| !c.is_alphanumeric()))
        .filter(|part| part.chars().count() >= CONTEXT_MIN_CHARS)
        .any(|part| needle.contains(&part.to_lowercase()))
}

// zxcvbn's user inputs: every context part, so the estimator discounts passwords made of
// the individual words a profile is made of, not just the profile string verbatim.
fn user_inputs<'a>(context: &[&'a str]) -> Vec<&'a str> {
    context
        .iter()
        .flat_map(|input| input.split(|c: char| !c.is_alphanumeric()))
        .filter(|part| !part.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes_of(violations: &[PolicyViolation]) -> Vec<&'static str> {
        violations.iter().map(|v| v.code).collect()
    }

    #[test]
    fn strong_passwords_pass() {
        let policy = PasswordPolicy::default();
        assert!(
            policy
                .check(&Password::new("correct horse battery staple"), &[])
                .is_ok()
        );
    }

    #[test]
    fn every_violation_is_reported_at_once() {
        let policy = PasswordPolicy::default();
        let violations = match policy.check(&Password::new("spot"), &["spot", "spotledger"]) {
            Err(violations) => violations,
            Ok(()) => panic!("expected violations"),
        };
        assert_eq!(
            codes_of(&violations),
            vec![
                PasswordPolicy::CODE_TOO_SHORT,
                PasswordPolicy::CODE_CONTAINS_CONTEXT,
                PasswordPolicy::CODE_TOO_WEAK
            ]
        );
    }

    #[test]
    fn over_long_passwords_are_rejected_on_length_alone() {
        let policy = PasswordPolicy::default();
        let over = "a".repeat(usize::try_from(policy.max_length).unwrap_or(1025) + 1);
        let violations = match policy.check(&Password::new(&over), &[]) {
            Err(violations) => violations,
            Ok(()) => panic!("expected a too_long violation"),
        };
        // The text is far too weak, but it is never estimated: the length rule already applies.
        assert_eq!(codes_of(&violations), vec![PasswordPolicy::CODE_TOO_LONG]);
    }

    #[test]
    fn lengths_count_characters_not_bytes() {
        // Eight accented characters: 16 bytes, 8 Unicode scalar values, too short.
        let policy = PasswordPolicy {
            forbid_context: false,
            min_strength: 0,
            ..PasswordPolicy::default()
        };
        assert!(policy.check(&Password::new("áéíóúáéíóúá"), &[]).is_ok());
        let violations = match policy.check(&Password::new("áéíóúáéíó"), &[]) {
            Err(violations) => violations,
            Ok(()) => panic!("expected a too_short violation"),
        };
        assert_eq!(codes_of(&violations), vec![PasswordPolicy::CODE_TOO_SHORT]);
    }

    #[test]
    fn context_strings_match_case_insensitively_and_by_words() {
        let policy = PasswordPolicy {
            forbid_context: true,
            min_strength: 0,
            ..PasswordPolicy::default()
        };
        let password = Password::new("xada.lovelacex");
        let violations = match policy.check(&password, &["Ada.Lovelace@example.org"]) {
            Err(violations) => violations,
            Ok(()) => panic!("expected a contains_context violation"),
        };
        assert_eq!(
            codes_of(&violations),
            vec![PasswordPolicy::CODE_CONTAINS_CONTEXT]
        );
        // A three-character context is not a rule: too common to be sensible.
        assert!(policy.check(&password, &["ada"]).is_ok());
    }

    #[test]
    fn too_weak_passwords_come_with_guidance() {
        let policy = PasswordPolicy::default();
        let violations = match policy.check(&Password::new("aaaaaaaaaa"), &[]) {
            Err(violations) => violations,
            Ok(()) => panic!("expected a too_weak violation"),
        };
        assert_eq!(codes_of(&violations), vec![PasswordPolicy::CODE_TOO_WEAK]);
        let only = &violations[0];
        assert!(only.message.contains("password is too predictable"));
    }

    #[test]
    fn violations_display_as_code_and_message() {
        let violation = PolicyViolation {
            code: PasswordPolicy::CODE_TOO_SHORT,
            message: "too short".to_owned(),
        };
        assert_eq!(format!("{violation}"), "auth.password.too_short: too short");
    }
}
