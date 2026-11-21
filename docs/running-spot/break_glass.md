Part of [Running Spot](../running-spot.md).

# Break-glass, the customer's side

`spot-break-glass` models one session; this service (`services/break_glass`, order 1170) is what the tenant server holds: the registry of sessions, the sealed recording archive, the customer's list, recording read and revoke, and the kill at expiry. The customer's audit chain is the truth; nothing the operator side holds can change what the customer sees.

## Setting

```toml
[services.break_glass]
enabled = true            # default
sweep_secs = 15           # 1 to 3600
retention_days = 365      # fallback when the data policy names none (1 to 3650)
max_recording_bytes = 67108864
page_max = 100
```

Unknown keys are refused at start (`break_glass_host.settings_invalid`).

## What the host must provide

The service reads these ports from the modules that start before it: the audit chain (`EventLog`), the append-only file store (`ObjectStore`, plus `Retire` for retention), the secrets sealer (`Sealer`), the duty check (`Duties`), the agent bridge (`AgentBridge`) and optionally the data policy's retention (`Retention`). The first four are required: when one is missing the check fails, readiness reports the site degraded naming it, every read answers `break_glass_host.not_wired` and no session can be authorised. The rest of the site is unaffected (the service is optional).

## What the customer can do

- List sessions (active and past) and read a summary: security administrator or security auditor duty.
- Read a recording: security administrator duty only. The recording is stored sealed, is checked against the digest on the chain on every read, and the read is written to the chain before any content is returned.
- Revoke an active session: security administrator duty. The session is closed first, so `authorise` refuses it at once; the kill goes to the agent bridge and, if it cannot be delivered, is retried by the sweep. The answer says whether the kill was delivered and whether the event is on the chain.

A failing, missing or negative duty check is a refusal. A session of another tenant answers exactly like a missing one.

## Expiry and retention

The sweep ends every session past its window, kills it and writes `expired`, even when the operator side never reports an end. Recordings of finished sessions are removed only by the sweep, only after the retention; there is no delete operation, and a vendor-side delete is refused (`break_glass.vendor_delete_refused`).
