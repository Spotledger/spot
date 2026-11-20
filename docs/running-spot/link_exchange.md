Part of [Running Spot](../running-spot.md).

# Link exchange runner (`[services.link_exchange]`)

Runs the exchange of signed record and message topics over a tenant link: the direct carrier (this
side dials the peer) and the pull carrier (this side long-polls the peer's mailbox). **Off by default**:
disabled, there is no task, no route and no network call.

```toml
[services.link_exchange]
enabled              = true
tick_ms              = 1000   # supervisor pass
dead_letter_attempts = 20     # hand-overs without confirmation before a message is dead-lettered
per_link_per_minute  = 600    # steps per link, per direction (also the inbound endpoint limit)
apply_batch          = 32     # pending records applied per pass
max_in_flight        = 16     # calls in flight across all links
```

Unknown keys and zero values are refused. The runner needs `LinkExchangePorts` (stores, peers, the
effect-hold adapter and the engine apply path), provided by the module that owns them; enabled with no
provider the site refuses to start and names the port. It is an optional service.

## What it guarantees

- **Fields.** The grant's field list is applied when a message is enqueued, before it is signed:
  a field not granted is never in the payload that leaves the process (only its name is audited).
- **States and ids.** `Queued -> Sent -> Acknowledged -> Applied | Rejected | Expired`. A message is
  signed once and resent byte for byte, so its id is stable and the receiver deduplicates.
- **Retries.** A failed step backs off exponentially (doubling from the client's base to its cap); the
  client's breaker probes a dead peer at its open period.
- **Dead letters.** After `dead_letter_attempts` hand-overs without confirmation a message expires
  early: tombstone for its slot, producer event `Expired`, later messages are not held behind it.
- **Rate.** A token bucket per link and direction on the sender; the served endpoints use the same
  limit per link.
- **Revocation.** Each iteration re-reads the link; one that is not `Active` ends its tasks at once.
- **Hold.** While the side-effect hold is on, or its state is unknown, nothing leaves (push and pull);
  queued messages wait.
- **Inbound.** The peer is identified by the connection, never by the body. Fields outside the
  grant are refused by the inbox. A pending record is applied through the engine as the **link
  principal**: the peer with exactly the grant's topics, not a system actor; a link revoked meanwhile
  never produces an applied row.

## Not done here

The adapters behind the ports (hold gate, replica apply as the link principal, peer directory) belong
to the modules that own those services.
