# The `cluster-redis` profile: running Spotledger against Redis or Valkey

This is the operator guide for the optional Redis/Valkey plugin. The default product does not
need it: a single node coordinates in process, and a cluster coordinates through PostgreSQL.
Read the threshold at which the profile pays for itself in
`crates/backends/spot-cluster-redis/docs/redis-profile.md` before turning it on.

## What it changes

With `[services.cluster_redis]` enabled, each site gets three ports from the server:

| Port | What it carries |
| --- | --- |
| `Coordinator` | leases with fencing tokens, counters, rate limits |
| `CacheStore` | the shared, lossy cache (always per tenant) |
| `InvalidationBus` | metadata and cache invalidations between nodes |

The module runs at order 1000 (the registry's minimum for a module file), after the built-in
modules; the host wiring that makes jobs, scheduler, realtime and metadata use the ports is a
separate step. With the table absent or
`enabled = false` nothing is registered and the site behaves exactly as before.

## Settings

```toml
# common.toml
[services.cluster_redis]
enabled = true
url = "rediss://redis.internal:6380"      # TLS, certificate verified
username = "spot"
password_ref = "file:/etc/spot/redis.password"
prefix = "spot:prod"
```

| Key | Meaning |
| --- | --- |
| `enabled` | `false` by default. |
| `url` | Required. `rediss://host:port`. A `redis://` URL is accepted only with `allow_plaintext = true` and a loopback host. The URL may not carry a user, a password, a query or a fragment (`#insecure` is not available). |
| `allow_plaintext` | Allows `redis://` to `localhost`, `127.0.0.0/8` or `::1`. Never a remote host. |
| `username` | The ACL user. Absent means the server's default user. |
| `password_ref` | Where the password lives: `env:NAME` (an upper-case variable) or `file:/absolute/path`. The password is never a setting. A file must be readable by its owner only (mode 0600) on Unix. |
| `prefix` | Key prefix, 1 to 64 of letters, digits, `:`, `_`, `-`. Two clusters on one server use two prefixes. Every key below it also carries the site id. |
| `pool` | Only `1` is accepted: the plugin multiplexes one reconnecting connection per site. |
| `require_restricted_acl` | Refuse to start when the user may run commands the plugin does not need. Default `false`. |
| `max_clock_skew_ms` | The most this node's clock may differ from the server's. Default 2000. |
| `probe_interval_ms` | How often the supervisor probes the server. Default 2000. |

## The ACL the plugin needs

The plugin needs these commands on keys and channels under its prefix: `GET`, `SET`, `DEL`,
`EXPIRE`/`PEXPIRE`, `EXISTS`, `TTL`/`PTTL`, `INCRBY`, the hash, list and set commands the cache
uses, `SCAN`, `PUBLISH`, `SUBSCRIBE`, `EVAL`/`EVALSHA`/`SCRIPT`, and `PING`. The start-up check
also uses `INFO`, `TIME`, `ACL WHOAMI` and `ACL DRYRUN`. A sketch:

```
user spot on >... ~spot:* &spot:* +@read +@write +@pubsub +@scripting +ping +info +time +acl|whoami +acl|dryrun -@dangerous
```

## The start-up check

Before the site starts, for each site, the host runs:

1. `PING`, and the version (older than 6 is refused);
2. the clock skew against the server's `TIME` (beyond `max_clock_skew_ms` is refused, because a
   lease is decided from clocks);
3. an ACL probe with `ACL DRYRUN`, which executes nothing: the plugin's own commands must be
   allowed (else refused), and the dangerous ones (`FLUSHALL`, `CONFIG`, `KEYS`, `SHUTDOWN`,
   `DEBUG`, `ACL SETUSER`, `REPLICAOF`, and a key outside the prefix) are reported when also
   allowed. With `require_restricted_acl = true` a broad user is refused. When the server
   cannot answer the probe (Redis older than 7, or `ACL DRYRUN` not allowed) the verdict is
   "unverified" and is logged.

A check that fails stops the site's start with a message that names the reason and never a
credential. The site does not fall back to another coordinator: a site that asked for Redis
does not start without it.

## What happens when the server is lost

| Part | Behaviour |
| --- | --- |
| Coordinator | Every call answers `coordinator.unavailable`. A lease cannot be renewed, so the jobs worker and the scheduler stop claiming within their lease TTL (fail closed). When the server returns, an old holder is refused (its fencing token is stale) and another node that took the lease while it was away keeps it. |
| Cache | A read is a miss and a write is dropped. When the supervisor sees the server again it clears the site's cache namespace, which bumps the namespace generation, so nothing cached before the outage is read afterwards. |
| Bus | A publish the server refuses goes to this node's own subscribers instead, and the status says so. The server's subscription reports a `Gap` after it reconnects, so a subscriber reconciles from the version store. Peers do not hear a change that was published while the server was away until they reconcile. |

The service is `optional`, so the site keeps serving reads. The jobs and scheduler do not run
without the coordinator, which is the intended failure.

## Metrics

On `/metrics` (when the metrics service is on):

| Name | Kind | Meaning |
| --- | --- | --- |
| `spot_cluster_redis_connected` | gauge | 1 while the server answers, else 0 |
| `spot_cluster_redis_lease_renew_seconds` | histogram | time a lease renewal took |
| `spot_cluster_redis_invalidation_publish_seconds` | histogram | time the server took to take an invalidation |
| `spot_cluster_redis_bus_fallback_total` | counter | invalidations that fell back to the per-node bus |
| `spot_cluster_redis_cache_resets_total` | counter | cache namespaces reset after an outage |

No label carries a site, a key, a lease name or a credential.

## Running the tests against a server

The integration tests in `crates/framework/spot-cli/tests/cluster_redis.rs` skip, with the reason
printed, unless `SPOT_TEST_REDIS_URL` is set:

```bash
docker run -d --rm -p 127.0.0.1:6391:6379 redis:7-alpine
SPOT_TEST_REDIS_URL=redis://127.0.0.1:6391 cargo test -p spot-cli --test cluster_redis
```

The ACL tests also need a server with a restricted user, and skip without
`SPOT_TEST_REDIS_ACL_URL` (a `redis://` URL to a server that has a user `spot` with the password
`testpw` and the commands above, and the default user switched off).
