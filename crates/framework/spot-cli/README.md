# spot — initial product host

```sh
cargo build -p spot-cli
spot new-site demo --root ./runtime --collation en
spot serve --root ./runtime
# Local development only; every configured bind address must be loopback:
spot serve --root ./runtime --dev
```

Create `runtime/common.toml` before provisioning:

```toml
apps = ["../my-app"] # each directory contains doctypes/*.toml

[server]
bind = ["127.0.0.1:8000"]
max_body_bytes = 10485760
request_timeout = 60
shutdown_grace = 30
trusted_proxies = []
# fallback_site = "demo"

[postgres]
# Supply an administrative PostgreSQL 18 URL through this protected local file.
# The role needs CREATEDB; the host never logs the URL.
admin_url = "postgres://spot@127.0.0.1:5432/postgres"
pool_max_size = 16

[telemetry]
filter = "info"
format = "json" # or pretty
service_name = "spotledger"
node_id = "node-0"
# [telemetry.otlp]
# endpoint = "http://127.0.0.1:4318/v1/traces"
# sample_ppm = 1000000
```

Connection URL defaults (TLS, connect timeout and application name) are parsed by
`spot-postgres::PgConfig`; the site's database replaces only the URL's database.
Protect `common.toml` since it can contain a database password. Use verified TLS
(`sslmode=require`) for remote PostgreSQL, and a TLS reverse proxy for remote HTTP.

`new-site` validates config and app metadata before side effects. Every site starts with
the platform app first and, unless `--bare` is given, every other app in the catalogue.
A site name is a lowercase single DNS label up to 63 bytes; hyphens map to underscores in
its database name. Provisioning reserves its directory exclusively, refuses any existing
directory or database, creates a UTF-8 ICU database, prepares metadata additively, and
seeds the platform app: the Administrator, the system roles and the Administrator's
`Tenant Administrator` assignment. It prints the Administrator's **one-time first
sign-in secret once** on stdout. Sign in with it as the password, then `POST
/api/v1/auth/password` to choose a password; the secret stops working. Only its Argon2id
verifier, expiry and immutable user id are written to `site.toml` (mode 0600 on Unix).
Failed provisioning deliberately never drops a database or overwrites a directory: inspect
the failure and repair through administration before retrying.

### The one-time secret

The first sign-in secret of the Administrator (`new-site`, `reset-admin`) and of an invited
user (`add-user`) is a password, never an API key: sent as `Authorization: Bearer` it is
refused.

- **It expires.** `site.toml` records `expires_at` (Unix seconds) on the verifier: 72 hours
  by default, `--secret-hours N` on `new-site`, `add-user` and `reset-admin` (1 to 336, so 14
  days at most). After that it is refused as a wrong password (same answer, same hashing
  work) and `spot reset-admin <site> --yes-i-am-the-operator` issues a new one. A secret
  written by an earlier version has no expiry and is treated as expired; `spot serve` and
  `spot check` say so on the command line and nothing extends it silently.
- **Until a password is chosen the session can do almost nothing.** A session started with the
  secret may only `POST /api/v1/auth/password`, `POST /api/v1/auth/logout` and `GET
  /api/v1/auth/session` (which carries the display name). Every other request, on any route
  or method, is answered `403` with the code `cli.password_change_required` before it reaches
  the router: one gate in the dispatcher, an allow-list on method and exact path. The gate
  also covers an API key whose owner has not chosen a password, and it keeps covering the
  session if the operator runs `set-password` meanwhile.
- **The change replaces the session.** `POST /api/v1/auth/password` from a restricted session
  answers `200` with a new session (`data.token`, a new session id); the old one is revoked,
  as is every other session of the user. An ordinary password change still answers `204`.
- **Scripted bootstrap** (replaces the old bearer secret): `spot new-site <site>
  --admin-api-key-file <path>` also writes a separately generated Administrator API key to a
  new file (mode 0600; an existing path is refused before anything is created). The key
  expires (24 hours; `--admin-api-key-hours N`, at most 168) and is limited by the same gate
  until `spot set-password` has run. Only its digest and expiry are stored.
- **Not logged.** The secret is printed once on stdout and appears in no log line, `Debug`
  output or audit record.

Existing development sites: run `spot check`; for each site it names a secret written by an
earlier version, run `spot reset-admin` (or `spot set-password`). Old API keys of such a user
stay in `site.toml`, but they can only change the password until then.

Each `sites/<site>/site.toml` has `site`, `database`, `collation` (default `en`),
`aliases` (default empty), and `[[api_keys]]` containing `key_id`, hex `digest` and UUIDv7
`user`. Aliases must be unique across sites. Unknown keys and independent config errors
are rejected together, without echoing values from secret-bearing files. Syntax errors
are reported by file and byte span. Metadata files use the `spot-meta` definition format;
this host loads all configured app DocTypes, not native controller code.

Site resolution happens before dispatch. Each site has its own metadata (platform first),
merged permission snapshot, engine, pool, readiness check and idempotency store. Every
`/api/v1` request is authenticated before the router sees it: a session token from
`POST /api/v1/auth/login` or a site API key. Missing or invalid credentials answer `401`.
`/api/v1/auth/login` is the one open route and is throttled; `/api/health/live` and
`/api/health/ready` remain public. The engine's `PermissionAuthorizer` enforces the rules
for every read and write, and list queries compile the caller's read rights with
`spot-perm-query` before storage is touched. A user's role assignments are read from the
platform records on every request, so an assignment change is effective immediately.

`--dev` supplies an in-memory Administrator credential only to requests with **no**
Authorization header; an invalid supplied credential is still refused. It cannot bind
non-loopback addresses. SIGINT/SIGTERM stop accepting and drain connections within the
configured grace period. Telemetry is kept alive through shutdown.

### The identity audit trail, and what is held per process

Every sign-in outcome is written to `sites/<site>/identity-audit.log`: one JSON line per
record, mode 0600, synced per record, rotated at 16 MiB with three older files kept (so at most
64 MiB). The host refuses to serve a site whose audit file cannot be opened. Records:
`sign_in_succeeded`, `session_started`, `sign_in_failed` (reason class), `sign_in_attempt` and
`sign_in_refused` (the presented name, redacted and cut at 128 characters, plus the salted
source digest), `lockout_started` and `lockout_ended`, `password_changed`,
`password_change_failed`, `session_revoked`. A record has typed, bounded fields and no free text:
no password, hash, token, one-time secret or raw address can be in one. A sign-in whose success
cannot be audited (after one retry) is refused and its session revoked. A failure that cannot
be audited is logged at error level (kind only) and the caller still gets the ordinary refusal,
so an audit outage reveals nothing. A lockout stays in force whether or not its record could
be written. Unauthenticated noise is bounded: at most 600 failure records and 60 lockout
records per minute; what is dropped is counted and written as one `failures_suppressed`
record. The file is a durable log, not yet the tamper-evident chain; shipping it off the host
is the operator's job until the chain store lands.

**Sessions and sign-in throttle counters are held in the process, not in the database.** A
restart signs every user out and forgets every lockout; the host says so in its log when it
starts a site. Both are bounded: a user holds at most the session policy's `max_per_user`
sessions, and the throttle tracks at most 50,000 keys, evicting the oldest key that is not a
live lockout; if every tracked key is a live lockout, sign-ins are refused rather than a lockout
being forgotten.

## Everyday commands (WP-PRD-01.T2)

`spot new-site <site> --bare` creates a site with only the platform app (the Administrator and
the system roles are seeded, and the command prints the Administrator's one-time first sign-in
secret once); plain
`new-site` keeps the first host's behaviour of hosting every directory in `common.toml`
`apps`. A bare site records its install list in `site.toml` (`apps = ["platform", ...]`, the
platform always first). `common.toml` `apps` is then the catalogue of app directories a site
may install, and listing `apps/platform` there makes the complete platform app (including the
`file` DocType other apps need) the one that is hosted instead of the smaller embedded copy.

| Command | Does |
| --- | --- |
| `install-app <app-or-bundle> [--site S] [--apply] [--seed]` | a packed bundle file is verified against the site's `[trust]` roots and revocation list before anything is written; an app directory or catalogue name is refused unless `[trust] require_signed = false` (development). Validates the app directory (manifest, DocTypes, permissions, controller) and reports every error; plans the schema change and prints it with risk classes; with `--apply` applies it online, records the install, and with `--seed` loads `seed.toml` (idempotent). A second run is a no-op. |
| `list-apps [--site S]` | installed apps per site and the apps available in the catalogue. |
| `migrate [--site S] [--dry-run] [--apply] [--plan-hash H]` | the same plan engine against the installed apps' current metadata. A refused (destructive) step is printed with its reason and the exit code is non-zero. `--plan-hash` refuses to apply a plan other than the reviewed one. |
| `add-user <site> <email> [--name N] [--role R...] [--secret-hours N]` | creates an invited user and prints a one-time first sign-in secret once (only its verifier and expiry are stored; it is a password for choosing a password, not an API key); `set-password` activates the invitation. |
| `set-password <site> <user> [--password-stdin]` | reads the password with echo off (twice) or from standard input, never from an argument; prints every violated policy rule. |
| `reset-admin <site> --yes-i-am-the-operator [--secret-hours N]` | replaces the Administrator's credentials with a new one-time secret and appends an audit record to `sites/<site>/audit.jsonl`. |
| `check` | validates `common.toml`, every `site.toml`, the app directories and each site's PostgreSQL connection, reporting every problem at once; exit 0 only when serving would start. |
| `reload [--site S]` | asks a running host to rebuild that site's metadata and permission snapshot. SIGHUP does the same for every site on Unix; the file marker works on Windows too. In-flight requests finish on the old snapshot. |

`--root` can also be set with `SPOT_ROOT`. Nothing here ever applies a schema change without
`--apply` or runs a destructive step; destructive changes belong to the expand and contract
path. Credential material (API key digests and Argon2id password verifiers) is kept in
`site.toml` until the secrets service can hold the platform's `secret` fields (WP-SEC-10).
Sign-in, sessions, the throttle and the permission engine are live in `serve`; API keys
remain accepted for scripts (the one-time secret is not one: see above).

## Verification

```sh
cargo fmt --all -- --check
cargo clippy -p spot-cli -p spot-postgres --all-targets -- -D warnings
cargo test -p spot-cli
SPOT_REQUIRE_PG=1 cargo test -p spot-cli --test smoke --test commands --test readme
SPOT_REQUIRE_PG=1 cargo test -p spot-postgres --features testing --test admin
```

The real-PG tests use the backend's `TestDb` harness and `SPOT_TEST_PG_URL` (a test
administrator URL). With `SPOT_REQUIRE_PG=1`, a missing server configuration fails rather
than silently skipping. The HTTP test exercises the actual `spot new-site` executable,
then in-process serving on an ephemeral TCP port, first sign-in and password change,
site isolation/aliases, bearer and session auth, child-row create/update/post, idempotent
replay, read-only rejection, outbox ordering and command ids, readiness, dev-mode
credential policy, and graceful shutdown. `tests/e2e.rs` proves the permission matrix over
HTTP: owner-scoped writes, hidden fields, list filtering, disabled users, logout, expiry,
lockout, API keys, reload and a bad-site `503`. Provisioned
test database names match the harness's UUIDv7 TTL sweep format; no SQL or PostgreSQL
driver enters `spot-cli`, including its tests.
