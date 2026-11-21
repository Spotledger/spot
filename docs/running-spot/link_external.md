Part of [Running Spot](../running-spot.md).

# External peers (`[services.link_external]`)

Runs the external carrier of a tenant link: message topics exchanged with a peer that is not a platform
tenant, over a connector or a signed webhook, under the same grants as an internal link. **Off by default**:
disabled there is no task, no route and no network call.

```toml
[services.link_external]
enabled             = true
tick_ms             = 1000   # outbound pass
pump_batch          = 16     # messages per peer per pass
per_peer_per_minute = 120    # inbound requests per peer name
```

Unknown keys and zero values are refused. The service needs `LinkExternalPorts` (stores, the configured
peers with their link and bridge identity, the egress-policy channel, the credential vault, a durable nonce
store and the effect-hold adapter), provided by the module that owns them; enabled without it the site
refuses to start and names the port. It is an optional service.

## Outbound

A message the outbox releases goes to the peer's channel, which delivers **through the egress policy**: only
allow-listed hosts are reachable, a denied host is a failed delivery (audited, the message waits and is
retried until it is answered or expires). The peer's credential is a **reference** resolved from the vault for
the one call; no secret is stored here, logged or echoed. While the side-effect hold is on (or unknown),
nothing leaves.

## Inbound: `POST /api/v1/link/inbound/{peer}`

The registry group `link_inbound` (native API 0.26.0, public, no bearer). It is mounted only when the
service is enabled, because the service provides the inbound port only then. An external peer's configured
URL is `https://<site host>/api/v1/link/inbound/<peer>`. The peer name is the external peer's id (`a-z`, `0-9`,
`_`, `-`). In order:

1. the per-peer **rate limit**, charged to the name in the path before the peer is looked up (so known and
   unknown names are limited alike), then the **source-address allow-list** when configured, then the peer's
   **body cap** (read while streaming, never buffered past it);
2. the **HMAC signature** over the raw body, with a timestamp window of at most **5 minutes**;
3. the **replay check**: the signature of an accepted request is claimed in a durable nonce store
   (`FileNonceStore`: append-only file, 0600, synced before it answers, read back on start, bounded). A second
   use is refused, also after a restart;
4. the tenant link's receive path: grant limits, field list, schema range, deduplication, ordering, audit.
   Fields outside the grant are refused; the message is stored through the inbox under the link's grant, as the
   peer's link principal, never as a system actor.

Every refusal is the same `401 link_inbound.unauthorized` (same status, headers and body): an unknown peer, a
bad or missing signature, a stale timestamp, a replay, a source outside the allow-list, an over-cap body and a
malformed name cannot be told apart. The only other answer is `429 api.rate_limited` (with `Retry-After`) for the rate limit. Success is
`202 {"accepted": true}`.

## Not done here

The adapters behind the ports (peer directory, vault, durable store in PostgreSQL instead of the file, hold
gate, the egress policy wiring) belong to the modules that own them.
