#![forbid(unsafe_code)]
//! Secret and personal-data redaction: the last line of defence on the way out of the
//! process (WP-PRD-08.T1).
//!
//! A [`Redactor`] cleans anything about to leave the process — log lines, error text,
//! telemetry attributes, support bundles, AI prompts — of secrets and personal data. Audit
//! events are **not** this crate's concern (ADR-061 carries audit in the journal), and
//! this crate does not know how to write a log: hosts plug it where text leaves the
//! process, and the `tracing` feature ships a ready-made [`RedactingMakeWriter`].
//!
//! The redactor is built from a TOML [`RedactionPolicy`] (the pattern-rule switches and the
//! JSON field names) plus the known secrets registered at runtime, never stored in the
//! policy: no secret therefore belongs in a config file it does not already belong in.
//!
//! # Guarantees (all tested)
//!
//! - **Idempotent:** `redact(redact(x)) == redact(x)`. The markers are inert: no rule's
//!   value alphabet includes `[`.
//! - **Bounded growth:** a replacement is at most the matched length plus 24 bytes
//!   ([`Redactor::redact_str_counted`] exposes the replacement count).
//! - **Linear work:** matching uses the `regex` crate (linear time, no catastrophic
//!   backtracking); the input is capped at [`MAX_INPUT_BYTES`] and a truncated result
//!   carries [`TRUNCATION_MARKER`].
//! - **Panic-free:** everything takes `&str` (hosts pass lossy text) and never unwraps.
//!
//! # Default pattern rules
//!
//! Every rule below is compiled in and **on by default** except the `pii` class; each one
//! has its own boolean switch under `[patterns]` in the [`RedactionPolicy`], so a host can
//! turn a single shape off without turning the others off.
//!
//! | `[patterns]` switch | shape it matches | replacement |
//! | --- | --- | --- |
//! | `pem_private_key` | `-----BEGIN ... PRIVATE KEY-----` to `-----END ...-----` | `[redacted:private_key]` |
//! | `jwt` | three base64url parts (`eyJ...`), two-part unsigned forms included | `[redacted:jwt]` |
//! | `bearer` | `Bearer <token>` (the scheme stays) | `Bearer [redacted:token]` |
//! | `authorization` | `authorization`, `proxy-authorization`, `x-api-key`, `www-authenticate`, `cookie`, `set-cookie` values (the name stays) | `name: [redacted:header]` |
//! | `platform_api_key` | `spk_<key id>_<secret>` (`spot-auth`) | `[redacted:api_key]` |
//! | `aws_key_id` | `AKIA`/`ASIA`/`AGPA`/`AIDA`/`AROA`/`AIPA`/`ANPA`/`ANVA`/`ABIA`/`ACCA` + 16 `[A-Z0-9]` | `[redacted:aws_key_id]` |
//! | `github_token` | `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_` + 36 token characters, or `github_pat_` + 22 or more | `[redacted:github_token]` |
//! | `slack_token` | `xoxa-`, `xoxb-`, `xoxp-`, `xoxr-`, `xoxs-` + 9 or more token characters | `[redacted:slack_token]` |
//! | `stripe_key` | `sk_live_`, `sk_test_`, `rk_live_`, `rk_test_` + 16 or more (`pk_` keys are public and stay) | `[redacted:stripe_key]` |
//! | `google_api_key` | `AIza` + 35 or more `[0-9A-Za-z_-]` | `[redacted:google_api_key]` |
//! | `npm_token` | `npm_` + 36 or more token characters | `[redacted:npm_token]` |
//! | `session_token` | session/access/refresh token values printed by name (the name stays) | `name=[redacted:session]` |
//! | `parameter_credentials` | `password=`, `secret=`, `token=`, `api_key=`-family values (the name stays) | `password=[redacted:parameter]` |
//! | `connection_string` | `scheme://user:password@` (the scheme and `@` stay) | `scheme://user:[redacted:credentials]@` |
//! | `card_number` | 13-19 digits that pass the Luhn check | `[redacted:card_number]` |
//! | `iban` | 15-34 characters that pass the mod-97 check | `[redacted:iban]` |
//! | `pii_email` (off) | email addresses | `[redacted:email]` |
//! | `pii_phone` (off) | phone numbers | `[redacted:phone]` |
//!
//! The bare provider rules exist for the free text no key name guards: a log line, an
//! error, a support bundle. The whole match becomes the marker, so **no character of the
//! secret is kept** — not even its prefix. The rules that keep text keep a *name* (a
//! header, a parameter key, a URL scheme), never a byte of the value.
//!
//! ## Why no pattern can be made to backtrack
//!
//! Matching is the `regex` crate (1.13): every pattern compiles to a Thompson NFA and is
//! run by the pike VM / bounded backtracker, so the engine never explores a choice point
//! twice and time and memory are **linear in the input length** for every pattern and
//! every input. The classic ReDoS shape — a nested quantifier such as `(a+)+` — cannot
//! express catastrophic backtracking here, and none of the rules nests one repetition
//! inside another: the provider rules are a literal prefix followed by a single
//! character-class repetition (the AWS one capped at exactly sixteen characters), and the
//! other rules' bounds (`{12,18}`, `{1,8}`, `{13,19}`) are bounded in the same way. The
//! `luhn_ok` and `mod97_ok` validators run per match over that match's own bytes, so their
//! cost is linear in the text as well.
//!
//! # Where `Redactor` plugs into `spot-telemetry`
//!
//! `spot-telemetry` today guards headers wholesale (`is_sensitive_header`,
//! `loggable_headers`) and prints `***`; layering a [`Redactor`] under its log and OTLP
//! sinks is `spot-telemetry`'s change to make (that crate is not owned here), so the
//! integration point is noted in the task report rather than wired here.
//!
//! # Known secrets and the in-memory trade-off
//!
//! Registration digests each secret (salted SHA-256 plus length) for safe reporting, and
//! keeps its matching forms — the value itself and its base64 and URL-encoded encodings —
//! only in [`Zeroizing`] memory, wiped when the redactor drops. Finding every substring or
//! encoding of a secret by digest alone would mean hashing every suffix window of the
//! input; holding the plaintext forms is the accepted trade-off (live memory only).

pub mod encoding;
pub mod error;
pub mod policy;

mod known;
mod patterns;
mod redactor;
#[cfg(feature = "tracing")]
pub mod tracing_writer;

pub use encoding::percent_encode;
pub use error::RedactError;
pub use known::KnownSecrets;
pub use patterns::Rules;
pub use policy::{DEFAULT_FIELD_NAMES, Patterns, RedactionPolicy};
pub use redactor::Redactor;
#[cfg(feature = "tracing")]
pub use tracing_writer::RedactingMakeWriter;

/// The opening half of a redaction marker: `[redacted:`.
pub const MARKER_OPEN: &str = "[redacted:";
/// The closing half of a redaction marker: `]`.
pub const MARKER_CLOSE: &str = "]";

/// Input longer than this is truncated before matching; the result carries the marker.
pub const MAX_INPUT_BYTES: usize = 1_048_576;

/// The marker appended to a result whose input exceeded [`MAX_INPUT_BYTES`].
pub const TRUNCATION_MARKER: &str = "[spot-redact: truncated at 1048576 bytes]";
