Part of [Running Spot](../running-spot.md).

# Functions

`POST|GET /api/v1/method/{app}/{module}/{function}` (`function_call`, `function_call_get`) calls a
module-level whitelisted function (ADR-16). It is not a way to call any method by name: only a
function an installed app declares in its manifest can be reached, and it runs for the caller, never
for the host. The rules live in `spot-functions`; the hosting module is
`crates/framework/spot-cli/src/services/function.rs`.

## Declaring a function

An app declares each function in its manifest (`app.toml`):

```toml
[[functions]]
module = "reports"
name = "total"
http = ["GET"]        # the verbs it answers; without `http` it is not reachable over HTTP
guest = false         # true: a caller with no credential may call it (read verbs only)
```

A guest function must name its verbs. The function's address is `<app>/<module>/<name>`; an app
declares only functions under its own name.

## Registering a native handler

The binary that links the app registers the implementation before the host starts, in
`crates/framework/spot-cli/src/natives.rs`:

```rust
natives::register_function("sample", "reports", "total", Arc::new(MyHandler));
// or, with a rate limit and an argument schema the manifest does not carry:
natives::register_function_with("sample", "reports", "total", spec, Arc::new(MyHandler));
// or a closure over plain values (`FunctionCall`), for a probe or a test:
natives::register_function_fn("sample", "reports", "total", spec, |call| Box::pin(async move { ... }));
```

Registering is not declaring: a handler no manifest declares is never reachable, and a declared
function without a handler answers `404 method.not_found` (the host logs the list once at
start-up). Script and WASM functions (EXT-06, EXT-05) will implement the same `FunctionHandler`
trait and be contributed through the same builder; no new path is needed.

The handler receives the caller (a user id, or nobody for a guest), the verb, the site and the
`Idempotency-Key`, and the arguments. It has no wider authority than that: a handler that reads or
writes data does so through the engine for that user, so the caller's rights apply.

## What a call checks, in order

1. The function exists, and an anonymous caller asked for a `guest` one: otherwise
   `404 method.not_found` (`function_code` `function.not_found`). A guest never learns which
   functions exist.
2. The verb: `405 api.method_not_allowed`, `details.allow` listing the verbs it answers.
3. A guest on a verb that changes things (`POST`, `PUT`, `PATCH`, `DELETE`): `401 auth.required`.
4. The rate limit: `429 api.rate_limited` with `details.retry_after` and the `Retry-After` header
   (whole seconds). The bucket is per function and caller; every guest shares one bucket.
5. The argument schema (`type`, `required`, `properties`, `items`): `400 api.invalid_request`; the
   explanation names the problem and the path, never an argument value. On a `GET` each query value
   is read by the schema first (`integer`, `number`, `boolean`).
6. The handler. Its own errors pass through as they are (a field error is `422`, a refusal is
   `403 permission.denied`), and a handler returning an error is never reported as a success.

`details.function_code` carries the finer `function.*` code of the first five. Calling with no
credential is a guest call; a credential that does not resolve, or a session that has not chosen
its password yet, is refused, never treated as a guest. The `admin` app's own methods (the
customise admin API, `GET` for `list` only) are served by the host before this registry.

## Operating it

- **Level: required**, with nothing to degrade: the registry is in memory and is built when the site
  starts and again on every reload. Two apps whose functions conflict (a duplicate address, an
  override of nothing, an override without a reason) stop the site start with every conflict listed;
  on a reload they fail the reload and the previous registry keeps serving.
- **Settings**, `[services.function]` in `common.toml` (unknown keys are refused):

  | Key | Default | Meaning |
  | --- | --- | --- |
  | `enabled` | `true` | `false`: the endpoint answers `404 method.not_found`, as before |
  | `guest_permits` | `30` | calls a guest function with no limit of its own allows per window |
  | `guest_window_seconds` | `60` | the guest window |
  | `idle_after_seconds` | `600` | a bucket idle this long is dropped when evicted |
  | `bucket_cap` | `10000` | more buckets than this make the next call evict idle ones |

- **Listing**: `FunctionService::list` returns every function with its declaring app, verbs, guest
  flag, limit, argument schema and override (who and why); the admin app's `list` serves it.
- **Eviction**: idle buckets are dropped by the job `platform:function_evict_idle`; the host
  registers the handler, a schedule enqueues it. There is no timer of its own, and `bucket_cap`
  bounds memory when no schedule runs.
- **Audit**: target `spot::audit::function` logs a line per call that reached the registry: caller,
  function address, verb, outcome code. Never an argument, a result, a token or a header.
