Part of [Running Spot](../running-spot.md).

# Process conversations

The conversation module (`crates/framework/spot-cli/src/services/process_conversations.rs` and
its submodules, `ORDER = 1180`) lets a process send a message to another organisation and wait for
the reply over a tenant link (ADR-080 section 5, ADR-058). `spot-bpm-conversations` holds the
rules (the process contract, the order of messages, the gate, the view); `spot-bpm-conversations-pg`
keeps the conversation records in PostgreSQL; this module connects them to one site. It is an
**optional** service: a broken conversation service degrades readiness and the processes that talk
to other organisations, never document serving.

## What it does

| Part | What |
| --- | --- |
| Node handlers | The `send_message` and `wait_message` steps, registered with the process engine through the `HandlerBuilder` port. They run off the runtime thread, because they call the conversation store. |
| Trigger kind | `message_arrived`: a published process whose trigger node names a contract message the contract allows as an entry message starts one request per arriving message. |
| Receiver | `LinkMessageSink` (a port the link host calls with every inbound message). |
| Sender | One `LinkSender` per configured link, over the link host's outbox, the site's identity key and the link's current state. |
| Driver | A pass every `tick_secs` seconds, as the holder of the consumer lease `process_conversations`: arrivals first (they start requests and wake waits), then the timeouts and delivery waits. |

## The link host

The link host (inbound link traffic, link stores, carriers) is another hosting task. This module
needs two things of it and offers one:

- the port `LinkResources`, which the link host provides: the link as the host holds it (so a
  revocation or expiry there stops the sender and the receiver at once), the link stores, the
  outbox, inbox, delivery notice feed and input quarantine of the exchange, the link audit chain,
  the optional region oracle and the retention of what was sent;
- the port `LinkMessageSink`, which this module provides: the link host passes every inbound
  message with the party its carrier authenticated, and returns the receipts of the answer to the
  peer.

Without `LinkResources` the module stays inert and logs `no link host; no conversations`.
`process_conversations::memory` implements `LinkResources` over in-memory stores and carries
messages between two sites of one process (`carry`); that is what `tests/process_conversations.rs`
runs on, and it is usable for a demonstration that holds both halves of a conversation.

The link stores the host offers are called from where the receiver and the driver pass run (a
runtime worker): they must be safe to call from there. The conversation store is not a problem of
the host's: this module takes every call to it off the runtime thread.

## Per-message rules

The link is read from the host **for every message**.

- **Grants, direction, expiry, revocation, region.** The inbox judges the message against the
  link as it is now (`Link::allows_with_region`, with the host's oracle) before any Run is touched.
  A message from a revoked or expired link, on a topic with no grant, in the wrong direction or
  from a peer the oracle places outside the grant's required region is refused or parked with an
  audit row and starts or wakes nothing. A grant that requires a region and has no oracle refuses
  every message (fail closed).
- **Identity and ids.** A reply wakes a waiting Run only if it names the cryptographic Run id and
  arrives on the link the conversation was opened on. A message from another link that names the
  id is dropped, and nothing in the answer says whether the id exists.
- **Replay.** The same message id applies once.
- **Contract.** A message that is not a known one, not allowed after the previous one, from the
  wrong role or outside the schema is refused with a reason code the sender receives and is held
  in the quarantine.
- **Late replies.** A reply that arrives after its wait timed out is recorded and held, not
  applied.

## Limits

All per link, all from the settings; nothing grows without bound.

| Setting | Default | What happens over it |
| --- | --- | --- |
| `max_open_conversations` | 64 | a message that would open another conversation is refused (`bpm_conversations.link_conversation_limit`) with an audit row |
| `max_held_inputs` | 512 | a refused input is answered but no longer held in the quarantine |
| `max_messages_per_minute` | 600 | the message is **not taken** (the sender keeps it and sends it again), an audit row `process_conversations.rate_limited` is written, readiness warns |
| `max_payload_bytes` | 65536 | the message is refused, an audit row `process_conversations.payload_too_large` is written, readiness warns |

## Audit and logs

Every refusal writes an audit row into the link's own chain: the reason code and the link,
conversation and message ids, never a payload. Work the module does as the platform (a request
started by a message, a wait woken by one, a timeout) is logged under the target
`spot::audit::process_conversations` as the service principal `service:process_conversations`.
No message body, field value, signature, token or secret is logged, and no id is a metric label.

## Configuration

```toml
[services.process_conversations]
enabled = true                    # default true
tick_secs = 5                     # 1..=300
max_open_conversations = 64
max_held_inputs = 512
max_messages_per_minute = 600
max_payload_bytes = 65536         # 1024..=262144
wait_for = "queued"               # queued | acknowledged | applied
identity = "link-key"             # the reference in `secrets`

[services.process_conversations.secrets.link-key]
sealed = "spsec:v1:..."           # a 32-byte seed sealed under the site's [services.secrets] key
aad = "link-identity"

[services.process_conversations.contracts.request_approve]
path = "contracts/request_approve.toml"   # relative to the configuration root, plain components
role = "approver"                         # the role this site plays

[[services.process_conversations.peers]]
contract = "request_approve"
role = "requester"                # the role the peer plays
link = "<link id>"
```

Unknown keys are refused. The configuration names the identity key by reference; it never holds
one in clear. A contract path with `..`, a leading slash, a backslash or any character outside
`[A-Za-z0-9._-]` is refused, a contract file larger than 1 MiB is refused, and a contract with any
problem is refused whole (a partner may have supplied it). A missing or unusable identity key
leaves the service degraded (`no usable identity key`), the site still serves documents.

## Health

The check reads the published process versions and warns when a link went over its message rate
(`messages were not taken because a link went over its rate`), when payloads were refused for
their size, and when published versions have triggers that do not build. A conversation store that
cannot answer fails the receive and the driver pass closed: nothing is acknowledged and the sender
keeps the message.
