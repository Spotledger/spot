Part of [Running Spot](../running-spot.md).

# The fleet heartbeat (`[services.heartbeat]`)

A tenant host can tell the control plane that it is alive and how it is doing, so the fleet view
shows seeded failures within a minute. It is **off by default**: a desktop tenant with no control
plane makes no network call and starts nothing.

```toml
[services.heartbeat]
enabled          = true
control_plane    = "https://cp.example.net:8443"   # https://host[:port], no path
host_cert        = "/etc/spot/host.crt"            # issued by the control plane's host CA
host_key         = "/etc/spot/host.key"            # mode 0600
ca_file          = "/etc/spot/control-plane-ca.crt"
node_id          = "node-1"                        # a label for log lines; not sent
interval_seconds = 15                              # at least 5
```

Optional keys: `sample_budget_ms` (2000, at least 50), `outbox_max_batches` (5760, at least 4),
`outbox_dir` (default `heartbeat-outbox` in the data directory).

## What is sent

One batch every interval: `{"observations": [{"scope", "signal", "value", "observed_at"}]}`. The
only signals are these eight, and every value is an integer.

| signal | scope | value |
|---|---|---|
| `health_probe` | node | 1 when every required service is healthy, else 0 |
| `error_rate` | tenant | failed requests in the last window, basis points (10000 = 100%) |
| `queue_lag` | tenant | age in seconds of the oldest waiting job |
| `job_failures` | tenant | jobs that failed in the window |
| `disk_usage` | node | data filesystem in use, basis points |
| `certificate_expiry` | node | seconds until the soonest of the host's mutual-TLS certificate and the site's server certificate expires; negative once expired |
| `licence_state` | tenant | 0 active, 1 grace, 2 expired; **left out** when the site has no licence |
| `replication_lag` | node | seconds behind the primary; **only on a replica role** |

The host and the tenant are named by the client certificate, not by the body: a batch cannot speak
for another tenant.

## What is never sent

No user name, e-mail address, document name, record id, file path, host name, request path or any
other free text; no counts of business records; no credential. A signal that cannot be measured is
left out, never invented. The names of failing samplers are never sent.

## Enrolling the certificate

1. Ask the control plane's operator for a host certificate for this tenant (the control plane's host
   CA signs it; the console shows the enrolment steps).
2. Save the certificate, its key and the control plane's CA as the three files above. The key must
   be mode 0600 or stricter: the site refuses to start otherwise and names the file.
3. Add the control plane's `host:port` to the site's egress allow-list. If it is missing the site
   refuses to start and the message names the entry to add.
4. Set `enabled = true` and restart.

## Reading "silent"

- **Host down or cut off:** no batch arrives; the control plane shows the host amber (unknown) after
  its freshness window and never green.
- **Host up, something broken:** the batch arrives with `health_probe = 0`, even if every sampler
  failed. A talking host with a red probe is a broken host, not a missing one.
- **Control plane unreachable:** batches queue in the outbox (files in the data directory, not the
  database) and ship oldest first when it returns. A full outbox drops the oldest batches and keeps a
  local gap marker; the service reports `degraded` and `spot_heartbeat_dropped_total` counts them.
  Offline for days is normal for a desktop tenant.

Metrics: `spot_heartbeat_shipped_total`, `spot_heartbeat_failed_total{reason}` (`unreachable`,
`refused`, `outbox`, `sampler`), `spot_heartbeat_dropped_total`.
