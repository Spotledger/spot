//! The grant's two parts: the payload that names what the grant is for, and the tag that proves
//! the authority issued it.
//!
//! A grant is `rg1.<base64url(payload)>.<base64url(tag)>` (base64url without padding). The
//! payload is a flat, self-describing field string so its bytes are exactly one canonical spelling
//! of one record — two encodings of the same grant would otherwise be two different tags under
//! the same secret, and a verifier that re-encoded a payload it had already accepted would be
//! comparing its own work instead of the caller's.
//!
//! ```text
//! v=1&r=<b64url request>&n=<b64url step>&a=<attempt>&u=<b64url actor>&e=<unix expiry>
//! ```
//!
//! Every value that can hold a delimiter is escaped by base64url, so the `&`-and-`=` framing is
//! unambiguous. The tag is `HMAC-SHA256(secret, "rg1." + base64url(payload))`: it covers the
//! prefix, so a grant cannot be relabelled into a later format, and the payload exactly as it
//! arrived, so no field can be changed without invalidating the tag.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;

/// The wire prefix every grant starts with: request grant, format 1.
pub const GRANT_PREFIX: &str = "rg1.";

/// The payload format version this crate writes and the only one it authenticates.
const PAYLOAD_VERSION: &str = "1";

/// The length of a decoded tag: one HMAC-SHA256.
const TAG_LEN: usize = 32;

/// Whether `actor` is a typed principal reference: `user:<id>`, `consumer:<id>` (a non-empty id
/// with no white space) or `system`. A bare id is not one: the same id under two kinds is two
/// principals, and an untyped actor could be read as either (ADR-083 section 2.8 item 4). The
/// engine writes `actor_text` of the invocation's actor, which is always one of these.
#[must_use]
pub fn is_typed_actor(actor: &str) -> bool {
    actor == spot_app_api::context::SYSTEM_PRINCIPAL
        || spot_app_api::PrincipalKind::of_text(actor).is_some()
}

/// What a grant is for: one acting person, one step, one attempt, until one instant.
///
/// It is the decoded payload, not a verified grant: [`ProductionRequestAuthority::authenticate`]
/// is what turns one of these into a
/// [`VerifiedRequest`](spot_bpm_governance::VerifiedRequest) after checking the tag, the expiry,
/// the person and the request store. Decoding alone proves nothing.
///
/// [`ProductionRequestAuthority::authenticate`]: crate::ProductionRequestAuthority::authenticate
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The request the grant was issued for.
    pub request: String,
    /// The step of that request.
    pub node: String,
    /// The attempt of that step.
    pub attempt: u32,
    /// The acting person the grant is bound to.
    pub actor: String,
    /// The absolute instant it stops being valid, as the engine passed it to `issue`.
    pub expires_at: OffsetDateTime,
}

impl Grant {
    /// A grant for `request`'s `node` at `attempt`, held by `actor`, valid until `expires_at`.
    #[must_use]
    pub fn new(
        request: impl Into<String>,
        node: impl Into<String>,
        attempt: u32,
        actor: impl Into<String>,
        expires_at: OffsetDateTime,
    ) -> Self {
        Self {
            request: request.into(),
            node: node.into(),
            attempt,
            actor: actor.into(),
            expires_at,
        }
    }

    /// The payload text: the exact bytes the tag covers.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "v={PAYLOAD_VERSION}&r={}&n={}&a={}&u={}&e={}",
            escape(&self.request),
            escape(&self.node),
            self.attempt,
            escape(&self.actor),
            self.expires_at.unix_timestamp()
        )
    }

    /// The payload this base64url text carries, or `None` when it is not exactly one canonical
    /// payload.
    ///
    /// Strict on purpose: the version must be this one, the fields must arrive in order with no
    /// repeats, the identifiers must be non-blank, the expiry must be a representable instant and
    /// the decoded text must re-encode to the same bytes. Only a caller that already verified the
    /// tag reaches this point, and this is the second line of defence against a payload whose two
    /// spellings a future version might read differently.
    #[must_use]
    pub fn parse_base64(payload_b64: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let grant = Self::parse(&text)?;
        (grant.encode() == text).then_some(grant)
    }

    /// The payload `text` spells, or `None` when it does not spell exactly one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut fields = text.split('&');
        let version = next_field(&mut fields, "v")?;
        if version != PAYLOAD_VERSION {
            return None;
        }
        let request = unescape(&next_field(&mut fields, "r")?)?;
        let node = unescape(&next_field(&mut fields, "n")?)?;
        let attempt = next_field(&mut fields, "a")?.parse::<u32>().ok()?;
        let actor = unescape(&next_field(&mut fields, "u")?)?;
        let expiry = next_field(&mut fields, "e")?.parse::<i64>().ok()?;
        if fields.next().is_some() {
            return None;
        }
        if request.trim().is_empty() || node.trim().is_empty() || !is_typed_actor(&actor) {
            return None;
        }
        let expires_at = OffsetDateTime::from_unix_timestamp(expiry).ok()?;
        Some(Self {
            request,
            node,
            attempt,
            actor,
            expires_at,
        })
    }

    /// The payload as the base64url text a grant carries.
    #[must_use]
    pub fn payload_base64(&self) -> String {
        escape(&self.encode())
    }
}

/// A grant split into its parts but **not** yet verified.
pub(crate) struct Token<'a> {
    /// The payload exactly as it arrived, which is what the tag is computed over.
    pub(crate) payload_b64: &'a str,
    /// The tag exactly as it arrived.
    pub(crate) tag_b64: &'a str,
}

impl<'a> Token<'a> {
    /// Splits `text` into a grant's parts, or `None` when it is not shaped like one.
    ///
    /// Shape only: the prefix, three parts that are not empty and nothing after the tag. Nothing
    /// here is trusted — the tag check and the payload parse decide.
    pub(crate) fn split(text: &'a str) -> Option<Self> {
        let rest = text.strip_prefix(GRANT_PREFIX)?;
        let mut parts = rest.split('.');
        let payload_b64 = parts.next()?;
        let tag_b64 = parts.next()?;
        if parts.next().is_some() || payload_b64.is_empty() || tag_b64.is_empty() {
            return None;
        }
        Some(Self {
            payload_b64,
            tag_b64,
        })
    }

    /// The tag this crate computes for a payload under `secret`.
    pub(crate) fn expected_tag(secret: &[u8], payload_b64: &str) -> Vec<u8> {
        let mut message = Vec::with_capacity(GRANT_PREFIX.len() + payload_b64.len());
        message.extend_from_slice(GRANT_PREFIX.as_bytes());
        message.extend_from_slice(payload_b64.as_bytes());
        ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret),
            &message,
        )
        .as_ref()
        .to_vec()
    }

    /// Whether `self`'s tag is the tag of its payload under `secret`, in constant time.
    ///
    /// A tag of the wrong length is refused before any comparison: the length of a tag is public,
    /// and `ct_eq` is only defined on equal-length slices.
    pub(crate) fn tag_matches(&self, secret: &[u8]) -> bool {
        let Ok(provided) = URL_SAFE_NO_PAD.decode(self.tag_b64) else {
            return false;
        };
        if provided.len() != TAG_LEN {
            return false;
        }
        let expected = Self::expected_tag(secret, self.payload_b64);
        bool::from(expected.ct_eq(&provided))
    }
}

/// The base64url spelling of `value`.
fn escape(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value.as_bytes())
}

/// The text `value` spells, or `None` when it is not base64url or not UTF-8.
fn unescape(value: &str) -> Option<String> {
    String::from_utf8(URL_SAFE_NO_PAD.decode(value).ok()?).ok()
}

/// The next `key=value` field of `fields`, when its key is `key`.
fn next_field<'a>(fields: &mut impl Iterator<Item = &'a str>, key: &str) -> Option<String> {
    let field = fields.next()?;
    let (found, value) = field.split_once('=')?;
    (found == key).then(|| value.to_owned())
}
