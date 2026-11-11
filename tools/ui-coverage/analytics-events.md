# Usage analytics: event schema for the telemetry endpoint

Owner of the endpoint: the server telemetry work (aggregation is out of scope for WP-UI-06.T1).
Client: `Spotledger.Client.Analytics` (`UsageAnalytics`, `HttpAnalyticsSink`).

## Principles

- **Opt-in.** Consent comes from the user's settings (`IAnalyticsConsent`, off by default). With consent off nothing is recorded and nothing is sent.
- **Counters and ids only.** No document data, no user text, no field values, no user id, no timestamps beyond the day. Every id passes `AnalyticsIds` (screen, action and shortcut ids `^[a-z0-9][a-z0-9._-]{0,63}$`; error codes the same with upper case allowed).
- **HTTPS only** (WP-UI-06.T2). `HttpAnalyticsSink` refuses to send unless the effective request URI (base address plus `api/telemetry/usage`) is `https`, and it never follows a redirect, so an `https` endpoint cannot bounce a batch to `http`. The single exception is a loopback host (`localhost`, `127.0.0.1`, `[::1]`) for a developer running a local server: the host has to pass `allowLoopbackHttp: true`, which it sets only when its own server URL is loopback - never from a setting the user edits. A refusal throws (`AnalyticsEndpointNotSecureException`), no request is made and the batch stays queued.
- **An anonymous ingest token, never the user's session** (WP-UI-06.T2). The sink builds its own `HttpClient` with no default header, no cookie container and no redirect handling, and sends exactly one credential: the ingest token from `IAnalyticsTokenSource`, in the `X-Spot-Ingest-Token` header. It is not the session token, the API key or any credential of the user, so a batch cannot be tied to a person. A client that carries a default `Authorization` header, a cookie container or an authentication-looking handler is rejected at construction (`AnalyticsClientNotAnonymousException`). With no ingest token configured the sink sends nothing and `FlushAsync` reports `FlushResult.NoIngestToken`; the counters stay local.
- **Screen ids are validated against the screens manifest** (WP-UI-06.T2). `KnownScreens.Manifest` is generated at build time from `screens/*.toml` (`KnownScreens.g.cs`, the same files and the same `example*` exclusion as `check.py`), so the client reads no file at runtime. `ScreenOpened`, `ActionUsed` and `ShortcutUsed` record only a listed id; an unknown id is dropped, counted in `rejected` (a number; the refused text is never stored, logged or echoed) and bounded at 1,000,000. An empty manifest means an empty set, which rejects every screen id: nothing is recorded until a real screen is declared.
- **Install-random id that rotates on opt-out** (WP-UI-06.T2). `install_id` is 128 random bits (32 lowercase hex), generated once per install and kept in `IInstallIdStore`. Turning analytics off - or calling `Clear()` for a withdrawal - discards the stored id and the pending counters, and the next opt-in creates a new id with `InstallId.Create()`. A batch that is in flight when the user withdraws is cancelled, not sent. Nothing is linkable across an opt-out; two on/off cycles yield three different ids. The inspector payload carries an empty `install_id` while consent is off.
- **Visible.** The settings inspector (`UsageAnalytics.PendingPayload` / `InspectJson`) shows exactly what the next batch contains.

## Request

`POST api/telemetry/usage` over **https**, with `Content-Type: application/json` and the deployment's ingest token in `X-Spot-Ingest-Token` (never `Authorization`; the sink never logs the token). Where the token comes from is the endpoint owner's decision: a deployment setting issued by the telemetry endpoint, handed to the client through `IAnalyticsTokenSource` (a management/bootstrapping call, not the user's login). The endpoint should reject a batch with no token or an unknown one; it should not accept a session token in its place.

Success is any 2xx. A 3xx is a failure (redirects are never followed), and on any other status the client keeps its counters and retries on the next flush. The client sends at most one batch per flush and never more than 2,048 counters.

```json
{
  "schema_version": 1,
  "install_id": "9f3c0a5e7b1d4c2a8e6f0b3d5a7c9e1f",
  "day": "2026-10-23",
  "counters": [
    {"event": "screen_opened", "screen_id": "sales.invoice.form", "count": 12},
    {"event": "action_used", "screen_id": "sales.invoice.form", "action_id": "save", "count": 9},
    {"event": "shortcut_used", "screen_id": "sales.invoice.form", "shortcut_id": "form.save", "count": 4},
    {"event": "error_shown", "code": "SP-1042", "count": 2}
  ],
  "dropped": 0,
  "rejected": 0
}
```

| Field | Type | Notes |
| --- | --- | --- |
| `schema_version` | integer | 1. Additive changes keep the number; a rename or removal bumps it. |
| `install_id` | string | 32 lowercase hex, or empty in the inspector while consent is off (nothing is sent then). A new id is created after every opt-out. |
| `day` | string | `yyyy-MM-dd`, the day the caller passed to the flush (client local day; the client never reads the clock itself). |
| `counters[].event` | string | `screen_opened`, `action_used`, `error_shown` or `shortcut_used`. |
| `counters[].screen_id` | string | Present for every event except `error_shown`; always an id the screens manifest lists. |
| `counters[].action_id` | string | `action_used` only. |
| `counters[].shortcut_id` | string | `shortcut_used` only: the command id, not the key combination. |
| `counters[].code` | string | `error_shown` only: the error code, never the message. |
| `counters[].count` | integer | Events of this key since the last successful flush; at least 1. |
| `dropped` | integer | Events refused because the client's counter table was full. |
| `rejected` | integer | Screen ids refused because the screens manifest does not list them, capped at 1,000,000. A number only: the refused text is never kept, logged or sent. |

The endpoint should reject any key outside this list, any id that fails the patterns above, and bodies over 256 KiB. It must not require a user session: the request carries the deployment's ingest token and nothing else.
