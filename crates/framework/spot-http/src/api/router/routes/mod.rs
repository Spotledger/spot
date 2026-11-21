//! The route registry: served operation groups that register themselves.
//!
//! A group is one file `src/api/router/routes/<name>.rs` (its port trait lives in
//! `src/api/ports/<name>.rs`). The file declares:
//!
//! - `pub const SERVES: &[&str]`: the contract operation ids it serves;
//! - `pub fn required_ports() -> &'static [PortKey]`: every port it needs, including the
//!   permission and audit ports of a group that changes data;
//! - `pub fn routes() -> axum::Router<RouteState>`: its routes, with [`RouteState`] as state;
//! - optionally `pub const PUBLIC: Public`: the group is **public** (see below).
//!
//! `build.rs` turns the files into `pub mod` lines and [`REGISTRY`]; adding a group edits no
//! shared file. [`mount`] builds the groups into routers:
//!
//! - **Fail closed.** A group with a required port absent is not mounted at all (never half
//!   wired), and the start-up log names the group and the port once. Its operations then answer
//!   exactly as an unserved operation does today.
//! - A group that serves an operation another group (or the hand-wired routes) already serves
//!   is not mounted either, and the log says which operation.
//! - The caller's identity comes only from the authentication layer the router puts around
//!   every registered group; a group is authenticated unless it declares `PUBLIC`, and its
//!   [`RouteState`] holds only the ports it declared.
//!
//! **Public groups.** `PUBLIC` is the one way to be served without a bearer, for the contract
//! operations that carry `security: []` (a cookie-only renewal, say). It is narrow and fails
//! closed:
//!
//! - the default stays authenticated: no flag, feature or configuration turns the bearer layer
//!   off, and a file without `PUBLIC` is behind it exactly as before;
//! - `build.rs` fails the build when a public group serves an operation whose contract entry is
//!   not `security: []`, and when an authenticated group serves one that is;
//! - a public group runs behind the rate limit of its declared [`RateClass`] (per client
//!   address), the limit is checked before the handler runs, and the `Authorization` header is
//!   removed before the handler sees the request, so a public handler never learns who the
//!   caller is.

use std::collections::BTreeSet;

use axum::Router;

use crate::api::ports::{PortKey, Ports};

/// The state of one route group: only the ports the group declared in `required_ports()`.
#[derive(Clone)]
pub struct RouteState {
    ports: Ports,
}

impl RouteState {
    /// A state over `ports` (the router builds it from the declared subset; tests may use this
    /// to call a group's handlers directly).
    pub fn new(ports: Ports) -> Self {
        RouteState { ports }
    }

    /// A declared port of type `T`; `None` for a port the group did not declare.
    pub fn port<T: ?Sized + Send + Sync + 'static>(&self) -> Option<std::sync::Arc<T>> {
        self.ports.get::<T>()
    }
}

/// A rate class of the contract; a public group declares one and cannot be written without it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RateClass {
    /// The class `auth_login` runs in: unauthenticated calls that spend or mint a credential.
    Auth,
}

/// The marker of a public group: `pub const PUBLIC: Public = Public { rate_class: RateClass::Auth };`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Public {
    /// The rate class the group runs in (mandatory).
    pub rate_class: RateClass,
}

/// One registered route group. `build.rs` fills [`REGISTRY`]; tests build their own.
#[derive(Clone, Copy)]
pub struct Registration {
    /// The module name (the file stem), for logs.
    pub name: &'static str,
    /// The contract operation ids the group serves.
    pub serves: &'static [&'static str],
    /// The ports the group needs.
    pub required_ports: fn() -> &'static [PortKey],
    /// The group's routes.
    pub routes: fn() -> Router<RouteState>,
    /// `Some` for a public group (served outside the bearer layer), `None` (the default) for an
    /// authenticated one.
    pub public: Option<Public>,
}

/// Why a group was not mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The group declares no operation: it cannot be told apart from a stub.
    ServesNothing,
    /// A required port is absent (the port type's name).
    MissingPort(&'static str),
    /// An operation is already served by an earlier group or the hand-wired routes.
    DuplicateOperation(&'static str),
}

/// What [`mount`] did, in registration order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountReport {
    /// The groups mounted (public ones included).
    pub mounted: Vec<&'static str>,
    /// The mounted groups that are public, so the log and tests can say which ones.
    pub public: Vec<&'static str>,
    /// The groups left out, with the reason.
    pub skipped: Vec<(&'static str, SkipReason)>,
    /// The operation ids the mounted groups serve.
    pub served: BTreeSet<&'static str>,
}

// The module list and `REGISTRY` are generated by `build.rs` from `src/api/router/routes/*.rs`.
include!(concat!(env!("OUT_DIR"), "/routes_registry.rs"));

/// The operation ids every registered group serves (the union of `SERVES`).
pub fn registry_serves() -> BTreeSet<&'static str> {
    REGISTRY
        .iter()
        .flat_map(|r| r.serves.iter().copied())
        .collect()
}

/// The groups [`mount`] built: the authenticated ones as one router and each public group as
/// its own router with the rate class it declared.
pub struct Mounted {
    /// The authenticated groups; they still need the authentication layer: the router adds it.
    pub authenticated: Router,
    /// The public groups, each with its declared rate class; they need the rate limit layer
    /// and never get the bearer layer.
    pub public: Vec<(RateClass, Router)>,
    /// What was mounted and what was left out.
    pub report: MountReport,
}

/// Mounts the groups of `registrations` that can be mounted over `ports`. `reserved` are the
/// operation ids the hand-wired routes already serve.
pub fn mount(registrations: &[Registration], ports: &Ports, reserved: &[&'static str]) -> Mounted {
    let mut router: Router = Router::new();
    let mut public_routers: Vec<(RateClass, Router)> = Vec::new();
    let mut report = MountReport::default();
    let mut claimed: BTreeSet<&'static str> = reserved.iter().copied().collect();
    for registration in registrations {
        let name = registration.name;
        if registration.serves.is_empty() {
            tracing::error!(
                module = name,
                "route group serves no operation; not mounted"
            );
            report.skipped.push((name, SkipReason::ServesNothing));
            continue;
        }
        if let Some(op) = registration
            .serves
            .iter()
            .copied()
            .find(|op| claimed.contains(op))
        {
            tracing::error!(
                module = name,
                operation = op,
                "operation is already served; route group not mounted"
            );
            report
                .skipped
                .push((name, SkipReason::DuplicateOperation(op)));
            continue;
        }
        let required = (registration.required_ports)();
        let state = match ports.subset(required) {
            Ok(subset) => RouteState::new(subset),
            Err(missing) => {
                tracing::warn!(
                    module = name,
                    port = missing.name(),
                    "route group not mounted: a required port is absent"
                );
                report
                    .skipped
                    .push((name, SkipReason::MissingPort(missing.name())));
                continue;
            }
        };
        let routes = (registration.routes)().with_state(state);
        match registration.public {
            Some(public) => {
                public_routers.push((public.rate_class, routes));
                report.public.push(name);
            }
            None => router = router.merge(routes),
        }
        claimed.extend(registration.serves.iter().copied());
        report.served.extend(registration.serves.iter().copied());
        report.mounted.push(name);
    }
    Mounted {
        authenticated: router,
        public: public_routers,
        report,
    }
}
