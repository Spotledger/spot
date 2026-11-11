//! The named pattern rules: every replacement class the redactor looks for, as linear-time
//! regular expressions plus integer-only validators (Luhn, mod-97).
//!
//! Rules are applied to the whole text one after another, in the order [`Rules::all`]
//! builds them: pem block, jwt, bearer, header value, platform API key, the bare provider
//! token shapes (AWS key id, GitHub, Slack, Stripe, Google, npm), session token, parameter
//! credentials, connection-string credentials, card number, IBAN, then the opt-in `pii`
//! class. Every replacement is the rule's marker (`[redacted:<label>]`), keeping the key,
//! scheme or header name where that reads better in a log line. The markers are inert by
//! construction: no value alphabet includes `[`, so no rule ever matches a marker another
//! rule (or the same rule) wrote.

use regex::Regex;

use crate::error::RedactError;

/// The label each rule's marker carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleLabel {
    /// Bearer tokens (`Bearer <token>`); the scheme stays.
    Bearer,
    /// Values of `Authorization`-family headers; the header name stays.
    Header,
    /// Platform API keys (`spk_<key_id>_<secret>`).
    ApiKey,
    /// AWS access key ids (`AKIA` and the other nine prefixes, bare in free text).
    AwsKeyId,
    /// GitHub tokens (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`).
    GithubToken,
    /// Slack tokens (`xoxa-`, `xoxb-`, `xoxp-`, `xoxr-`, `xoxs-`).
    SlackToken,
    /// Stripe secret and restricted keys (`sk_live_`, `sk_test_`, `rk_live_`, `rk_test_`).
    StripeKey,
    /// Google API keys (`AIza` plus thirty-five characters).
    GoogleApiKey,
    /// npm tokens (`npm_` plus thirty-six characters).
    NpmToken,
    /// Session tokens printed by name; the key stays.
    Session,
    /// JSON Web Tokens.
    Jwt,
    /// PEM private key blocks.
    PrivateKey,
    /// `password=`/`secret=` fragments; the key stays.
    Parameter,
    /// Credentials inside connection URLs; the scheme stays.
    Credentials,
    /// Card numbers that pass the Luhn check.
    CardNumber,
    /// IBANs that pass the mod-97 check.
    Iban,
    /// Email addresses (the `pii` class, off by default).
    Email,
    /// Phone numbers (the `pii` class, off by default).
    Phone,
}

impl RuleLabel {
    /// The marker text for this label.
    #[must_use]
    pub fn marker(self) -> String {
        match self {
            Self::Bearer => "[redacted:token]".to_owned(),
            Self::Header => "[redacted:header]".to_owned(),
            Self::ApiKey => "[redacted:api_key]".to_owned(),
            Self::AwsKeyId => "[redacted:aws_key_id]".to_owned(),
            Self::GithubToken => "[redacted:github_token]".to_owned(),
            Self::SlackToken => "[redacted:slack_token]".to_owned(),
            Self::StripeKey => "[redacted:stripe_key]".to_owned(),
            Self::GoogleApiKey => "[redacted:google_api_key]".to_owned(),
            Self::NpmToken => "[redacted:npm_token]".to_owned(),
            Self::Session => "[redacted:session]".to_owned(),
            Self::Jwt => "[redacted:jwt]".to_owned(),
            Self::PrivateKey => "[redacted:private_key]".to_owned(),
            Self::Parameter => "[redacted:parameter]".to_owned(),
            Self::Credentials => "[redacted:credentials]".to_owned(),
            Self::CardNumber => "[redacted:card_number]".to_owned(),
            Self::Iban => "[redacted:iban]".to_owned(),
            Self::Email => "[redacted:email]".to_owned(),
            Self::Phone => "[redacted:phone]".to_owned(),
        }
    }
}

/// A per-match validator: returns the byte length of the credential within the whole match
/// (the credential starts at the match start; the rest of the match stays), or `None` to
/// veto the match.
type Validator = fn(&str) -> Option<usize>;

/// One named rule: a regex, its marker, whether the key/scheme/name prefix (group 1) stays,
/// and the per-match validator (Luhn, mod-97) that no pattern alone can express.
pub struct Rule {
    /// The rule name (stable, for diagnostics).
    pub name: &'static str,
    regex: Regex,
    marker: String,
    /// Trailing text kept after the marker (`@` for the connection-string rule).
    suffix: &'static str,
    keep_prefix: bool,
    validate: Option<Validator>,
}

impl Rule {
    /// Compiles `pattern`. The patterns are compile-time strings in this file, so an error
    /// is a programming bug surfaced as [`RedactError::Policy`].
    fn new(
        name: &'static str,
        pattern: &str,
        label: RuleLabel,
        suffix: &'static str,
    ) -> Result<Self, RedactError> {
        let regex = Regex::new(pattern).map_err(|e| RedactError::Policy(e.to_string()))?;
        Ok(Self {
            name,
            regex,
            marker: label.marker(),
            suffix,
            keep_prefix: false,
            validate: None,
        })
    }

    /// The marker plus the kept key/scheme prefix (group 1) reads better for the key-shaped
    /// rules; the credential-only rules replace the whole match.
    fn keeps_prefix(mut self) -> Self {
        self.keep_prefix = true;
        self
    }

    /// Attaches the per-match validator.
    fn validated(mut self, validate: Validator) -> Self {
        self.validate = Some(validate);
        self
    }

    /// Applies the rule to `text` in place; returns the number of replacements made. A
    /// vetoed candidate (failed Luhn or mod-97) stays as it was.
    pub fn apply(&self, text: &mut String) -> usize {
        let mut count = 0usize;
        let template = format!("${{1}}{}{}", self.marker, self.suffix);
        let result = self.regex.replace_all(text, |caps: &regex::Captures| {
            let whole = caps.get(0).map_or(String::new(), |m| m.as_str().to_owned());
            if let Some(v) = self.validate {
                // Validator rules are credential-only: the marker replaces the credential's
                // span; whatever the validator trimmed off stays.
                let Some(end) = v(&whole) else { return whole };
                if end == 0 {
                    return whole;
                }
                count += 1;
                let mut out = self.marker.clone();
                out.push_str(&whole[end..]);
                return out;
            }
            count += 1;
            if self.keep_prefix {
                // A value that already holds a marker is left as it is: partial
                // redactions are never re-redacted, so markers keep their punctuation.
                let g1 = caps.get(1).map_or("", |g| g.as_str());
                let markered = caps
                    .get(0)
                    .is_some_and(|m| m.as_str()[g1.len()..].contains(crate::MARKER_OPEN));
                if markered {
                    count -= 1;
                    return whole;
                }
                let mut out = String::new();
                caps.expand(&template, &mut out);
                out
            } else {
                self.marker.clone()
            }
        });
        if count > 0 {
            *text = result.into_owned();
        }
        count
    }
}

/// One row of the rule table: name, pattern, label, per-match validator.
type RuleSpec = (
    &'static str,
    String,
    RuleLabel,
    &'static str,
    Option<Validator>,
);

/// Compiles one rule; the key-shaped rules keep their prefix (group 1).
fn build_rule(
    name: &'static str,
    pattern: &str,
    label: RuleLabel,
    suffix: &'static str,
    validate: Option<Validator>,
) -> Result<Rule, RedactError> {
    let mut rule = Rule::new(name, pattern, label, suffix)?;
    if matches!(
        name,
        "authorization_header"
            | "session_token"
            | "parameter_credentials"
            | "connection_string"
            | "bearer"
    ) {
        rule = rule.keeps_prefix();
    }
    if let Some(v) = validate {
        rule = rule.validated(v);
    }
    Ok(rule)
}

/// The per-rule patterns. Order matters only for reading (every marker is inert); the
/// validators gate the Luhn and mod-97 rules per match.
// Each entry is a named block with its own comment, so the table pushes row by row.
#[allow(clippy::vec_init_then_push)]
fn rule_table() -> Vec<RuleSpec> {
    use RuleLabel as L;
    let mut rules: Vec<RuleSpec> = Vec::new();
    // PEM first: a private key block can hold a JWT- or bearer-shaped body.
    rules.push((
        "pem_private_key",
        r"(?s)(-----BEGIN [A-Z ]*PRIVATE KEY-----[A-Za-z0-9+/=\r\n]+-----END [A-Z ]*PRIVATE KEY-----)".to_owned(),
        L::PrivateKey,
        "",
        None,
    ));
    rules.push((
        "jwt",
        r"\b(eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}(?:\.[A-Za-z0-9_-]*)?)".to_owned(),
        L::Jwt,
        "",
        None,
    ));
    rules.push((
        "bearer",
        r"(?i)(\bbearer[ \t]+)([A-Za-z0-9._~+/=-]{8,})".to_owned(),
        L::Bearer,
        "",
        None,
    ));
    rules.push((
        "authorization_header",
        r#"(?i)\b(((?:proxy-)?authorization|x-api-key|www-authenticate|cookie|set-cookie)(\s*["']?\s*[:=][ \t]*["']?))([^\r\n,}"]+)"#
            .to_owned(),
        L::Header,
        "",
        None,
    ));
    // The API-key shape: `spk_`, a 12-character lower-case Base32 key id, a 43-character
    // Base64url secret (spot-auth).
    rules.push((
        "platform_api_key",
        r"\b(spk_[a-z2-7]{12}_[A-Za-z0-9_-]{43}\b)".to_owned(),
        L::ApiKey,
        "",
        None,
    ));
    // Bare provider tokens: the shapes `spot-connectors`, the LLM clients and the bundle
    // exporters meet in free text, with no key name in front of them. Each pattern starts
    // on a word boundary and consumes the whole maximal run of its token alphabet, so a
    // match never starts mid-identifier and never stops mid-token; only the AWS rule stops
    // at the fixed sixteen characters its format defines (a longer run is refused, never
    // partly redacted). The whole match becomes the marker: no prefix of the secret stays.
    rules.push((
        "aws_key_id",
        r"\b((?:AKIA|ASIA|AGPA|AIDA|AROA|AIPA|ANPA|ANVA|ABIA|ACCA)[A-Z0-9]{16})\b".to_owned(),
        L::AwsKeyId,
        "",
        None,
    ));
    rules.push((
        "github_token",
        // Five classic shapes (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`) and the
        // fine-grained `github_pat_` shape.
        r"\b((?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,}))".to_owned(),
        L::GithubToken,
        "",
        None,
    ));
    rules.push((
        "slack_token",
        // `xox` plus the a/b/p/r/s kinds, a dash, then the token body (dashes included).
        // No trailing boundary: a body may end with a dash, where `\b` would not hold.
        r"\b(xox[abprs]-[A-Za-z0-9-]{9,})".to_owned(),
        L::SlackToken,
        "",
        None,
    ));
    rules.push((
        "stripe_key",
        // Secret and restricted keys in live and test mode; publishable (`pk_`) keys are
        // public by design and stay readable.
        r"\b((?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,})".to_owned(),
        L::StripeKey,
        "",
        None,
    ));
    rules.push((
        "google_api_key",
        // `AIza` plus thirty-five characters; the alphabet holds `-` and `_`, so the run is
        // greedy and carries no trailing boundary (a key may end on a dash).
        r"\b(AIza[0-9A-Za-z_-]{35,})".to_owned(),
        L::GoogleApiKey,
        "",
        None,
    ));
    rules.push((
        "npm_token",
        r"\b(npm_[A-Za-z0-9]{36,})".to_owned(),
        L::NpmToken,
        "",
        None,
    ));
    rules.push((
        "session_token",
        // Separators never eat the value's quotes (the quoted alternative does); bare
        // values run to whitespace or a separator byte.
        r#"(?i)\b((?:session[-_ ]?(?:token|id)?|sess[-_ ]?(?:token|id)|access[-_ ]?token|refresh[-_ ]?token)\s*["']?\s*[:=][ \t]*)("[A-Za-z0-9._~-]{8,}"|'[A-Za-z0-9._~-]{8,}'|[A-Za-z0-9._~-]{8,})"#
            .to_owned(),
        L::Session,
        "",
        None,
    ));
    rules.push((
        "parameter_credentials",
        // Query, form and JSON-ish credentials; the quoted alternatives may hold spaces.
        // Bare colon values stop at whitespace (free text after a colon is out of scope).
        r#"(?i)\b((?:pass(?:word|wd)?|pwd|secret|token|api[_-]?key|apikey|client[_-]?secret|auth[_-]?token)\s*["']?\s*[:=][ \t]*)("[^"\r\n]*"|'[^'\r\n]*'|[^\s&;"',}\[]+)"#
            .to_owned(),
        L::Parameter,
        "",
        None,
    ));
    rules.push((
        "connection_string",
        r#"(?i)\b((?:https?|postgres(?:ql)?|mysql|mariadb|redis|rediss|amqps?|mongodb(?:\+srv)?|sftp|ftp)://)([^\s:/@"]*):([^\s:/@"]+)(@)"#
            .to_owned(),
        L::Credentials,
        "${4}",
        None,
    ));
    rules.push((
        "card_number",
        // Lazy: stops at the first boundary, so two adjacent numbers never merge; the
        // Luhn validator then vetoes ordinary 13-19 digit numbers.
        r"\b(\d(?:[ -]?\d){12,18}?)\b".to_owned(),
        L::CardNumber,
        "",
        Some(luhn_ok),
    ));
    rules.push((
        "iban",
        r"(\b[A-Za-z]{2}\d{2}(?:[ ]?[A-Za-z0-9]{1,4}){1,8}\b)".to_owned(),
        L::Iban,
        "",
        Some(mod97_ok),
    ));
    rules.push((
        "pii_email",
        r"(\b[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}\b)".to_owned(),
        L::Email,
        "",
        None,
    ));
    rules.push((
        "pii_phone",
        r"(?i)(\+\d{1,3}(?:[ -]?\d{2,6}){1,6}\b|\(\d{3}\) \d{3}-\d{4}\b)".to_owned(),
        L::Phone,
        "",
        None,
    ));
    rules
}

/// The Luhn checksum (ISO/IEC 7812-1): from the rightmost digit, double every second one.
/// 13 to 19 digits, integer arithmetic only (ADR-12 keeps floats from money). Returns the
/// credential length within the candidate (all of it), or `None` for a veto.
#[must_use]
pub fn luhn_ok(candidate: &str) -> Option<usize> {
    let digits: Vec<u32> = candidate
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c.to_digit(10).unwrap_or(0))
        .collect();
    if digits.len() < 13 || digits.len() > 19 {
        return None;
    }
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = *d;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    if sum.is_multiple_of(10) {
        Some(candidate.len())
    } else {
        None
    }
}

/// The IBAN check (ISO 7064 mod 97-10): strip spaces, move the first four characters to the
/// end, replace letters by their number (A=10 .. Z=35) and require the whole decimal number
/// to be 1 mod 97.
///
/// The pattern is greedy over trailing words, so this scans candidates back to the shortest
/// valid prefix: the return value is how many bytes of the candidate are the IBAN (the text
/// after it stays), or `None` when no prefix qualifies.
#[must_use]
pub fn mod97_ok(candidate: &str) -> Option<usize> {
    let mut k = candidate.len();
    while k >= 15 {
        // A prefix is a candidate only if the next byte is not alphanumeric: a real IBAN
        // ends at a token boundary, so cutting a longer number mid-token is refused.
        let boundary = k == candidate.len() || !candidate.as_bytes()[k].is_ascii_alphanumeric();
        if boundary {
            let cand = candidate[..k].trim_end_matches([' ', '-']);
            if cand.len() >= 15 {
                let s: String = cand
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                if s.len() >= 15 && s.len() <= 34 && mod97_valid(&s) {
                    return Some(cand.len());
                }
            }
        }
        k -= 1;
    }
    None
}

/// The mod-97 residue of a cleaned IBAN: letters and digits only.
fn mod97_valid(s: &str) -> bool {
    if !s.is_char_boundary(4) {
        return false;
    }
    if !s.is_char_boundary(2) {
        return false;
    }
    let rearranged: String = format!("{}{}", &s[4..], &s[..4]);
    let mut rem: u32 = 0;
    for c in rearranged.chars() {
        match c {
            'a'..='z' | 'A'..='Z' => {
                let v = u32::from(c.to_ascii_uppercase() as u8 - b'A') + 10;
                rem = (rem * 100 + v) % 97;
            }
            '0'..='9' => rem = (rem * 10 + u32::from(c as u8 - b'0')) % 97,
            _ => return false,
        }
    }
    rem == 1
}

/// The compiled rules a [`crate::Redactor`] runs, in application order (crate docs).
pub struct Rules(Vec<Rule>);

impl Rules {
    /// Builds the enabled rules from the policy switches. The patterns are compile-time
    /// strings; an error here is a bug, surfaced as [`RedactError::Policy`]. The rules run
    /// in the table's order (crate docs): PEM block, JWT, bearer, header, platform API key,
    /// the bare provider tokens (AWS key id, GitHub, Slack, Stripe, Google, npm), session
    /// token, parameter credentials, connection string, card, IBAN, then the opt-in `pii`.
    ///
    /// Every default rule has its own switch in [`crate::policy::Patterns`], so a policy
    /// can turn one shape off without turning the others off.
    pub fn build(p: &crate::policy::Patterns) -> Result<Self, RedactError> {
        let mut rules = Vec::new();
        for (name, pattern, label, suffix, validate) in rule_table() {
            let on = match name {
                "pem_private_key" => p.pem_private_key,
                "jwt" => p.jwt,
                "bearer" => p.bearer,
                "authorization_header" => p.authorization,
                "platform_api_key" => p.platform_api_key,
                "aws_key_id" => p.aws_key_id,
                "github_token" => p.github_token,
                "slack_token" => p.slack_token,
                "stripe_key" => p.stripe_key,
                "google_api_key" => p.google_api_key,
                "npm_token" => p.npm_token,
                "session_token" => p.session_token,
                "parameter_credentials" => p.parameter_credentials,
                "connection_string" => p.connection_string,
                "card_number" => p.card_number,
                "iban" => p.iban,
                "pii_email" => p.pii_email,
                "pii_phone" => p.pii_phone,
                other => {
                    return Err(RedactError::Policy(format!(
                        "rule {other} has no policy switch"
                    )));
                }
            };
            if on {
                rules.push(build_rule(name, &pattern, label, suffix, validate)?);
            }
        }
        Ok(Self(rules))
    }

    /// Applies every rule in order; the sum of replacements.
    pub fn apply_all(&self, text: &mut String) -> usize {
        self.0.iter().map(|r| r.apply(text)).sum()
    }

    /// The rule names in application order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.0.iter().map(|r| r.name).collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // the brief exempts tests from the no-expect rule
    use super::Rules;
    use crate::policy::Patterns;

    /// A policy switch: what a caller writes as `rule = false` in `[patterns]`.
    type Switch = fn(&mut Patterns);

    /// Task WP-PRD-08.T3 deliverable 3: every new default rule has its own switch, and
    /// turning it off leaves the text exactly as it was — no rule fires, nothing changes.
    #[test]
    fn every_provider_rule_can_be_switched_off_on_its_own() {
        let cases: [(Switch, &str, &str); 6] = [
            (
                |p| p.aws_key_id = false,
                "AKIAIOSFODNN7EXAMPLE",
                "aws_key_id",
            ),
            (
                |p| p.github_token = false,
                "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
                "github_token",
            ),
            (
                |p| p.slack_token = false,
                "xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOp",
                "slack_token",
            ),
            (
                |p| p.stripe_key = false,
                "sk_live_51NxAbCdEfGhIjKlMnOpQrStUv",
                "stripe_key",
            ),
            (
                |p| p.google_api_key = false,
                "AIzaSyD-1234567890abcdefghijklmnopqrstu",
                "google_api_key",
            ),
            (
                |p| p.npm_token = false,
                "npm_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
                "npm_token",
            ),
        ];
        let on = Rules::build(&Patterns::default()).expect("the defaults compile");
        for (switch_off, sample, label) in cases {
            assert!(on.names().contains(&label), "{label} is a default rule");
            let mut text = sample.to_owned();
            assert_eq!(on.apply_all(&mut text), 1, "{label} fires on its own shape");
            assert_eq!(text, format!("[redacted:{label}]"));

            let mut patterns = Patterns::default();
            switch_off(&mut patterns);
            let off = Rules::build(&patterns).expect("the patterns compile");
            assert!(!off.names().contains(&label), "{label} is off");
            let mut text = sample.to_owned();
            assert_eq!(off.apply_all(&mut text), 0, "{label} stays quiet");
            assert_eq!(text, sample, "the text is left exactly as it was");
        }
    }

    /// Switching one rule off never switches another off: the switches are independent.
    #[test]
    fn switching_one_rule_off_leaves_the_others_on() {
        let patterns = Patterns {
            aws_key_id: false,
            ..Patterns::default()
        };
        let rules = Rules::build(&patterns).expect("the patterns compile");
        let mut text = "AKIAIOSFODNN7EXAMPLE npm_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789".to_owned();
        assert_eq!(rules.apply_all(&mut text), 1);
        assert_eq!(text, "AKIAIOSFODNN7EXAMPLE [redacted:npm_token]");
    }
}
