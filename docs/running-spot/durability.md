# The durability gate (`[services.durability]`)

Zero loss is a statement about failure domains: a write is not acknowledged until its event exists in a second failure domain (ADR-061 section 4). The gate holds the response of every **mutating** request (`POST`, `PUT`, `PATCH`, `DELETE`) of a site whose journal is on until the journal has made the write durable. Reads and health routes are never gated: a halted tenant can still be read and exported.

The policy is the journal's (`[services.journal]` `policy` and `on_unreachable`, see [the journal](journal.md)); this page is what the gate does with it.

```toml
[services.durability]
wait_seconds = 5          # how long a committed write waits for the journal (default 5)
retry_after_seconds = 5   # the Retry-After of a refusal (default 5)
```

## What the gate waits for

After the handler returned success, the gate reads the outbox head **after** that commit (the storage contract's `commit()` returns no position and is frozen). If the head is above the journal's durable position it waits for it, up to `wait_seconds`, then answers. The head may also cover entries of concurrent transactions: the gate may wait for slightly more than it needs, never less.

| Policy | The acknowledgement |
| --- | --- |
| no journal, or `local` | at once (the local commit is the durability; the gate counts it) |
| `remote-sync` | after the journal holds the head (a tail chunk is in the target) |
| `replica-sync` | refused at start: `durability_gate.no_standby` (this build has no standby confirmation; the policy must not silently behave as `local`) |

## When the store is unreachable

- **`degrade`**: the write is acknowledged on the local commit (immediately when the shipper already knows the store is down, after `wait_seconds` otherwise). The backlog stays in the outbox (the barrier) and `RPO_BREACH` is raised by the journal service once the outage outlasts its grace.
- **`halt`**: new mutating requests are refused **before the handler runs** with `503 server.unavailable`, `Retry-After`, and `details.code = "journal.halted"`; the gate resumes by itself when the store returns. A write that already committed but was not confirmed in time answers `503` with `details.code = "journal.commit_outcome_unknown"`, retryable. Repeat it with the same `Idempotency-Key`: the idempotency layer sits inside the gate and replays the original result, so the document exists once. The error body never contains a position, a path or an object key.

The gate cannot be bypassed by a method-override header or a path trick: it is installed after routing and looks at the HTTP method of the routed request.

## Not gated

Non-HTTP writes (jobs, sync apply, scheduled work) are journalled and shipped by the journal service and covered by the RPO alarm. A job whose effect must be journalled before an external call is the job engine's committed-step concern, not the gate's.

## Policy at runtime

`DurabilityHandle::change_policy` changes the policy; it takes effect on the next request and writes `sys_journal_admin` (`policy_changed`, target `from->to`). A **downgrade** (for example `remote-sync` to `local`) needs the tenant administrator and is always audited (`durability_gate.permission_denied` otherwise); an **upgrade** needs the journal shipper healthy (`durability_gate.shipper_unhealthy` otherwise).

## What you see

`durability: remote-sync (degrade), acks p99 41ms`, or `durability: off (no journal)`. One tenant's halted store never blocks another tenant on the same host: the gate, the policy and the journal are per site.
