Part of [Running Spot](../running-spot.md).

# Listeners and rate limits

`spot serve` binds one listener per address in `[server]` of `common.toml`, and each listener
serves **one audience** (ADR-083, Barrier 2). A route is served only on the listeners of the
audiences its table entry names; a path the listener's router does not mount is the unserved
envelope, never the staff behaviour.

## The `listeners` table

```toml
[server]
bind = ["127.0.0.1:8000"]            # staff listeners, exactly as before

listeners = [
  { bind = "127.0.0.1:8001", audience = "staff" },
  { bind = "10.0.0.5:9000",  audience = "consumer" },
]
```

| Key | Meaning |
| --- | --- |
| `bind` | Staff listeners (the ERP and the console clients). Required, as always. |
| `listeners[].bind` | One more address. |
| `listeners[].audience` | `"staff"` or `"consumer"`. Required; any other word is refused. |

- A config with no `listeners` behaves exactly as before.
- **Staff** listeners serve the staff router: every route marked for staff or partner, the
  module routes (SCIM, realtime, second factor, webhooks) and the open paths a module declared.
- **Consumer** listeners serve only the consumer router: the routes the table marks for
  consumers and nothing else. Sign-in, password change, realtime, metrics, the module routes and
  the open paths are staff surfaces and are not mounted there.
- Two listeners may not overlap: the same address, or a wildcard address (`0.0.0.0`, `::`) and
  any address on the same port, whatever their audiences. The host refuses to start and names
  the clashing address. Port `0` never clashes.
- While no route is marked for consumers, a configured consumer listener is **not started**:
  the host logs once at info level
  (`the configured consumer listeners are not started: no route is marked for consumers yet`),
  and the staff listeners are unaffected. It starts as soon as a release marks a consumer route.
- Listener changes need a **restart**. A reload (a `reload` file in a site folder) swaps the
  site runtimes behind the listeners, staff and consumer routers together, and never touches
  the sockets.

## Rate limits of the public routes

The public route groups (those the contract marks `security: []`) are limited per client address
by one budget, on every router the host builds:

| Key (in `[server]`) | Default | Bounds |
| --- | --- | --- |
| `auth_rate_limit` | `20` | 1 to 10000 requests per window per client address |
| `auth_rate_window_seconds` | `60` | whole seconds, 1 to 3600 |

A value outside its bounds, a negative, fractional or non-numeric value stops the start with a
message naming the key (`auth_rate_limit` or `auth_rate_window_seconds`), before any site is built
or socket bound. Behind a reverse proxy, list the proxy in `trusted_proxies`: otherwise every
client shares the proxy's address and therefore one budget. See
[the HTTP routes note](http-routes.md) for the eviction rule of the table.
