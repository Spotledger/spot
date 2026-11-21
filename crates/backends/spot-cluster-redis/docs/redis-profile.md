# The `cluster-redis` profile

`spot-cluster-redis` (WP-PLT-19) is the **optional** Redis/Valkey plugin. It implements the same
`CacheStore`, `Coordinator`, `InvalidationBus`, `VersionStore` and `NodePresence` interfaces as
the built-in backends, so it is a drop-in for a deployment that already runs Redis or Valkey. The
default product does not need it: the database-backed coordinator (WP-PLT-18.T2) coordinates a
cluster, `InProcessCache` caches one process and the in-process bus fans metadata changes out
within it.

> This profile lives here, beside the crate it describes.

## When to run it

Run the `cluster-redis` profile when **all** of these hold:

1. More than one application node must share coordination state (leases, counters, rate limits),
   so the single-node in-process coordinator is not enough.
2. A Redis/Valkey server is already operated for the deployment (the plugin adds no new service to
   the estate), and the operator accepts its availability as part of the coordination path.
3. A benchmark shows the need. On the reference host below the database-backed coordinator and the
   Redis plugin are both network round trips; the plugin is only worth it when offloading
   coordination from PostgreSQL measurably reduces database contention. Measure both before
   switching.

The cache half is the easier win: the cache is lossy by contract, so a shared Redis cache survives
a node restart and warms a second node, which the per-process cache cannot.

Do **not** run it when one node serves the site, or when the database is not the coordination
bottleneck: the in-process backends are one to two orders of magnitude faster (see below).

## Running a server

Valkey and Redis are both supported; every command the plugin uses exists in both. A compose
snippet for local development or a single-node deployment:

```yaml
services:
  valkey:
    image: valkey/valkey:8-alpine      # or redis:7-alpine
    command: ["valkey-server", "--save", "", "--appendonly", "no"]
    ports: ["6379:6379"]
    healthcheck:
      test: ["CMD", "valkey-cli", "ping"]
      interval: 5s
      timeout: 3s
      retries: 5
```

Point a node at it with `SPOT_REDIS_URL=redis://host:6379` (a `rediss://` URL uses TLS; the plugin
enables the `tokio-rustls-comp` feature). The profile name is `cluster-redis`; the plugin's cargo
feature is `redis` and is **off by default**, so nothing compiles the `redis` crate unless a
profile turns it on.

### CI service container (the lead's step)

The integration suites skip without a server. To run them in CI, add a service container to the
`rust` and `acceptance` jobs and export the URL:

```yaml
services:
  redis:
    image: redis:7-alpine
    ports: ["6379:6379"]
    options: >-
      --health-cmd "redis-cli ping" --health-interval 5s
      --health-timeout 3s --health-retries 5
env:
  SPOT_TEST_REDIS_URL: redis://127.0.0.1:6379
```

Then the suites run with the feature on:

```bash
SPOT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test -p spot-cluster-redis --features redis
```

Without `--features redis` the crate builds only its pure key/script units and the integration
suites compile to nothing, which is what the default workspace build and the task acceptance
commands run.

## Benchmark

`docs/results.json` in this crate is the output of

```bash
SPOT_TEST_REDIS_URL=redis://127.0.0.1:6380 \
  cargo run --release -p spot-cluster-redis --bin bench --features redis -- --ops 5000
```

On the reference host (Linux, release build, Redis 7.4 in a container on the loopback interface),
5000 iterations each:

| backend | operation | ops/s | p99 | mean |
| --- | --- | ---: | ---: | ---: |
| in_process | cache_set | 86,992 | 20 µs | 11 µs |
| in_process | cache_get | 147,340 | 4 µs | 6 µs |
| in_process | lock_acquire_release | 97,528 | 18 µs | 10 µs |
| in_process | rate_take | 211,371 | 5 µs | 4 µs |
| redis | cache_set | 3,971 | 1,531 µs | 251 µs |
| redis | cache_get | 13,249 | 181 µs | 75 µs |
| redis | lock_acquire_release | 4,352 | 513 µs | 229 µs |
| redis | rate_take | 10,102 | 162 µs | 98 µs |

Reading the numbers:

- The Redis leg is **network bound**: a raw `PING` to the same container measured ~83 µs on this
  host, so the plugin adds a small, constant overhead over the round trip. The comparison that
  matters in production is Redis against PostgreSQL, not against a local in-process map; the
  PostgreSQL coordinator needs a live database, which is the lead's CI step, so this host reports
  the in-process reference instead.
- `cache_set` is slower than `cache_get` because a write also reconciles the site's size index (a
  second Lua round trip). A deployment that does not need the byte gauge can leave it out.
- The threshold to switch is therefore not "Redis is faster" but "coordination must be shared and
  the database's coordination load is worth moving off it". When the cluster's coordination calls
  exceed roughly **5,000/s sustained** and PostgreSQL lock or rate-limit waits appear in request
  latency, offloading them to a Redis/Valkey the estate already runs pays for the extra round
  trip. Below that, keep the database coordinator and the in-process cache.
