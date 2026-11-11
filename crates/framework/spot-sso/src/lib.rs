//! Single sign-on for the Spotledger Platform: the tenant sign-in policy and OIDC
//! authorization-code sign-in, as a sans-I/O, clock-free core (WP-IAM-05.T1, ADR-070
//! sections 5 and 7).
//!
//! # What this crate is
//!
//! Two halves that meet at one decision:
//!
//! - The **tenant sign-in policy** ([`SignInPolicy`], [`check_policy`],
//!   [`decide_local_signin`], [`validate_change`]): which sign-in methods a tenant allows
//!   (`local`, `sso_optional`, `sso_required`), who may bypass SSO (the break-glass
//!   administrators), and when the tenant would lock itself out.
//! - **OIDC** ([`begin`], [`callback`], [`complete`]): the authorization-code flow with PKCE
//!   as an explicit state machine — discovery, `state` and `nonce`, full ID-token validation,
//!   key rotation — ending in an [`SsoIdentity`] that [`resolve`] turns into a sign-in.
//!
//! # Purity
//!
//! Nothing here reads the clock, the network, the filesystem or an entropy source. Every
//! function takes what it needs — the discovery document, the key set, the token response,
//! the current time, random bytes — and returns what to do next as data. The HTTP calls are
//! the caller's, through `spot-egress`'s connector. This is why a test needs no socket.
//!
//! No token, code, verifier or secret is ever logged: [`TokenRequest`] redacts its form, and
//! [`SsoError`] carries a public and an administrator text, never a token.
//!
//! # Standards this crate implements, in our own words
//!
//! - **OpenID Connect Core 1.0** — the authorization code flow; ID-token validation in the
//!   order of section 3.1.3.7 (issuer, audience, `azp`, `exp`, `nbf`, `iat`, `nonce`,
//!   `at_hash`, `acr`, `email_verified`).
//! - **RFC 7636 (PKCE)** — `S256` only; the verifier is 43 Base64url characters from 32
//!   random bytes.
//! - **RFC 8414 / OpenID Connect Discovery 1.0** — the provider's metadata document, checked
//!   against the configuration (issuer, `S256`, the code response type).
//! - **RFC 7517 / 7519 / 7518** — JWK sets (RSA ≥ 2048 bits, EC P-256), JWT claims, and the
//!   RS256 and ES256 algorithms only.
//! - **RFC 9700 (OAuth 2.0 security best current practice)** — exact redirect URIs with no
//!   wildcard, `state` for CSRF, `nonce` for replay, PKCE for every client, a bounded clock
//!   skew, and keys taken only from the configured `jwks_uri`.
//!
//! # The no-weaker-method rule
//!
//! A sign-in through SSO carries [`AuthMethod::Sso`] and the authentication context the IdP
//! reported (`acr`/`amr`), so [`session_method_change`] (which delegates to `spot-iam-access`)
//! can refuse a move to a weaker method. Under `sso_required`, a local password session is
//! refused except for break-glass administrators ([`decide_local_signin`]).

#![forbid(unsafe_code)]

#[cfg(feature = "testing")]
pub mod contract;
pub mod error;
mod identity;
mod jose;
pub mod logout;
pub mod method;
pub mod oidc;
pub mod pending;
pub mod policy;
pub mod provider;
pub mod replay;
pub mod resolve;
pub mod saml;
mod wire;

pub use error::{Result, ResultList, SsoError, SsoErrors, codes};
pub use identity::SsoIdentity;
pub use logout::{IdentityRef, LogoutTarget, end_session_url, resolve_target};
pub use method::{AuthMethod, MethodError, MethodGuard, TargetSignIn, session_method_change};
pub use oidc::backchannel::validate_logout_token;
pub use oidc::discovery::Discovery;
pub use oidc::flow::{
    AuthRequest, CallbackNext, CallbackParams, CompleteError, TokenRequest, TokenResponse, begin,
    callback, complete,
};
pub use oidc::jwks::{Jwk, Jwks, KeyCache, MIN_RSA_BITS, PublicKey, SkippedKey, parse_jwks};
pub use oidc::jwt::ParsedToken;
pub use pending::{MemoryPendingStore, PENDING_TTL_SECONDS, PendingAuth, PendingStore};
pub use policy::{
    AlertLevel, BreakGlassAlert, BreakGlassReason, DirectoryUser, DirectoryView, JitPolicy,
    JitRule, LocalDecision, SecondFactor, SignInMode, SignInPolicy, check_policy,
    decide_local_signin, decide_sso_signin, validate_change,
};
pub use provider::{ClaimsMap, ClientAuth, Jwa, ProviderConfig, ProviderId, RawProviderConfig};
pub use replay::{Claim, MemoryReplayStore, ReplayError, ReplayStore, StoreFuture, namespaced};
pub use resolve::{JitCreated, Resolution, identity_key, link_identity, resolve};
pub use saml::{
    IdpConfig, IdpMetadata, MetadataTrust, SamlBinding, SamlClaimsMap, SamlEndpoint,
    SamlPendingAuth, SamlSignIn, SignaturePolicy, SpConfig,
};
