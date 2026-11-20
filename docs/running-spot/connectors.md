Part of [Running Spot](../running-spot.md).

# Connectors (the `connectors` service)

The `connectors` service hosts the connector SDK (`spot-connectors`): a site configures a connector,
schedules or runs it, and watches it. It is **optional**; a site with no connector has it idle, and a
connector that failed to load or is in the dead-letter state degrades readiness (never stops the
site). It needs the `secrets` service (the vault is sealed with the site keyring) and the jobs
service; without the keyring it does not register.

## Writing a connector

Build it from the SDK template (`sdk/connector-template/` at the repository root): a Rust
guest that exports `describe`, `invoke` and `webhook` and imports only `log`, `http-request`,
`secrets-get`, `kv-get` and `kv-set`. It has no network, file system or clock of its own. Pack and
sign it like an app (`spot-pack pack --sign`) and put the bundle in the bundle store
(`services.connectors.bundles_dir`, the same `<digest>/app.bundle|app.sig|app.cert` layout as
`services.apps`; see apps.md).

## Configuring it

A `connector` row holds the component **digest**, the **hosts** it may call (`host` or `host:port`,
one per line; **empty means no network at all**), the credential kind and **reference name**, other
readable references, the schedule (`every N seconds`, at least 10) and the mapping. A credential
**value** is never a field. It is written once, through the write-only secret action, into the vault:

- sealed at rest with the site key, stored in PostgreSQL by `(site, reference)`;
- bound to its site and reference (a copy moved to another reference or site does not open);
- opened only inside the host's `secrets-get` import for a reference the connector declared, or when
  the host injects the `bearer`, `basic` or `oauth2` credential into an outbound request;
- never in a DocType read, a log, the run log, a metric label or an error. An operator who can edit
  a connector cannot read a stored secret back.

## Rotating a secret

Write the new value to the same reference: the next run uses it (nothing is cached). To change the
site key, rotate the keyring and call the re-seal action: every stored value is opened under its old
key and sealed under the current one.

## Runs

A run is the job `platform:connector_run` (`{"connector","operation","input"}`), run as the service
principal `connector:<name>`, never as the administrator who configured it.

- **Scheduled** connectors enqueue one job per slot; the slot number is in the idempotency key, so
  two nodes ticking the same slot create one job.
- **Manual** runs come from the `connector_invoke` route (WP-INT-05.T4).
- **Limits** per connector (fuel, wall clock, memory, host calls, output) from
  `[services.connectors.limits]`, with per-connector overrides under `limits.apps.<name>`, capped by
  the site maxima. One run of a connector at a time by default (`max_concurrent_runs`).
- **Failures.** A retryable upstream failure is retried with the job worker's backoff. On the last
  attempt the connector goes to the **dead-letter** state, an alert is raised
  (`spot::alert::connector`), and it runs no more until an operator re-enables it. A permanent
  failure fails the job at once.

## Egress

Every outbound call passes `spot-egress` with the connector's own allow-list. Loopback, private,
link-local and metadata addresses are refused after resolution; redirects are checked again; response
size and time are capped; credentials are injected by the host and never visible to logs.

## Monitoring

`spot_connector_runs_total{outcome}` (`ok`, `retry`, `failed`, `dead_letter`), the host's per-connector
duration histograms, the age of the last success in the status listing, and the `connector_run`
log (identifiers, outcome, duration and a stable code only).

```toml
[services.connectors]
bundles_dir = "bundles"
schedule_tick_seconds = 15
max_concurrent_runs = 1

[services.connectors.limits]
fuel = 100000000
wall_clock_ms = 1000
```
