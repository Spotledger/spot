# Running Spot

`spot serve` runs the framework services for every configured site from one process. This
guide is for the operator: how to bring up a first site, what runs, what listens, where the files and logs are, what
`common.toml` sets, and what shutdown does.

## Your first site

This walkthrough takes a fresh clone to a running site with nothing but the repository. Every
command was run in order, in an empty directory, on Linux, on 10 November 2026. Windows is not
yet checked.

**1. Prerequisites.**

- The Rust toolchain pinned in `rust-toolchain.toml` (install it with `rustup`; the first `cargo`
  command fetches the pinned version).
- PostgreSQL 18 that you can reach with a role allowed to create databases (the `spot` role below
  is a superuser). `spot new-site` creates one database per site and nothing in this walkthrough
  needs more than that.
- For the repository's own tests, PostgreSQL must also offer the five extensions `postgis`,
  `btree_gist`, `pg_trgm`, `unaccent` and `vector` (pgvector). A first site does not create them,
  so you can skip them until you run tests.

If you have Docker, the repository starts a throwaway PostgreSQL 18 with all five extensions
(about a minute, the first time it installs PostGIS and pgvector inside the container):

```sh
SPOT_PG_CONTAINER=spot-pg SPOT_PG_PORT=5432 tools/ci/start-postgres.sh
# last lines it prints:
#   export SPOT_TEST_PG_URL=postgres://spot:spot@127.0.0.1:5432/postgres
#   export SPOT_REQUIRE_PG=1
```

The user and password (`spot` / `spot`) are for this throwaway server only. Without Docker, any
PostgreSQL 18 will do; put its URL in `admin_url` in step 3. Two different settings name a
PostgreSQL URL, and they are not interchangeable:

| Setting | Read by | Used for |
| --- | --- | --- |
| `SPOT_TEST_PG_URL` (environment variable; `SPOT_REQUIRE_PG=1` makes missing PostgreSQL a failure) | `cargo test` in crates that need a database | tests only; the `spot` binary never reads it |
| `[postgres] admin_url` in `common.toml` | `spot new-site`, `check`, `serve` | the running system: the URL of a role that can create databases |

**2. Build the CLI.**

```sh
cargo build -p spot-cli --locked     # about 3 minutes from a cold cache
export SPOT=$PWD/target/debug/spot   # adjust if CARGO_TARGET_DIR is set
export REPO=$PWD
```

**3. Make a configuration root.** `spot` reads `<root>/common.toml`, where `<root>` is the
current directory or the `--root` option (or `SPOT_ROOT`). Without that file, every command
stops with `cannot read ./common.toml`. Make a new, empty directory for the root, and copy the two
apps in the repository into it:

```sh
mkdir -p ~/spot-root/apps && cd ~/spot-root
cp -r "$REPO/apps/platform" "$REPO/apps/sample" apps/
```

Then write `common.toml`:

```toml
# Each entry is an app directory (relative to this file, or absolute) that holds a doctypes/
# directory. The list must not be empty.
apps = ["apps/platform", "apps/sample"]

[server]
bind = ["127.0.0.1:8080"]

[postgres]
# A role that can create databases. Use the URL of your own server here.
admin_url = "postgres://spot:spot@127.0.0.1:5432/postgres"

[trust]
# The default, written out. A directory install (`spot install-app sample`) is refused while this
# is true; see "Installing a signed app bundle". It does not affect the apps listed above, which
# are loaded as they are when the site starts.
require_signed = true
```

`apps` is required. A listed directory without a `doctypes/` directory is refused
(`apps: missing doctypes directory in ...`). `[server]` and `[postgres] admin_url` are required
too; everything else in this file has a default (see "`common.toml` services"). The default
`server.bind` port is 8000; this walkthrough uses 8080 because 8000 is a popular choice for other
tools.

**4. Create the site and check it.**

```sh
"$SPOT" new-site acme
"$SPOT" check
"$SPOT" list-apps
```

`new-site` creates the database `acme`, the folder `sites/acme/` and the Administrator, and prints
the Administrator's first sign-in secret **once**: copy it now. It is good for 72 hours and is
only used to choose a password, so it is not an API key. A lost secret is replaced with
`spot reset-admin acme --yes-i-am-the-operator`. `check` prints `ok: 1 site(s) ready to serve`.

**5. Serve and check health.**

```sh
"$SPOT" serve                  # stays in the foreground; use a second terminal for the next line
curl -s http://127.0.0.1:8080/api/health/ready
```

The answer is `{"checks":[...],"status":"ready"}`, and every check says `"ok":true`. Stop the
server with `Ctrl+C` (see "Shutdown").

**6. Remove the site again.** The database stays after you delete the folder. To start over:

```sh
docker exec spot-pg psql -U spot -d postgres -c 'DROP DATABASE acme'   # or: psql "<admin_url>" -c ...
rm -rf sites/acme
```

### Start-up warnings you can ignore

`serve` writes JSON log lines to standard error. A first site prints four warnings like this:

```
"level":"warn","target":"spot_http::api::router::routes","message":"route group not mounted: a required port is absent","module":"notices", ...
```

The modules are `consumer_renew`, `events`, `notices` and `report`. These are expected: each names
an optional HTTP route group whose backing service your configuration does not provide, so the
host leaves those routes out (they answer `404`) and carries on. They are not errors, and
`/api/health/ready` still says `ready`. Two other notices are normal at start-up too: that the
sessions and the sign-in throttle are held in this process (a restart signs everyone out), and
that no `[services.secrets]` table is set, so secret fields cannot be written.

### First-site troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| `cannot read ./common.toml` (or `<dir>/common.toml`) | There is no `common.toml` in the current directory. `cd` to your root, or pass `--root <dir>` (or set `SPOT_ROOT`). |
| `apps: missing doctypes directory in apps` | A path in `apps` is not an app directory. Each one must hold a `doctypes/` directory: copy `apps/platform` and `apps/sample` from the repository as in step 3. |
| `invalid site name` | A site name is a lower-case host name (letters, digits, hyphens and dots, no leading or trailing hyphen); `Acme` and `ac_me` are refused. |
| `postgres.admin_url: cannot reach PostgreSQL` | `postgres.admin_url` is wrong, or PostgreSQL is not up (`pg_isready -h 127.0.0.1 -p 5432`). Its role must be allowed to create databases. The URL is never printed back. |
| `failed to bind 127.0.0.1:8000` | Another program owns that port. Change `server.bind`. Check the answer you get: another web server on the port can make `curl` look like a Spot error. |
| `curl` prints nothing, or `Connection refused` | `serve` is not running, or runs on another port. |
| `route group not mounted: a required port is absent` | Expected for an optional service you did not configure; see the previous section. |
| Tests that need PostgreSQL fail or skip | `SPOT_TEST_PG_URL` is not set. `DATABASE_URL` is not read. Set `SPOT_REQUIRE_PG=1` so a missing database is a failure. |
| `spot install-app sample` is refused with `cli.directory_unsigned` | `require_signed` is true. Install a signed bundle, or set `require_signed = false` on a development site. |

## Start, stop, check

```sh
spot new-site acme                          # creates sites/acme and its database
spot --root /srv/spot serve                 # serves every site under sites/
spot --root /srv/spot check                 # validates configuration and sites, touches nothing
spot --root /srv/spot reload acme           # rebuilds acme's runtime without a restart
```

`serve` binds the addresses in `common.toml` `server.bind` (default `127.0.0.1:8000`).
`SIGINT` or `SIGTERM` starts the shutdown described below; `spot reload <site>` (or `SIGHUP`,
which reloads every site) rebuilds a site's engine and router in place — the session and
throttle stores survive, and requests already running finish on the old runtime.

## What runs, per site

Every site gets one service assembly. The services register through the registry
(`crates/framework/spot-cli/src/services/mod.rs`), each at an order number: the framework's own
services are listed in the `BUILTIN` table in `crates/framework/spot-cli/src/host.rs`, and a module
file in `src/services/` declares its own `ORDER`. The table below is the construction and start
order, and `Order` is that number. Shutdown runs it in reverse.

| Order | Service | Level | What it does | Fails the site? |
| --- | --- | --- | --- | --- |
| 100 | `storage` | required | The site's PostgreSQL pool; its check is the pool's health. | yes |
| 200 | `auth` | required | API keys and password verifiers loaded; the site's sign-in. | yes |
| 300 | `metadata` | required | The live metadata snapshot, its version store and the additive schema plan. | yes |
| 400 | `customise` | required | Customisation recovery and the served-metadata reconcile; without it the site would serve base metadata over a database that holds customised tables. | yes |
| 500 | `participants` | not a service | The write participants (audit, owner check, edge) are built before the engine exists, in `build_site`; they have no level and no check that can fail. Readiness reports `participants:<site>` (always `ok`); a site that cannot build them refuses to start. | yes (at build) |
| 600 | `jobs` | required | The job worker and the outbox intake, under the site's coordinator lease, on the durable store. | yes |
| 600 | `scheduler` | required | The schedule leader: manifest schedules become jobs. | yes |
| 700 | `mail` | optional | The `platform:send_mail` job handler and its ports. | no — degrades |
| 800 | `realtime` | optional | The feed pump; `GET /api/v1/realtime` (WebSocket). | no — degrades |
| 900 | `files` | required in the cluster profile, optional otherwise | The file provider's folder takes a write; `/api/v1/files` is served by `spot-http` over the file service. | cluster only |
| 900 | `print` | optional | The renderer makes a page; `GET /api/v1/print/{doctype}/{id}` is served by `spot-http`. | no — degrades |
| 900 | `telemetry` | optional | `GET /metrics`. | no — degrades |
| 1000+ | new modules | per module | A new module declares `pub const ORDER: u32` of 1000 or more. | per module |

How the order is applied:

- Shutdown runs the reverse of this order.
- Ties on `ORDER` break by module name, so the order is the same on every machine (`files`,
  `print`, `telemetry` at 900 start in that order).
- A module that needs another module's port must have a larger `ORDER` than the provider:
  `customise` takes the `metadata` port, so it is 400 after 300.
- The jobs module registers first, but its worker and scheduler are built by a finaliser that
  runs after every module has registered. That is why `mail` (700), which is registered after
  `jobs` (600), can still contribute its `send_mail` handler, and why a later module may
  contribute a job handler too.
- Change of 3 November 2026: `realtime` now starts before `files`, `print` and `telemetry`
  (it used to follow them). Nothing depended on the old relative order. Checked in
  `crates/framework/spot-cli/src`: the only ports are `LiveMetadata` (provided by `metadata`,
  required by `customise`) and the file service (required by `mail`, provided by the host
  before any module runs, so it does not depend on the `files` service's order); `realtime`,
  `print` and `telemetry` neither provide nor require a port, and no service's check waits on
  another's readiness.

The installed apps' native controllers are not a service: they are installed into the site's
engine when its runtime is built (see "Apps, controllers and app permissions").

A **required** service that fails its start-up check stops that site: the site answers `503`
with `server.unavailable` and the other sites keep serving. An **optional** service that fails
or stops degrades the site: `/api/health/ready` stays `200` (so a load balancer keeps it) and
its `checks` entry carries `degraded: <service> (<reason>)`.

```
GET /api/health/live    {"status":"live"}
GET /api/health/ready   {"status":"ready","checks":[{"name":"acme","ok":true,"detail":"degraded: telemetry (metrics are disabled in common.toml)"}]}
```

## Ports and endpoints

| Path | What |
| --- | --- |
| `/api/v1/*` | The native API. One credential per request: a session token or an API key. |
| `/api/v1/auth/login` | The only open route. |
| `/api/v1/realtime` | The WebSocket route (`spot.v1`). It carries no bearer: the first message, `auth`, does. |
| `/api/v1/files`, `/api/v1/files/{id}` | Upload and download (`spot-http` routes over the file service). |
| `/api/v1/print/{doctype}/{id}` | A PDF of one document, made as the caller (`?format=` names a print format). |
| `/metrics` | Prometheus text. An administrator's credential (see "Reading `/metrics`") unless `services.metrics.require_auth = false`. |
| `/api/health/live`, `/api/health/ready` | Liveness and readiness, no credential. |

## Reading `/metrics`

The counters are **per process** and cover every site that process serves (request totals,
statements, cost counters), so `/metrics` is an operator surface, not a user feature.

- **Who can read it.** With `require_auth = true` (the default), only a user who holds the
  `Tenant Administrator` role (the role that holds every right) on the site the request's host
  name resolves to, by an assignment with no scope restriction. Any other signed-in user, of that
  site or another, gets `403 permission.denied` with the standard error body: the same body for a
  user with some rights and a user with none, no counter text, and no hint about the permission.
  No credential, or one that is not valid on that site, gets `401`. A user who must change a
  password first (first sign-in) is refused by the password-change gate before anything else.
  The refusals are counted like any request (route `/metrics`, status 403).
- **How a Prometheus scrape authenticates.** Create a dedicated user for it, give it the
  `Tenant Administrator` role and nothing else, and give it an API key; Prometheus sends the key
  as `Authorization: Bearer <key>` (`authorization: credentials_file:` in its scrape config).
  Scrape each site's host name you want the numbers through; the numbers are the process's, so one
  scrape per process is enough.
- **Loopback mode.** `services.metrics.require_auth = false` serves `/metrics` with no
  credential, for a collector on the same machine. The host refuses to start when
  `require_auth = false` and any `server.bind` address is not loopback (the start-up error names
  both settings), because every bind address serves every route.

## Services

Each hosted service has its own page. A new service adds one line here and one page under
`running-spot/`; nothing else in this file changes.

- [Files](running-spot/files.md)
- [File uploads](running-spot/file-uploads.md)
- [Durable jobs and coordination](running-spot/jobs.md)
- [Realtime](running-spot/realtime.md)
- [Mail](running-spot/mail.md)
- [Print](running-spot/print.md)
- [Reports](running-spot/report.md)
- [Export](running-spot/export.md)

## Logs and traces

One JSON line per request from the telemetry layer: request id, method, route template, status
and duration. Tracing spans and the Prometheus counters come from the same layer.
`[telemetry]` in `common.toml` sets the filter, the format and an OTLP collector; without an
OTLP endpoint nothing is exported, and traces stay local.

## `common.toml` services

Every option below defaults to the single-node desktop profile: one process, nothing shared.

```toml
[services]
profile = "single-node"        # or "cluster"
shutdown_grace_ms = 5000       # per-service drain deadline

[services.metadata]
bus = "in-process"             # "polling": the database version store, seen by every node
poll_ms = 2000                 # how often a node reads the version store

[services.realtime]
enabled = true
heartbeat_seconds = 25
poll_ms = 100

[services.jobs]
queues = ["default"]
store = "postgres"             # "memory" (single-node only): jobs live in this process alone
intake_poll_ms = 250
grace_ms = 10000               # how long a stopping worker waits for its running job
lease_ttl_ms = 15000           # the queue-consumer lease; the holder renews at a third of it

[services.scheduler]
tick_ms = 1000
lease_ttl_ms = 30000

[services.mail]
enabled = true
sink = "memory"                # "smtp" sends; "memory" captures (tests, local development)
sent_log = "auto"              # "auto": the database log when jobs.store is "postgres" (a restart cannot
                               # send an accepted message again), memory otherwise; or "postgres"/"memory"

[services.print]
enabled = true
template_root = "templates"

[services.files]
provider = "local"             # "local" or "memory"
root = "files"
max_bytes = 26214400

[services.metrics]
enabled = true
require_auth = true            # false: a co-located collector may scrape without a credential; every server.bind address must then be loopback
```

Every option is validated **together**: one start-up reports every problem at once, including
the combinations (`profile = "cluster"` requires `metadata.bus = "polling"` and
`jobs.store = "postgres"`; `sink = "smtp"` requires an `[services.mail.smtp]` table;
`mail.sent_log = "postgres"` requires `jobs.store = "postgres"` (`auto`, the default, follows the job
store; the start-up log says which log is in use, for example `mail sent log: postgres (auto)`); the scheduler's
`lease_ttl_ms` must exceed `tick_ms`).

The database sent log is the table `spot_mail_sent_log`. It lives in the `spot_` namespace that
`spot migrate` leaves alone, so using it never blocks an online schema change. A site opts out with
`sent_log = "memory"`; the risk is a repeat send: a crash between the mail server's acceptance and
the log write loses the record, and the retry sends the same message again.

### A worked cluster

Two or more nodes on one PostgreSQL database. The database is the only shared piece: metadata
invalidations travel through its version store. Runtime metadata changes are also recorded in
`sites/<site>/metadata.json`, a file: until definitions live in the database, nodes on
different machines must share the site folder.

```toml
[services]
profile = "cluster"
shutdown_grace_ms = 15000

[services.metadata]
bus = "polling"
poll_ms = 1000                 # every node sees a metadata change within one poll

[services.jobs]
queues = ["default", "mail"]
intake_poll_ms = 250
grace_ms = 60000

[services.scheduler]
tick_ms = 1000
lease_ttl_ms = 30000

[services.mail]
enabled = true
sink = "smtp"

[services.mail.smtp]
host = "smtp.example.com"
port = 587
security = "starttls"
from = "erp@example.com"
username = "erp@example.com"
password = "…"                  # a secret: never in a log, a report or a readiness detail

[services.metrics]
enabled = true
require_auth = true
```

What the cluster profile changes: metadata invalidations go through the database, and so does
the coordinator. Every node runs the same services; the queue consumer and the scheduler leader
are each held by one node at a time through a fenced lease in the database (see "Durable jobs
and coordination"), so any number of nodes may run.

## Apps, controllers and app permissions

An installed app whose `app.toml` names `controller = "native:<name>"` has its native controllers
registered, one per DocType (`native:<app>` and the companions `native:<app>.<suffix>`), and
their pipeline steps and whitelisted methods are installed into the site's engine. Only
controllers compiled into the binary exist (`spot_cli::natives`); a reference to anything else
refuses the app. Hooks run under the caller's permissions: a step that loads a document the
caller may not read fails that read.

An app declares named permissions in `app.toml` (`[[permissions]]`), and grants them to roles in
`permissions/app_permissions.toml`:

```toml
[[entries]]
role = "Note Approver"
permission = "approve_note"
```

The site's catalogue is the union of its installed apps' declarations (a name declared twice
refuses the site), and the grants are part of every permission snapshot the site builds: the
first one, a reload and a customisation swap. A method that requires a permission nobody
declares is refused to everyone but the platform itself, and the site reports it:

```
GET /api/health/ready   ... "detail":"degraded: perm.app.unknown: method `bless` of `note` requires permission `no_such_permission`, which no installed app declares"
```

Editing `app_permissions.toml` takes effect at the next reload (`spot reload <site>`).

## Installing a signed app bundle

`spot install-app <file>` installs an app from a packed bundle written by `spot-pack pack --sign`. The host checks the signature, the publisher's certificate, the site's trust roots and its revocation list **before** it writes anything, and a bundle that fails any check changes nothing. Put the trust settings in `common.toml`:

```toml
[trust]
roots = "trust/roots.txt"                # one root public key per line (base64); empty or absent trusts nobody
revocations = "trust/revocations.json"   # the revocation list the root issued; required to install a signed bundle
require_signed = true                    # the default
```

Paths are relative to the configuration root. With no root pinned, **every signed bundle is refused** until an operator adds one; the host never ships a key.

The bundle sits in a folder with its `app.sig` (written by `spot-pack pack --sign`) and `app.cert` (the publisher's certificate file). Prepare the app's place first: an empty directory `apps/<app name>/doctypes/`, listed in `common.toml` `apps`. A verified bundle is unpacked into a temporary directory and renamed into that place; a place that already holds other content is never replaced. The command prints `install mode: verified bundle ... (publisher ..., digest ...)`.

What an operator sees on a refusal is one line, `cli.<code>: <words>`, and the same code is written to the site's `audit.jsonl` with `"outcome":"refused"`, the bundle digest and the publisher id the signature file names (never a key or a payload):

| Code | Meaning |
| --- | --- |
| `cli.bundle_unsigned` | no `app.sig`, and `require_signed` is true |
| `cli.bundle_signature_invalid` | the signature does not verify over this bundle (changed content, flipped signature, another app or version) |
| `cli.bundle_untrusted_publisher` | the certificate's root is not pinned, or the certificate does not match the signature |
| `cli.bundle_revoked` | the publisher, the key or this bundle is on the revocation list |
| `cli.bundle_certificate_expired` | the certificate is not valid now, or was not when the bundle was signed |
| `cli.bundle_no_trust` | no root pinned under `[trust] roots` |
| `cli.bundle_revocations_invalid` | no usable revocation list, or its signature does not verify |
| `cli.bundle_certificate_missing` | `app.cert` is missing |
| `cli.bundle_unsafe_path` | an entry is absolute, has `..`, or is a link |
| `cli.bundle_incompatible` | the bundle needs another host version |
| `cli.bundle_invalid` | the bundle does not open, or names an app that cannot be installed |
| `cli.bundle_not_listed`, `cli.bundle_app_exists`, `cli.bundle_unpack_failed` | the app's place is not listed, holds other content, or could not be written |
| `cli.directory_unsigned` | an app directory or catalogue name was given and `require_signed` is true |

**Directory installs are for development only.** `spot install-app sample` (a catalogue name or an app directory) is refused while `require_signed` is true, which it is unless `common.toml` says otherwise. A development site sets `require_signed = false`; the command then prints `install mode: development directory install`. An unsigned bundle is accepted only on such a site, and a signature that is present is verified there too.

A refused `reset-admin` (the `--yes-i-am-the-operator` flag missing, or the credential store refusing) is written to the same `audit.jsonl` with `"outcome":"refused"` and a `reason`. A reset for a site that does not exist has no site folder to write into, so it prints a warning instead.

## The participants every write passes through

Every document write goes through the engine's transaction participants. The site installs them in
one place (`services::participants::build`) and in one order, which is not the host's to choose:
**audit first, then the owner check, then edge sync last**. The audit sees the write the owner
check lets through; edge sync derives the graph edges from the document that survived every
refusal. They are rebuilt for every metadata generation: a DocType or field made while the site
runs is installed on the same engine at the swap, so edge sync derives from what the site serves
and never from the metadata it started with.

What a site installs today:

- **Edge sync**, always. Each write keeps the site's edge tables equal to its link columns, in the
  same transaction as the document.
- **The owner check**, only when the site has a `[partition]` profile. It reads ownership from
  `spot-partition-pg`, whose tables live in the site's own database, and refuses a write to a scope
  this node does not own with `partition.not_owner` before anything is stored. The registry is
  never queried from the request path: the host reads the assignment trail at start-up and re-reads
  it every 30 seconds on a thread of its own, and the check answers from that snapshot (a blocking
  database call inside a request is refused by design, ADR-082). A change of ownership therefore
  reaches a running node within half a minute, and `spot reload <site>` rebuilds the site around
  the same snapshot. A single-node site (no `[partition]` section) installs no owner check.
- **The audit participant** is not installed yet: the audit chain has no store the host can reach
  (its DDL is not published for the migrator and there is no durable appender store), so an event
  written now would never be chained. WP-PRD-01.T22 installs it and documents what is audited.
  Readiness says `docaudit: off`.

A partitioned profile is configuration, not a DocType property:

```toml
[partition]
node = "node-eu-1"
doctypes = ["invoice", "time_entry"]

[[partition.scope]]
doctype = "invoice"
key = "branch"
field = "branch"
```

A DocType listed under `doctypes` with no `[[partition.scope]]` entry is unprotected, not allowed:
the site refuses to start with `partition.scope_uncovered` and names every such DocType.

Who owns a scope is data, not configuration: `spot_partition_assignment` holds one effective-dated
assignment per scope key, written by the control plane, and `spot_partition_history` is its
append-only trail. `spot-cli` reads them; it never assigns. Switching a node profile on, in order:

```
# 1. add the [partition] section above to sites/<site>/site.toml
# 2. create the ownership tables (also creates the participants' other tables)
spot migrate <site> --apply
# 3. assign the scopes this node owns (the control plane; an unassigned scope is refused)
# 4. rebuild the serving site so the new profile is read
spot reload <site>
```

Step 2 is not optional: a node profile whose database lacks the ownership tables refuses to serve
with "the database is missing the partition ownership tables ...; run `spot migrate --apply`",
rather than running an owner check it can never answer.

### The start-up refusals

Every one of them stops the site. A skipped participant is a silent integrity defect, so none of
them warns and serves anyway.

| Code | When | What the operator does |
| --- | --- | --- |
| `partition.scope_uncovered` | `partition.doctypes` names a DocType with no `[[partition.scope]]` entry | add the entry (DocType, key type, field) or drop the DocType from `doctypes`; the site serves nothing until then |
| `cli.partition_registry_missing` | the site names a partition node but the host could not build the ownership registry (a pool too small to serve the bridge, or the store unavailable) | check `[postgres] pool_max_size` (at least 3) and run `spot migrate <site> --apply` |
| `docaudit.double_registration` | an installed app registers an `AuditStep` for a DocType the audit participant already audits | remove the app's audit step: the participant records the write. Armed for WP-PRD-01.T22; with no audit participant installed today it cannot fire |
| "the database is missing the partition ownership tables" | a partitioned site whose `spot migrate --apply` has not run | `spot migrate <site> --apply` |

### What `ready` says about the participants

`GET /api/health/ready` reports one line per site, `participants:<site>`:

- `partition: off` for a single-node site;
- `partition: node <id>, <n> DocTypes` for a partitioned one — the node it runs as and how many
  DocTypes it checks. Never an assignment, a scope key or a value;
- `docaudit: on` or `docaudit: off`;
- `installed: audit, owner, edge` — the participants, in the order they run.

### Where the participants do not run

Two paths never pass through them. Neither is in the tree yet, so neither runs today:

- **replica apply** (an incoming record from a peer, WP-EDG-06) is not this site's write, so it
  carries the receive-side check instead: `OwnerCheck::allow_replica` over the same ownership map
  accepts a record with scope keys only from the node that owns them.
- **import** is the site's own write path and must go through the engine like any other; an import
  that wrote documents directly would leave the edge tables and the ownership check behind.

### The participants' tables

The participants write to tables that are not documents: `spot_graph_edge` and the edge drift
tables, the mail sent log, and the ownership registry's four tables
(`spot_partition_assignment`, `spot_partition_history`, `spot_range_series`, `spot_range_grant`).
They are created by `spot migrate`, never at start-up. The first three groups are steps of the
schema plan; the ownership tables are created by their own crate under its own advisory lock,
which is why `spot migrate --apply` runs whether or not the plan has anything else to do:

```
spot migrate <site> --apply
```

`spot new-site` runs the same plan, so a new site is ready to serve at once — but it is created
without a `[partition]` section, so a site that is *switched on* later needs the `spot migrate`
of step 2 above. A site whose database is behind refuses to start with "the database is missing
the platform tables ...; run `spot migrate --apply`" instead of writing a document whose edges
cannot be stored. A second `spot migrate --apply` plans nothing.

## Shutdown

1. Readiness flips to `503` and the listener stops accepting.
2. In-flight requests finish within `server.shutdown_grace`.
3. Each site's services stop in the reverse of their start order, each within
   `services.shutdown_grace_ms`: the job worker finishes or releases its job, the scheduler
   stops planning, the metadata follower stops polling, and the coordinators release their
   leases.
4. A service that overruns its deadline is reported and left to the runtime; nothing is left
   running that the process does not own.

No accepted request is lost: the listener drains before any service stops, and a job that was
running either completes (its claim is committed) or is released for another node.

## Troubleshooting

| Symptom | Look at |
| --- | --- |
| A site answers `503 server.unavailable` | `serve`'s stderr: the site's required service and its check's reason are printed once at start. |
| A site fails to start | `serve`'s stderr names the failing module or service: the registry stops at the first failing module, and the start-up check stops at the first failing required service, both in the start order above. |
| `/api/health/ready` shows `degraded:` | The named optional service's own reason (in `common.toml` or its check). |
| A custom field is not in the API | `sites/<site>/metadata.json`, then `spot reload <site>`; the schema plan is additive and refuses a destructive change. |
| Jobs do not run | `services.jobs.queues` must include the job's queue; the worker claims only its configured queues. On a second node, only the lease holder consumes (`consuming the job queue` is logged once, by the holder). |
| A method is refused with `permission.denied` for a role that should hold it | The role's row in `permissions/app_permissions.toml`, then `spot reload <site>`; `/api/health/ready` names a permission nobody declares. |
| A realtime join is refused | The same read right as the list or the document; the answer is the same whether the room exists or not. |
| A second node does not see metadata | `profile = "cluster"` with `metadata.bus = "polling"`, and both nodes on the same database. |
