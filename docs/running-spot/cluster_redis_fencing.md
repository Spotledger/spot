Part of [Running Spot](../running-spot.md).

# Cluster Redis: fencing and short outages

This page records what the two-host test (`tests/cluster_redis_fencing.rs`, WP-PLT-19.T7) proves about `[services.cluster_redis]`, and one behaviour an operator must know.

## Fencing when a host is cut off

Two hosts of one cluster share one lease store. Host A holds the lease for a job and runs it.

1. A is cut off from Redis. Its renewal fails with `coordinator.unavailable`, so A runs nothing (it does not guess that it still holds the lease), and `/api/health/ready` reports the site degraded with "the coordination server is unreachable" at once.
2. The lease runs out; host B acquires it under a **larger fencing token** and runs the job. Two hosts never run the job under one token.
3. A heals. The server refuses A's old token and A's old lease handle (`coordinator.lease_lost`), A drops its lease without acting, and it cannot take the lease while B holds it.
4. When B lets go, A rejoins under a still larger token.

The test runs this twice: once without a server (the in-process coordinator on a manual clock behind the plugin's own observing decorator, with a switch that cuts a host off) and once on a real Redis or Valkey server (`SPOT_TEST_REDIS_URL`, for example `redis://127.0.0.1:6391`, loopback only), with a TCP proxy per host that the test cuts and restores. Without the variable the real-server tests print `SKIPPED` and pass. Lease times are under a second and no sleep is over a second.

## An outage shorter than one probe is not seen

The plugin knows the server is down from two places: a lease call that fails (at once), and the supervisor's probe (every `probe_interval_ms`, five seconds by default). The cache is neither: the Redis cache swallows a connection failure as a miss, and a lost `delete` returns `false`, with no error code. So an outage in which nothing renews a lease and no probe runs is not seen:

- an invalidation sent during the cut is lost, and the entry it should have removed is served after the cut;
- the reset-after-recovery gate (`cache_resets`) does not run, because the status never showed a disconnect.

In a running host the lease renewals and the 5 s probe make such a window short, but it exists. The test pins the behaviour as it is today (with `probe_interval_ms` set to an hour so only a manual probe could help); if the plugin changes to see these outages, the test fails and the guide here must change with it.

What to do about it today: keep `probe_interval_ms` low (the default is a good value), and treat any `spot_cluster_redis_cache_resets_total` increase as an outage the plugin did see. A cache entry that must never be stale should carry a short TTL.
