//! The module-level whitelisted function registry (WP-PLT-17.T1; ADR-16).
//!
//! Apps declare functions in their manifest as [`spot_manifest::FunctionSub`]s; this crate
//! collects them with their native, WASM or script handlers into a [`FunctionRegistry`] that
//! answers one question for the future `POST /api/method/<app>/<module>/<function>` transport:
//! may this caller call this function with this verb and these arguments, and which handler
//! runs. It decides; it never runs a transport and holds no storage or HTTP types.
//!
//! 1. Apps contribute through a [`FunctionRegistryBuilder`]:
//!    [`register`](FunctionRegistryBuilder::register) for a declared function — the
//!    manifest's [`FunctionSub`] supplies module, name, HTTP verbs and the guest flag, while
//!    the rate limit and argument schema the manifest does not carry ride along on the same
//!    registration — [`register_decl`](FunctionRegistryBuilder::register_decl) for a
//!    host-built declaration, and
//!    [`override_function`](FunctionRegistryBuilder::override_function) for an app replacing
//!    another app's function (the admin override table, as with step overrides).
//! 2. [`FunctionRegistryBuilder::build`] checks everything together and reports **every**
//!    [`Conflict`] at once, each with a stable `function.*` code. Functions and overrides are
//!    sorted before the checks run, so registration order never changes the result.
//! 3. A [`FunctionRegistry`] dispatches through [`FunctionHandler`]s: an unknown function and
//!    a non-guest function called by an anonymous caller both answer `function.not_found`
//!    (the endpoint never reveals which functions exist to a guest); a wrong verb answers
//!    `function.verb_not_allowed` with the `Allow` set; a guest calling a verb that changes
//!    things answers `function.auth_required`; a spent rate limit answers
//!    `function.rate_limited` with `retry_after`; and failed argument schemas answer
//!    `function.bad_arguments`. Rate limiting is a token bucket per `(function, caller)` over
//!    the injectable [`Clock`](spot_core::clock::Clock), with bounded memory: buckets idle
//!    for longer than the threshold are evicted by the host.
//! 4. [`FunctionRegistry::list`] is the admin override listing: every function with its
//!    declaring app, verbs, guest flag, limit, schema, and override by whom and why.
//!
//! The crate reads time only through `spot_core::clock::Clock`, never logs arguments, uses no
//! floats and no unwraps outside tests.

mod args;
mod builder;
mod key;
mod limit;
mod registry;
mod schema;

pub use args::{Caller, Context, FunctionHandler, FunctionHandlerInfo, HandlerFuture};
pub use builder::{Conflict, FunctionDecl, FunctionInfo, FunctionRegistryBuilder, RateLimit};
pub use key::{FunctionKey, InvalidFunctionKey, ModuleKey, NAME_MAX_BYTES, is_storage_name};
pub use limit::{LimitError, RateLimiter, TokenBucket};
pub use registry::FunctionRegistry;
pub use schema::{SchemaError, validate_arguments};
pub use spot_manifest::{FunctionSub, HttpVerb};

/// The upper-case name of an [`HttpVerb`], as a transport puts it into an `Allow` header.
pub fn verb_name(verb: HttpVerb) -> &'static str {
    match verb {
        HttpVerb::Get => "GET",
        HttpVerb::Post => "POST",
        HttpVerb::Put => "PUT",
        HttpVerb::Patch => "PATCH",
        HttpVerb::Delete => "DELETE",
        HttpVerb::Head => "HEAD",
        HttpVerb::Options => "OPTIONS",
    }
}

/// The upper-case verb name of an [`Option`]d verb, `"NONE"` when there is none; the shape the
/// verb-not-allowed message names the rejected verb by.
pub fn verb_name_owned(verb: Option<HttpVerb>) -> String {
    verb.map_or_else(|| "NONE".to_owned(), |v| verb_name(v).to_owned())
}

/// Stable `function.*` codes: dispatch errors and install-time conflicts.
pub mod codes {
    /// The function does not exist (or is hidden from an anonymous caller).
    pub const NOT_FOUND: &str = "function.not_found";
    /// The function does not answer this verb; the error carries the `Allow` set.
    pub const VERB_NOT_ALLOWED: &str = "function.verb_not_allowed";
    /// The function exists and allows the verb, but needs a signed-in caller (a guest on a
    /// verb that changes things).
    pub const AUTH_REQUIRED: &str = "function.auth_required";
    /// The caller has spent the function's rate limit; the error carries `retry_after`.
    pub const RATE_LIMITED: &str = "function.rate_limited";
    /// The arguments do not match the function's argument schema.
    pub const BAD_ARGUMENTS: &str = "function.bad_arguments";

    /// Two apps declare the same function key.
    pub const DUPLICATE: &str = "function.duplicate";
    /// An override targets a function nobody declares, names an invalid app, or carries no
    /// reason.
    pub const OVERRIDE_UNKNOWN: &str = "function.override_unknown";
    /// Two apps override the same function.
    pub const OVERRIDE_DUPLICATE: &str = "function.override_duplicate";
    /// An app overrides its own function through the override table (a chain).
    pub const OVERRIDE_CHAIN: &str = "function.override_chain";
    /// `http` is declared but the verb set is empty.
    pub const VERB_EMPTY: &str = "function.verb_empty";
    /// A guest function without an explicit verb list.
    pub const GUEST_VERBS: &str = "function.guest_verbs";
    /// An app declares a function key outside its own prefix.
    pub const FOREIGN_APP: &str = "function.foreign_app";
}

/// The [`FunctionSub`] one declaring-app registration would carry: module and name from the
/// key, verbs and guest flag from the declaration. For hosts that reinstall a registry's
/// declarations into a manifest, and for the round-trip tests.
impl From<&FunctionDecl> for FunctionSub {
    fn from(decl: &FunctionDecl) -> FunctionSub {
        decl_to_sub(decl)
    }
}

impl From<FunctionDecl> for FunctionSub {
    fn from(decl: FunctionDecl) -> FunctionSub {
        decl_to_sub(&decl)
    }
}

/// The [`FunctionSub`] one declaring-app registration would carry: module and name from the
/// key, verbs and guest flag from the declaration. For hosts that reinstall a registry's
/// declarations into a manifest, and for the round-trip tests.
///
/// # Panics
/// Never for keys that went through [`FunctionKey::parse`]; a host-assembled
/// [`FunctionDecl`] with a non-storage-style part panics here — the builder refuses such a
/// declaration anyway.
pub fn decl_to_sub(decl: &FunctionDecl) -> FunctionSub {
    let named = |what: &str, part: &str| {
        spot_app_api::prelude::Name::new(part)
            .unwrap_or_else(|_| panic!("{what} names must be storage-style names: {part:?}"))
    };
    FunctionSub {
        module: named("module", &decl.key.module),
        name: named("function", &decl.key.name),
        http: decl.verbs.clone(),
        guest: decl.guest,
    }
}
