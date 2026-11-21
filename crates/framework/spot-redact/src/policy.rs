//! The redaction policy: the TOML configuration a [`Redactor`] is built from.
//!
//! Known-secret **values** are never in the policy: they are registered at runtime against a
//! [`Redactor`](crate::Redactor) so no secret ever lands in a file it does not belong in. The
//! policy carries the pattern-rule switches and the JSON field names.
//!
//! ```toml
//! [patterns]
//! card_number = true
//! aws_key_id = true         # every bare provider token rule is on by default
//! github_token = true       # and each one can be switched off on its own
//! pii_email = false         # the `pii` class is off by default
//! pii_phone = false
//!
//! field_names = ["api_secret", "recovery_code"]
//! ```

use serde::{Deserialize, Serialize};

use crate::error::RedactError;

/// Every rule on except the `pii` class (`pii_email`, `pii_phone`), which is off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Patterns {
    /// Bearer tokens: `Bearer <opaque token>` wherever it appears.
    pub bearer: bool,
    /// `Authorization`-family header values (`authorization`, `proxy-authorization`,
    /// `x-api-key`, `www-authenticate`) printed as `name: value`.
    pub authorization: bool,
    /// Platform API keys in the `spk_<key_id>_<secret>` shape (`spot-auth`).
    pub platform_api_key: bool,
    /// AWS access key ids (`AKIA`, `ASIA`, `AGPA`, `AIDA`, `AROA`, `AIPA`, `ANPA`,
    /// `ANVA`, `ABIA`, `ACCA`, then sixteen `[A-Z0-9]`) found bare in free text.
    pub aws_key_id: bool,
    /// GitHub tokens: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_` and `github_pat_`.
    pub github_token: bool,
    /// Slack tokens: `xoxa-`, `xoxb-`, `xoxp-`, `xoxr-`, `xoxs-`.
    pub slack_token: bool,
    /// Stripe secret and restricted keys: `sk_live_`, `sk_test_`, `rk_live_`, `rk_test_`
    /// (publishable `pk_` keys are public and never match).
    pub stripe_key: bool,
    /// Google API keys: `AIza` plus thirty-five characters.
    pub google_api_key: bool,
    /// npm tokens: `npm_` plus thirty-six characters.
    pub npm_token: bool,
    /// Session tokens printed by name (`session-token: <value>` and friends). When
    /// `spot-auth` gains a session-token prefix, that shape joins this rule.
    pub session_token: bool,
    /// JSON Web Tokens (three base64url parts; two-part unsigned forms included).
    pub jwt: bool,
    /// PEM private key blocks (`-----BEGIN ... PRIVATE KEY-----` to `-----END ...`).
    pub pem_private_key: bool,
    /// `password=`/`secret=`-family keys with values: query strings, form bodies and
    /// JSON-ish `"key": "value"` fragments.
    pub parameter_credentials: bool,
    /// Credentials in connection URLs: `scheme://user:password@` (https, postgre*, mysql,
    /// redis, amqps*, mongodb*, ftp, sftp).
    pub connection_string: bool,
    /// Card numbers, 13-19 digits possibly separated by spaces or dashes, kept only when
    /// they pass the Luhn check.
    pub card_number: bool,
    /// IBANs, 15-34 characters, kept only when they pass the mod-97 check.
    pub iban: bool,
    /// Email addresses (the `pii` class; off by default).
    pub pii_email: bool,
    /// Phone numbers (the `pii` class; off by default).
    pub pii_phone: bool,
}

impl Default for Patterns {
    fn default() -> Self {
        Self {
            bearer: true,
            authorization: true,
            platform_api_key: true,
            // The bare provider token shapes (WP-PRD-08.T3).
            aws_key_id: true,
            github_token: true,
            slack_token: true,
            stripe_key: true,
            google_api_key: true,
            npm_token: true,
            session_token: true,
            jwt: true,
            pem_private_key: true,
            parameter_credentials: true,
            connection_string: true,
            card_number: true,
            iban: true,
            // The `pii` class is off by default (WP-PRD-08.T1 deliverable 1).
            pii_email: false,
            pii_phone: false,
        }
    }
}

/// The built-in JSON keys whose values are always redacted (case-insensitive, exact name).
pub const DEFAULT_FIELD_NAMES: &[&str] = &[
    "password",
    "secret",
    "token",
    "api_key",
    "apikey",
    "authorization",
    "cookie",
    "set-cookie",
    "private_key",
    "client_secret",
    "access_token",
    "refresh_token",
    "session_token",
    "auth_token",
    "recovery_code",
    "card_number",
];

/// The redaction policy a [`Redactor`](crate::Redactor) is built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RedactionPolicy {
    /// The pattern-rule switches. A missing rule keeps its documented default.
    #[serde(default)]
    pub patterns: Patterns,
    /// Extra JSON key names whose values are always redacted; these are added to
    /// [`DEFAULT_FIELD_NAMES`], never replace it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_names: Vec<String>,
}

impl RedactionPolicy {
    /// Parses the policy from TOML text. An unknown rule name or an ill-formed file is an
    /// error; the message quotes the toml parser, so never put secret values in a policy.
    pub fn from_toml(text: &str) -> Result<Self, RedactError> {
        toml::from_str(text).map_err(|e| RedactError::Policy(e.to_string()))
    }

    /// The policy in TOML form (stable key order follows the struct field order).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_else(|_| String::new())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // the brief exempts tests from the no-expect rule
    use super::{DEFAULT_FIELD_NAMES, RedactionPolicy};

    #[test]
    fn the_pii_class_is_off_by_default() {
        let p = RedactionPolicy::default();
        assert!(!p.patterns.pii_email);
        assert!(!p.patterns.pii_phone);
        assert!(p.patterns.card_number);
    }

    #[test]
    fn toml_round_trip() {
        let text = "field_names = [\"x\"]\n\n[patterns]\ncard_number = false\npii_email = true\n";
        let p = RedactionPolicy::from_toml(text).expect("valid policy");
        assert!(!p.patterns.card_number);
        assert!(p.patterns.pii_email);
        assert!(p.patterns.bearer, "untouched rules keep their default");
        assert_eq!(p.field_names, vec!["x".to_owned()]);
        assert_eq!(
            p.to_toml(),
            RedactionPolicy::from_toml(&p.to_toml())
                .expect("valid")
                .to_toml()
        );
    }

    #[test]
    fn unknown_rules_are_rejected() {
        assert!(RedactionPolicy::from_toml("[patterns]\nnope = true\n").is_err());
        assert!(RedactionPolicy::from_toml("broken = [").is_err());
    }

    #[test]
    fn empty_toml_gives_the_defaults() {
        let p = RedactionPolicy::from_toml("").expect("empty is the default");
        assert_eq!(p, RedactionPolicy::default());
        assert!(DEFAULT_FIELD_NAMES.len() > 10);
    }

    /// The provider-token rules (WP-PRD-08.T3) are on by default and each can be turned
    /// off from `[patterns]` without naming any other rule.
    #[test]
    fn provider_rules_default_on_and_switchable_from_toml() {
        let p = RedactionPolicy::default();
        assert!(p.patterns.aws_key_id);
        assert!(p.patterns.github_token);
        assert!(p.patterns.slack_token);
        assert!(p.patterns.stripe_key);
        assert!(p.patterns.google_api_key);
        assert!(p.patterns.npm_token);

        let text = "[patterns]\ngithub_token = false\nslack_token = false\n";
        let q = RedactionPolicy::from_toml(text).expect("valid policy");
        assert!(!q.patterns.github_token);
        assert!(!q.patterns.slack_token);
        assert!(q.patterns.aws_key_id, "untouched rules keep their default");
        assert!(q.patterns.npm_token, "untouched rules keep their default");
    }
}
