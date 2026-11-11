Part of [Running Spot](../running-spot.md).

# Adding a served HTTP operation

`spot-http` serves the `/api/v1` operations of `contracts/native-api/openapi.yaml`. A new served
operation group is **two new files and no shared edit**: nothing in `router/mod.rs`,
`service.rs` or `tests/conformance.rs` changes. `build.rs` scans the two directories and
generates the module list.

```
crates/framework/spot-http/src/api/ports/<name>.rs          the port trait and its PortKey
crates/framework/spot-http/src/api/router/routes/<name>.rs  the routes and what they serve
```

## 1. The port

The route file never talks to storage or a service directly; it codes against a port trait,
object safe, async through boxed futures (the shape of `FilesPort`). The host provides the
implementation. The port file declares the trait and a key for it:

```rust
//! The reports port ...
pub trait ReportsPort: Send + Sync { /* boxed-future methods */ }
pub const KEY: PortKey = PortKey::of::<dyn ReportsPort>();
```

`pub const KEY: PortKey` at column 0 is what the scanner looks for. The host adds the
implementation with `ApiServices::with_port::<dyn ReportsPort>(Arc::new(..))`. A fake of the port
belongs in the crate's tests, next to the harness.

## 2. The routes

```rust
pub const SERVES: &[&str] = &["report_list", "report_describe"];   // contract operation ids

pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 3] = [
        ports::reports::KEY,
        ports::permissions::KEY,   // a group that changes data names its permission ...
        ports::audit::KEY,         // ... and audit ports here too
    ];
    &KEYS
}

pub fn routes() -> axum::Router<RouteState> { /* .route("/api/v1/...", get(handler)) */ }
```

All three at column 0 (a file with only some of them stops the build with a message). A file
with none is a plain helper module.

Rules the router enforces and the group relies on:

- **Fail closed.** The group is mounted only when every port in `required_ports()` is present.
  If one is missing, nothing of the group is mounted, the operations answer exactly as an
  unserved operation does (the unrouted `404`, never a `500`), and the start-up log names the
  group and the missing port once (`module`, `port`). Never leave a permission or audit port out
  of `required_ports()` to make a group mount.
- **No double service.** An operation id served by a hand-wired route (`HAND_WIRED_SERVES`) or by
  an earlier group is not served twice: the later group is not mounted and the log names the
  operation. The conformance test also fails on it.
- **Authentication is the default.** The router wraps every registered group in the bearer
  check unless the group declares `PUBLIC` (section "Public groups"). A handler takes the caller identity from the authenticated request exactly as the
  existing handlers do (`build_ctx` in `router/mod.rs` is reachable from the `routes` module);
  it never reads a credential itself. The group's `RouteState` holds only its declared ports.
- Handlers use the shared helpers of the router (`fail`, `envelope`, `take_body`, ...) and the
  error `Code`s of `api/registry.rs`.

## Public groups

A few contract operations carry `security: []` (no bearer): a cookie-only renewal, for example.
A route file serves one by declaring a marker next to the three items:

```rust
pub const PUBLIC: Public = Public { rate_class: RateClass::Auth };   // column 0
```

The rules, all fail closed:

- **The default is authenticated.** A file without `PUBLIC` is behind the bearer check exactly
  as before. No flag, feature or configuration turns the bearer layer off.
- **Only `security: []` operations.** `build.rs` reads `contracts/native-api/openapi.yaml` (a
  plain scan, no YAML dependency) and fails the build, naming the file and the operation, when a
  public group's `SERVES` lists an operation whose contract entry is not exactly `security: []`
  (or is not in the contract), and when a group without `PUBLIC` serves a `security: []`
  operation. `SERVES` of a registering file must therefore be a literal list. A missing contract
  file also stops the build. `tests/routes_registry.rs` unit-tests the check.
- **Exactly the contract's paths.** A public file mounts exactly the contract paths and methods of
  its `SERVES`, with plain literal `.route` calls only; the build fails otherwise. `build.rs`
  tokenizes the file (comments and strings removed first) and accepts it only when `routes()` is
  `fn routes() -> Router<RouteState>` whose body is one expression from `Router::new()` through
  `.route("<literal>", get(..).post(..))`, `.layer(..)`, `.route_layer(..)` and `.with_state(..)`
  (method calls from `get`, `post`, `put`, `delete`, `patch`, `head`, `options` only), and then
  compares the set of (method, path) with the contract's for `SERVES`; `{id}` and `:id` are one
  form. It also refuses the file when it declares `macro_rules!` or `mod`, uses `include!` or
  `include_str!`, invokes a macro inside `routes()` or outside a function body, names `Router`
  anywhere but `routes()` (so a helper cannot take or return one), or renames an import. A
  `.nest`, `.merge`, `.fallback`, `on(..)`, `any(..)`, `MethodRouter::new()` or a computed path
  therefore stops the build: the scanner never accepts a file it does not fully understand, and
  the cost of a refusal is a rewrite in the plain form. `tests/routes_registry.rs` unit-tests it
  and `tests/public_routes.rs` shows a public group with an extra path rejected.
- **What the check also refuses** (each has a test named `bypass_*` in `tests/routes_registry.rs`):
  the identifier `routes` anywhere but `pub fn routes()` at the top level (no decoy, nested,
  re-exported or aliased `routes`); every attribute but `doc`, `allow`, `warn`, `deny`, `expect`,
  `derive`, `must_use` and `inline` (so no `cfg`, `cfg_attr`, `path`, `macro_use`); every glob
  import; the `extern` keyword; a `use` that brings a method name from anywhere but exactly
  `axum::routing::<method>` (or `Router` from anywhere but `axum::Router`); and any local item or
  binding named `get`, `post`, `put`, `delete`, `patch`, `head`, `options`, `on` or `any`; `tracing`, `serde_json` and `json`
  imported from anywhere but the crates of those names, and a logging macro that is not exactly
  `tracing::<level>!` (so a same-crate macro cannot hide a router in its expansion); a `use`
  tree nested deeper than 32 levels; and a `SERVES` that the tokens do not spell as the single
  `pub const SERVES: &[&str] = &[..];` the text scan read (no decoy in a comment).
- **Residual, not checked (review covers them):**
  - a `.layer(from_fn(..))` closure can answer any path, because layers legitimately carry
    middleware; the check reads only what `.route` mounts, so a reviewer must read the layers of
    a public file;
  - `/item/{a}-{b}` mounted for the contract path `/item/{id}` is accepted (it matches a subset
    of what `{id}` does, so it is narrower, not wider);
  - `:id` and `{id}` are one form in the check, although axum 0.8 panics on `:id` at startup, so
    that spelling never survives the first test run.
- **A rate class is mandatory.** `Public` has one field, `rate_class`, not optional. `Auth`
  (the class `auth_login` runs in) allows 20 requests per minute per client address (an IPv6
  address counts as its /64), counted in memory by the router with the clock of
  `ApiServices::with_clock` (monotonic, never the wall clock) and answers `429 api.rate_limited`
  with `Retry-After`. The limit runs before the handler, so before any cookie is read. The
  counter is per rate class and address, shared by every public group of the class, and is
  separate from the failed-sign-in limit the auth service keeps for `auth_login` (per user and
  address). A request without a known address shares one bucket; a full table refuses a new
  address rather than growing.
- **No identity.** The router removes `Authorization` before a public handler runs. The
  handler's `RouteState` holds only its declared ports, as for every group.
- A public group is subject to the same skip rules (missing port, duplicate operation, serves
  nothing); `MountReport::public` names the public groups that were mounted.

`routes/consumer_renew.rs` is the first public group (`auth_consumer_renew`). `tests/public_routes.rs`
walks every non-public group of the real registry and asserts `401 auth.required` without a
bearer, so a new group cannot opt out by accident.

## 3. Error codes

A new error code is a contract change (`x-spot-error-codes`, the `Code` table in
`api/registry.rs`) and a catalogue entry in `crates/framework/spot-errors/catalogue/<name>.toml`.
Both are made by a lead task before the group is written; the conformance test pins their number.
No message carries a key, token, payload or personal data.

## 4. Conformance

`tests/conformance.rs` computes the list of unserved operations: the contract's `/api/v1`
operations minus `served_operations()` (the hand-wired list plus every group's `SERVES`). It
asserts that every served id is a contract operation, that none is served twice, and that
everything served before stays served. Serving an operation therefore removes it from the gap
list by itself, and the walk then needs a request case for it in `case()` (the walk fails with
"no case" otherwise). The conformance harness (`app_with_files`) must provide the group's ports
with fakes so the operation answers there.

`tests/routes_registry.rs` shows the mounted, port-absent, half-wired and duplicate cases with a
fake port and a fake group (`router_with(services, &[Registration { .. }])`).

## What is not moved yet

The document, meta, auth, permission, reveal, file, lookup, print and lifecycle routes stay where
they are in `router/mod.rs` and its `auth`, `resource`, `lifecycle`, `lookup`, `files`,
`permissions` and `print` modules. They share the idempotency and bearer middlewares and
`SharedState`, which a group does not see, so moving them is a separate change with its own
review. The registry runs beside them; `HAND_WIRED_SERVES` is the one hand-kept list, checked
against the contract by the conformance test.
