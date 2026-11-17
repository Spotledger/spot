# Usage metering emitter (`[services.metering]`)

Counts active users, transactions, storage and AI tokens on the tenant host and ships them to the control plane
(`POST /cp/v1/host/usage`) when one is configured. **Off by default**: a desktop tenant without a control plane
makes no network call and runs no task.

```toml
[services.metering]
enabled          = true
tenant           = "acme"                         # part of every event id
control_plane    = "https://cp.example:8444"      # the host listener
ca_cert          = "/etc/spot/cp-ca.pem"          # optional: the CA that signed the server certificate
host_cert        = "/etc/spot/host.pem"           # issued by the control plane's host CA
host_key         = "/etc/spot/host.key"           # mode 0600
interval_seconds = 3600                           # sampling bucket and shipping period (at least 60)
outbox_max       = 10000                          # local events before summarising (at least 100)
batch            = 500                            # events per request (at most 1000)
```

Unknown keys are refused. Enabled, the service needs `MeteringPorts` (the counters, the per-site salt, the
outbox and the mutual-TLS shipper), provided by the module that owns them; with no provider the site refuses to
start and names the port. It is an optional service.

## Guarantees

- **Never in the request path, never fails a write.** A background task samples the counters (cheap reads it runs
  itself) into a local outbox table, then ships. Posting a document with the control plane down, or with this task
  failing, is unaffected.
- **Idempotent ids.** `tenant:meter:bucket` for amounts and `tenant:active_users:digest:day` for users; a restart or a
  second sample of the same bucket cannot count twice. A counter's cursor moves in the same transaction as its event,
  and only when the event was new. The first sample only sets a baseline: history before enabling is not billed.
- **Offline for days is fine.** No answer, 429 or 5xx leaves the batch in the outbox; a 400, 413 or 422 drops that
  batch (retrying cannot fix it) and counts it. 401, 403 and 404 are about the host, not the batch: they wait.
- **Bounded.** Over `outbox_max` the oldest amount events of one meter are **summarised** (counters add, the storage
  gauge keeps its latest value); nothing is dropped. If it cannot shrink (only user events remain) readiness turns
  degraded; users see no error.
- **No personal data.** Active users leave as `sha256(site salt, 0x00, id)` digests; counts and digests only. The salt
  and digests are never logged. The transport is mutual TLS, outbound only; the key file must be mode 0600 (checked at
  the start-up check).

## Tables (tenant database)

`<prefix>_outbox (event_id pk, meter, amount, member, at, shipped)` and `<prefix>_cursor (meter pk, last)`. Shipped
rows are kept two days so a resample of the same bucket is a no-op, then purged.

## Not done here

The adapters behind the ports (the counters from the host's telemetry, the salt, the mTLS transport built from
`host_cert` and `host_key`) belong to the modules that own those.
