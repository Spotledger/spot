# Link and metering port providers

The link exchange, external link and usage emitter services run over ports the host provides. This page
lists what each provider here does and what is still open.

## Provided

* `ProbeHold` (`SendHold`): wraps a probe of the effect hold's state. Unknown is held (fail closed). The
  host passes `|| Some(!matches!(gate.snapshot(), HoldSnapshot::Open))` over its `spot_effect_hold::Gate`.
* `SeededResilience` (`ResilienceFactory`): a seeded jitter per link (the host injects the seed) and the
  Tokio timer, sharing the host's bulkhead.
* `PgNonceStore` (`NonceStore` of the external link): one row per `(peer, nonce)` in `<prefix>_nonce`, the
  primary key decides who claimed first, so replay refusal survives a restart and holds across nodes.
  Call `prepare()` once at start. The inbound route needs a multi-thread runtime (the store refuses on a
  current-thread one). The file store stays for a single node; both pass one conformance test.
* `StoredSalt` (`SiteSalt` of the usage emitter): refuses a salt under 16 bytes or all zero and never prints it.
  The host reads the bytes from the secret store.
* The usage outbox needs no new provider: `PgOutbox` in `services/metering.rs` is already the PostgreSQL outbox.

## Not provided yet (needs a path or a dependency outside this task)

* the mutual-TLS transport for the usage shipper (`rustls` PEM loading is not a dependency of `spot-cli`);
* `PeerDirectory` (pinned certificates and endpoints per link: the same transport and the link configuration);
* `LinkRecordApplier` (replica apply as the link principal: the engine's apply path);
* the usage `UsageSources` counters (the host's request and storage counters).
