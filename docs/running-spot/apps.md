Part of [Running Spot](../running-spot.md).

# Installed apps at run time (the `apps` service)

The `apps` service hosts the WebAssembly components of the apps installed on a site. It is
**optional**: a site with no installed app has it idle, and a failed or disabled app degrades the site
(readiness names it with its code) but never stops it.

## What happens at start-up

1. The service reads the site's installed apps from the PostgreSQL installer registry.
2. For each app it reads the bundle by digest from the bundle store and **verifies it again**: the
   bytes must open as a bundle with the digest the app was installed with, the signature and publisher
   certificate must verify against `[trust] roots`, and neither the publisher, the key nor the bundle
   may be on `[trust] revocations`. A certificate that expired after the signing is accepted at
   start-up, as designed. With `[trust] require_signed = false` (development) an unsigned bundle loads.
3. A bundle that fails is **not loaded**: it is listed as `failed` with `app.bundle_invalid` or
   `app.publisher_revoked`, and the rest of the site starts normally.
4. The component is compiled once (cached by digest, `cache_entries`) and linked with exactly the
   capabilities the administrator approved at install. An import that was not approved fails the link
   (`app.capability_denied`).

An install, upgrade or uninstall signals `AppsPort::reload` (everything is re-read) or
`AppsPort::unload` (one app leaves at once).

## The bundle store

Content addressed, under `services.apps.bundles_dir` (default `bundles`, relative to the
configuration root):

```
bundles/<64 hex digest>/app.bundle    the bundle
bundles/<64 hex digest>/app.sig       the detached signature (spot-pack pack --sign)
bundles/<64 hex digest>/app.cert      the publisher certificate
```

## Limits

Every call runs under fuel, wall-clock, memory, host-call and output limits. The defaults and the
per-app overrides are capped by the site maxima:

```toml
[services.apps]
breaker_kills = 5            # kills in the window that disable an app (0 = off)
breaker_window_seconds = 300
egress_allow = ["api.example.com:443"]

[services.apps.limits]
fuel = 100000000
wall_clock_ms = 1000
memory_mib = 16
host_calls = 1000
output_kib = 1024
max_fuel = 1000000000
max_wall_clock_ms = 10000
max_memory_mib = 64

[services.apps.limits.apps.erp_selling]
wall_clock_ms = 2000
```

An overrun kills only that call's instance and answers `app.limit_exceeded`. It writes an audit
record to the `spot::audit::apps` log target (the app and the limit, never a payload) and counts
`spot_app_kills_total{app,limit}`. An app killed `breaker_kills` times inside the window is **disabled**
(`app.disabled`, shown in `GET /api/v1/apps` and in readiness) until the next reload.

## Network

An app reaches only hosts that are both approved for it at install and listed in `egress_allow`. An
empty list means no app has any network. Names are resolved once and every address is judged:
loopback, private, link-local and cloud-metadata addresses are refused after resolution, and a
redirect is a new request that is checked again. No request or response body is logged.

## Who a hook runs as

A document read by an app runs as the **invoking user**, through the same permission engine and field
guard as any caller; a system or consumer actor reads nothing. There is no service identity reachable
from guest code. Hooks that run inside a transaction are refused any external call (ADR-51).

## Monitoring

- `GET /api/v1/apps`: each app, its version, digest, publisher, status and recent kills.
- Readiness: `apps` lists every failed or disabled app with its code.
- Metric: `spot_app_kills_total{app,limit}`.
