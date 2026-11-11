Part of [Running Spot](../running-spot.md).

# Integration nodes (HTTP, e-mail, outgoing webhook)

`services.process_integration` hosts the Integration node handlers from `spot-bpm-integration`
for the process engine (`services.process`): an HTTP call, an e-mail, or a signed outgoing
webhook, each reaching the outside world only through this module's own hosted egress client
— no handler opens a socket itself.

## What is hosted

Three handlers, one `spot_bpm_engine::NodeHandler` registered with `services.process`'s
`HandlerBuilder` port for the `Integration` node kind:

- **HTTP** — a request built from the node's config and the Case's data, sent through an egress
  `Guard` and `Sender`; a credential is put on the request only when the rendered URL's host is
  one the node's own configuration names literally (WP-BPM-06.T2's rule), never one steered by
  data.
- **E-mail** — built and validated by `spot-bpm-integration`, then queued durably (see below)
  rather than dialed inline.
- **Outgoing webhook** — signed with `spot-webhooks`' scheme (`X-Spot-Signature`) and posted
  through the same kind of hosted egress client as HTTP.

A TypeScript code node and a WASM connector node (WP-INT-05) are not hosted by this module;
`spot-bpm-integration` carries their handlers for a later task to host.

## The node's config: `connector` selects the kind, `endpoint` carries it

The Process model's `IntegrationConfig` (blueprint node type `Integration`) has only three
fields: `connector`, `action` and `endpoint` — no room for a URL template, headers, a body
mapping, a credential reference, a retry policy or e-mail fields. Widening that model is
`crates/framework/spot-bpm-model`, which this task does not own, so this module instead reads:

- `connector`: `"http"`, `"mail"` or `"webhook"` — which hosted handler runs the node.
- `endpoint`: the node's full, kind-specific config, JSON-encoded as text — exactly the shape
  `spot-bpm-integration`'s `HttpNodeConfig`, `MailNodeConfig` or `WebhookNodeConfig` parses from
  JSON (see that crate's docs for every field).
- `action` is not read by this module.

A node whose `connector` names a kind nothing here hosts, or whose `endpoint` is not valid
JSON for that kind, fails with `bpm_integration.config_invalid` — named before any call is
made, never partway through one. This is a deliberate workaround for the model gap, not a
long-term shape; see this task's report, `## Open questions`, for the alternative (widening
`IntegrationConfig`) and what it would need.

## Credentials

`[services.process_integration.credentials]` names sealed entries a node's config may
reference (`credential_ref`):

```toml
[services.process_integration.credentials.sales_api]
sealed = "spsec:v1:...."
aad = "integration:sales_api"
```

Each is opened through the site's `[services.secrets]` key provider at call time only, and
never cached: `spot-bpm-integration`'s redaction pass scrubs the value from every node
output, the Run's stored variables, a log line and an error message before any of those are
observable. Without `[services.secrets]` configured for this site, or without a matching
entry here, a credentialed node fails closed with `bpm_integration.credential_missing` — there
is no plaintext fallback.

## Egress policy

HTTP and webhook destinations are whatever the node's config and Case data name, so this
module's guard has no per-host allow-list: it relies on the egress crate's own address and port
classification for SSRF protection (a private, loopback, link-local or cloud-metadata address
is refused; a dangerous port — 22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250 — needs
an explicit allow entry this policy carries none of). A redirect to another host is re-checked
by the same policy on every hop, so a credential never follows one to a host the node's config
did not name.

E-mail reaches only the site's configured `services.mail` SMTP host and port — the e-mail node
has no host of its own to name — under a `deny_non_public` policy with one allow entry, the
same shape `services.mail` restricts its own account to.

A response body is capped at `max_response_bytes` (default 1 MiB) and never stored whole in
the Run: only the fields a node's response mapping picks out are.

## Mail: durable and idempotent through the job queue

`spot-bpm-integration`'s `MailPort` trait carries the rendered message only, so the module
scopes the engine's `(case, node, attempt)` key around each node run and submits a job
(`bpm_integration:send_mail`, queue `default`) whose idempotency key is
`bpm_integration_mail:<case>:<node>:<attempt>`. The same attempt run twice (a replay after a
crash) is one job; two cases with identical mails are two jobs; a later attempt is a new job.
The key is never derived from the message content. A send with no engine key in scope fails
closed. The node completes once the job is durably queued; the job handler performs the guarded
SMTP send afterwards.

The SMTP relay's name is resolved and vetted by the egress guard on the first send, not when the
site starts, so a DNS failure at start never stops the site; the queued job simply retries.

Without `[services.mail]` configured and enabled for this site, an e-mail node fails closed
with `bpm_integration.mail_permanent` rather than silently dropping the message.

## Configuration

`[services.process_integration]`:

| Key                  | Default      | Notes                                                        |
|-----------------------|--------------|---------------------------------------------------------------|
| `enabled`             | `true`       | `false` makes the module inert: no handler, no job handler.   |
| `max_response_bytes`  | `1048576`    | Must be at least 1.                                            |
| `timeout_secs`        | `30`         | `1..=600`. The call timeout; a node's own `timeout_secs` is capped to it. |
| `max_retry_attempts`  | `5`          | `1..=20`. A node's `retry.max_attempts` (default 3) is capped to it.     |
| `credentials`         | `{}`         | Name to `{ sealed, aad }`, each a `spot-secrets` sealed entry. |

Unknown keys are refused.

## Runtime

The engine's node handlers are synchronous, so the module bridges onto the asynchronous handlers
with a blocking call on the multi-thread runtime the host always builds. Called outside a
runtime, or on a current-thread one, the node fails closed with `bpm_integration.transport_failed`
instead of panicking. A call that exceeds the timeout is reported by the egress sender as the
same transport code and retried within the node's policy.

## Start order

`ORDER = 1115`: after `services.process` (1100, whose `HandlerBuilder` port this module
requires) and `services.jobs` (whose `JobSubmit` port this module requires for the e-mail
queue), and before any later node-kind module in the 1100–1230 series.

## Health

The module adds an optional service with no loop of its own; a stopped or missing
`[services.secrets]` or `[services.mail]` table degrades only the node kinds that need them
(a credentialed node, or an e-mail node), never the site's own readiness.
