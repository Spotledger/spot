# The app store server: `spot-store-server`

This is the operator guide for the Spotledger app store as a service (WP-STR-03.T3, ADR-21). It
serves the store API contract (`contracts/store-api`, 0.2.1) to publishers, sites and the review
desk, over the registry core (`spot-registry`), its PostgreSQL index (`spot-registry-pg`) and the
store's own durable state (`spot-store-state-pg`). It is a separate deployable: never part of
`spot serve`, never a site's host, and never on a tenant's request path.

```bash
spot-store-server serve --config /etc/spot-store/store.toml
```

## What it does, and what it never does

- Publishers upload a signed bundle (`POST /store/v1/apps/{name}/versions`). The store verifies
  the publisher's own detached signature, the revocation list, name ownership and that the version
  is above every version already published. It **never signs** a bundle, a certificate or a review.
- A published version enters `pending_review`. The review (`spot-app-review`) runs in a bounded
  worker and its summary (a verdict and counts, never the findings) is stored. **Nothing is public
  until a person approves it** through an operator route. The review never says an app is safe.
- Search, app and version metadata, and bundle download answer for **listed** versions only. A
  version that is under review, rejected or withdrawn answers exactly as an unknown version does,
  except to its owning publisher. The listing fails closed: only an explicit `listed` row in the
  state store lists a version, so a crash or a database that cannot answer hides, never exposes.
- Sites verify every bundle themselves. The download carries the stored detached signature and the
  publisher certificate in the headers `X-Spot-Signature` and `X-Spot-Certificate` (standard base64
  of the JSON documents `spot-bundle-signing` defines), and its `ETag` is the canonical digest. No
  header or field says the bytes are verified, safe or trusted.
- The only key the server holds is the **store key**. It signs revocation lists (and, from
  WP-STR-08, licences). Nothing else.

## Configuration

A TOML file. An unknown key is an error. **No secret is a setting:** keys are read from files, and
on Unix a key file that anyone but its owner can read is refused (use mode `0600`).

```toml
listen = "127.0.0.1:8480"                   # host:port; put TLS and a proxy in front
blob_dir = "/var/lib/spot-store/blobs"      # content-addressed bundles, private to the service
temp_dir = "/var/lib/spot-store/tmp"        # in-flight uploads, mode 0700, cleaned at start
store_key_file = "/etc/spot-store/store.key"        # 64 hex digits: the Ed25519 seed
admin_key_file = "/etc/spot-store/admin.secret"     # 32 to 256 printable characters
database_url_file = "/etc/spot-store/database.url"  # or database_url = "..." when it holds no password
paid_apps = []                              # app names whose bundles need a site licence

[[trust_roots]]                             # roots publisher certificates chain to
public_key_file = "/etc/spot-store/roots/publisher-root.pub"   # base64 of the 32-byte public key
valid_from = 0
# valid_to = 1900000000

[limits]
max_bundle_bytes = 67108864      # at most the contract's 64 MiB
max_manifest_bytes = 65536       # at most the contract's 64 KiB
max_text_part_bytes = 16384
max_json_bytes = 8192
max_credential_bytes = 8192
upload_timeout_secs = 300

[workers]                        # the server's own OS threads for blocking calls
threads = 8
queue = 64

[database]
pool_size = 16
max_in_flight = 12               # must exceed workers.threads + review.workers + 1
queue_capacity = 64

[revocation]
refresh_secs = 21600             # reissue every 6 hours, 1 hour to 7 days
lifetime_secs = 86400            # next_update is issued_at plus this, 1 hour to 7 days

[review]
workers = 2
queue = 16
timeout_secs = 120
# [review.profile] holds spot-app-review's own limits.

[rates]                          # each rule: max calls per window_secs
public_per_address = { max = 600, window_secs = 60 }
publish_per_key = { max = 10, window_secs = 60 }
publish_per_address = { max = 30, window_secs = 60 }
review_per_key = { max = 120, window_secs = 60 }
admin_per_address = { max = 120, window_secs = 60 }
auth_failures_per_address = { max = 20, window_secs = 300 }
```

The server **fails to start** when the store key, the operator secret or a trust root file is
missing or unreadable, when the database cannot be prepared, or when a setting is out of bounds.
The error names the setting, never a value.

The store key's public half is a trust root of the registry's own checks and the key sites pin for
the revocation list. Publisher certificates chain to the roots in `[[trust_roots]]`.

## Authentication

- **Publisher keys.** `Authorization: Bearer spk_<64 hex digits>`. The operator issues them (below);
  only their SHA-256 is stored. The presented key is hashed and the hashes are compared in constant
  time, also when no such key exists. A key is bound to one publisher id: an upload signed by, or
  certified for, another publisher is `403 store.forbidden` and stores nothing. A key that is
  unknown, revoked or malformed answers the same `401 store.unauthorized`. Failed checks are counted
  per source address; past the limit the address gets `429` before it reaches the store.
- **Site licences.** For a paid app (`paid_apps`), `app_bundle_download` needs a site licence token
  as the bearer: a `spot-license` envelope signed by the store key, covering the app. Missing or
  insufficient is `403 store.not_entitled`. A missing version is `404` either way. A free app
  ignores any licence. A `spk_` bearer on a download is a publisher key, so an owner fetches their
  own unlisted bundle.
- **Operator.** `Authorization: Bearer <operator secret>`, compared in constant time on SHA-256,
  with its own rate class and failure counter. Operator routes are not part of the public contract.

## Operator routes

All are `POST` with a JSON body unless noted, behind the operator credential, and audited (a refusal
is audited too, with a `refused: <code>` reason).

| Route | Body | Effect |
| --- | --- | --- |
| `/admin/v1/publishers/{id}/keys` | `{"label"}` | Issues a publisher key; the key is in the `201` answer once and never again. |
| `/admin/v1/publisher-keys/{key_id}/revoke` | none | Revokes one publisher key. |
| `/admin/v1/publishers/{id}/revoke` | `{"reason"}` | Suspends the publisher, revokes all their keys, reissues the revocation list. There is no reinstatement. |
| `/admin/v1/signing-keys/{key_id}/revoke` | `{"reason"}` | Lists a signing key as compromised and reissues the list. |
| `/admin/v1/bundles/{digest}/revoke` | `{"reason"}` | Lists a bundle digest and reissues the list. |
| `/admin/v1/apps/{name}/versions/{version}/approve` | optional `{"reason"}` | `pending_review` to `listed`. |
| `/admin/v1/apps/{name}/versions/{version}/reject` | optional `{"reason"}` | `pending_review` to `rejected`. |
| `/admin/v1/apps/{name}/versions/{version}/withdraw` | optional `{"reason"}` | `listed` to `withdrawn`. |
| `/admin/v1/reviews/{id}/rerun` | none | Queues a review again (after a failure, a timeout or a restart). |
| `GET /admin/v1/audit?after=&limit=` | none | The audit log, oldest first. |
| `GET /metrics` | none | Prometheus text. |

The revocation routes call the issuer synchronously: the new list is signed, recorded and served
before the route answers, and the server's own publish checks use it from the next request.

## The revocation list

`GET /store/v1/revocations` serves `spot-bundle-signing`'s fresh (V2) list, built, signed and encoded
only by that crate, with the store key behind its `RevocationSigner` port. `ETag` is the generation
and `Cache-Control` is `max-age=300`; every caller gets the same bytes.

- Entries: revoked publishers, revoked signing keys (listed as compromised, which refuses every
  signature the key ever made) and revoked bundles. `effective_from` is the revocation time.
- A list is issued at start when none is stored, or the stored one is due, stale or different;
  whenever an entry changes; and by the maintenance task every `refresh_secs`. The last list is kept
  in the state store, so a restart serves the same bytes and the same `ETag`.
- `/health` answers `503` when the served list has passed its `next_update` (a refresh that keeps
  failing), so a monitor sees it before sites start refusing installs.
- Revoked publishers are read from the `revoke_publisher` entries of the state store's append-only
  audit log (neither port lists suspended publishers).

## Blocking calls (ADR-082 class C)

The registry, the PostgreSQL index and the state store are synchronous, and the shared
`spot-pg-bridge` refuses any caller on a tokio runtime thread, `spawn_blocking` threads included
(`pg_bridge.wrong_thread`). So the server owns a small pool of **plain OS threads** fed by a bounded
queue (`[workers]`); a handler sends a closure and awaits a oneshot reply, and no handler thread ever
blocks. A full queue is `503 store_server.unavailable` with `Retry-After`; the bridge's
`pg_bridge.busy`, `pg_bridge.timeout` and `pg_bridge.connect` map to `503` too. Keep
`database.max_in_flight` above `workers.threads + review.workers + 1` so the bridge itself never
refuses a call as busy. The server is built on the main thread, before the async runtime starts.

`Registry::publish` is async but never waits on anything outside its thread (the filesystem blob
store does blocking I/O inside the future), so a worker drives it with a parker-based `block_on`.

## Review workers

`spot_app_review::review` is CPU bound and synchronous. It runs on its own OS threads with no
network (the no-network probe) and a wall-clock limit (`review.timeout_secs`). The review is
cooperative, so a review that overruns cannot be stopped: it is recorded as `failed` with
`store_server.review_timeout`, and its thread keeps its permit until the review returns. When every
permit is held by such a stuck review, new publishes are refused with `503` and `/health` says so.

## Uploads

The multipart body is read by axum's `multipart` support (`multer`); the server writes no parser.
The bundle part is streamed to a private temporary file and cut off the moment it passes
`max_bundle_bytes` (`413 store.too_large`); a declared `Content-Length` above the cap is refused
before the body is read. Text parts are capped separately. The order of parts is free; a repeated
`bundle` or `signature`, an unknown part, a certificate part that is not `text/plain`, more than 8
certificates, or a missing part is `400 store.invalid_request`. This store takes a chain of exactly
one certificate (the leaf, issued by a trust root). The temporary file is removed in every case.
Blob paths come only from the canonical content digest; no request value is ever a path.

## Logs, audit, metrics

One JSON line per request on stderr: time, request id, method, path, status, duration, source
address. A query string, a header or a body never reaches the log. The audit log (state store)
records publish, yank, withdraw, revoke and approve actions with the actor, subject and reason, and
never a key, token, bundle or request body. If an audit write fails after the action it records, the
server logs `audit_failed`, counts it (`spot_store_audit_failures_total`) and answers as it
would have. The entry of a revocation or a listing change is part of the action: if it cannot be
written the route answers `503` (before the revocation list is reissued) and the action can be
repeated. `/metrics` carries totals only; no label holds a key, address or publisher.

## Known limits

- One process owns the revocation cache: a second process on the same database would serve its own
  generations (it never goes backwards: generations come from the database).
- Search offers an app by its newest listed-eligible release: an app whose newest release is still
  under review is not offered until it is approved, even when an older release is listed. A search
  page refills from at most 5 registry pages and then returns what it has with a cursor.
- A review that was queued when the server stopped stays `queued`; queue it again with the rerun
  route.
- The source address is the peer address. Behind a proxy, rate limits per address are per proxy.

## Running the tests

```bash
cargo test -p spot-store-server
SPOT_TEST_PG_URL=postgres://spot:spot@localhost:5432/postgres cargo test -p spot-store-server --test postgres
```

Everything except `tests/postgres.rs` runs on memory doubles and a real filesystem blob store;
`tests/postgres.rs` skips (with a message) without `SPOT_TEST_PG_URL` and fails when
`SPOT_REQUIRE_PG=1` is set and the URL is not.
