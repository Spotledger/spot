Part of [Running Spot](../running-spot.md).

# Outbound webhooks

A site can tell another system when a document changes. You create a `webhook` document (a target
URL, the DocType, the events, an optional condition, a payload mode, headers) and every committed
event that matches becomes one signed `POST`, with bounded retries and a dead-letter state. The
module is `crates/framework/spot-cli/src/services/webhooks.rs` (and `webhooks/*.rs`); the delivery
logic itself is the `spot-webhooks` crate.

Webhooks are an optional service. If they cannot run, the site keeps serving and `/api/health/ready`
reports it degraded; nothing is delivered until the cause is fixed. Nothing is ever sent unsigned.

## What you need

- **The secrets vault** (`[services.secrets]` in `common.toml`, see [Secrets](secrets.md)) and a
  signing secret on each webhook ([Webhook signing secrets](webhook-secrets.md)). Without the vault
  the service reports `no secrets vault` and sends nothing.
- **The `webhooks` job queue.** Add it to `services.jobs.queues`, or the worker never claims a
  delivery and the service reports the missing queue:

  ```toml
  [services.jobs]
  queues = ["default", "webhooks"]
  ```

- The `webhook` and `webhook_delivery` DocTypes, which the platform app creates on every site.

## Configure

`[services.webhooks]` in `common.toml`. Unknown keys are refused at start-up, and so is a value out
of range. Defaults are conservative.

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | | `false` turns the module off completely. |
| `max_attempts` | `5` | 1 to 20 | The retry ceiling, the first attempt included. A webhook can ask for fewer, never more. |
| `max_payload_bytes` | `262144` | 1 to 1048576 | The most bytes one payload may take. |
| `per_endpoint_concurrency` | `4` | 1 to 64 | The most deliveries in flight to one endpoint. |
| `timeout_secs` | `15` | 1 to 120 | The request timeout. A webhook can ask for less, never more. |
| `max_response_bytes` | `65536` | 1 to 1048576 | The most response bytes read. The body is discarded, never stored or logged. |
| `poll_ms` | `1000` | 100 to 60000 | How often the intake reads the outbox. |
| `batch_size` | `100` | 1 to 500 | Outbox events per read. |
| `allowed_hosts` | `[]` | | When set, the only hosts (`host` or `*.suffix`) a webhook may reach; everything else is refused. |
| `replay_history` | `false` | | On the first run, also deliver events committed before the module ever ran. |

## How a delivery happens

1. **Intake.** One node holds the `webhooks` outbox lease and reads the outbox from a cursor kept in
   the site database (the `spot_webhook_intake` table, one row per site). The cursor moves only after
   a page of events has been turned into jobs, so a restart or a lease change re-reads, never skips.
2. **Match.** An event matches an enabled, active webhook when the DocType and the event name are
   listed. If the webhook has a condition, it is evaluated on the document as it is now; a condition
   that is false, or fails to evaluate, sends nothing. Events about `webhook_delivery` and about
   `sys_` types never trigger a webhook.
3. **Job.** One job per event and webhook, on the `webhooks` queue. The **delivery id** is derived
   from the webhook and the event (a UUID v5), and the job's idempotency key is built from the same
   two ids, so a replayed intake creates no second job.
4. **Send.** The delivery handler opens the signing key, signs and sends through the site's egress
   guard with the `webhook` purpose. The guard refuses private, loopback, link-local (including the
   cloud metadata address), unique-local, multicast and unspecified addresses, and connects to the
   address it checked. **Redirects are not followed:** a 3xx is a failed delivery. A failure retries
   with back-off up to the attempt ceiling and then dead-letters. A run of failures to one endpoint
   opens that endpoint's circuit breaker, and deliveries wait until it closes.
5. **Log.** Each attempt is one `webhook_delivery` document: state, status code, an error class,
   timing, the delivery and event ids. It holds **no payload, no response body, no URL and no
   message from the target**.

**Delivery is at least once.** A crash between the send and the log can send the same delivery
again. Every retry carries the same delivery id (header `X-Spot-Delivery`) with a fresh timestamp in
the signature; a receiver dedupes on that id and rejects a stale timestamp.

## What goes in the payload

The document is read by the platform itself and then cut down to the fields with **no protection
group, no classification and no mask**. A field a least-privileged reader could not see never enters
a payload or a job row. The payload modes (`ids`, `fields` with an allow-list, `full`) then select
from what is left. A payload larger than `max_payload_bytes` is cut down. A job argument larger than
the job store accepts carries the document id only.

The intake acts as the platform (`SystemReader`), not as the webhook's owner; the field cut above is
the same for every webhook, so the owner's own read rights do not widen it.

## Worked example

Tell a receiver when a sales order is posted.

1. Make sure `services.jobs.queues` holds `webhooks` and `[services.secrets]` is set; restart.
2. Create a `webhook` document (as Tenant Administrator or a role with write on `webhook`),
   through the resource API every DocType uses, with these fields:

   ```
   {
     "name": "orders-posted",
     "url": "https://receiver.example.com/spot",
     "doctype": "sales_order",
     "events": ["post"],
     "condition": "doc.grand_total > 1000",
     "payload_mode": "full",
     "enabled": true,
     "state": "active"
   }
   ```

3. Set its signing secret: `PUT /api/v1/webhooks/orders-posted/signing-secret` with
   `{"generate": true}` and give the answer's `secret` to the receiver once.
4. Post a sales order above 1000. Within about a second (`poll_ms`) the receiver gets one `POST` with
   `X-Spot-Signature`, `X-Spot-Event-Id` and `X-Spot-Delivery`. Verify it as in
   `crates/framework/spot-webhooks/docs/receiver-example.md`.
5. Look at `webhook_delivery`: one row per attempt, `state` `delivered`, `retrying` or `dead_letter`.

## When it does not deliver

| Sign | Cause and fix |
| --- | --- |
| Readiness says `no secrets vault` | Configure `[services.secrets]`; webhooks cannot sign without it. |
| Readiness says `webhooks is not in services.jobs.queues` | Add the queue and restart. |
| Deliveries `dead_letter` with an egress error class | The target resolved to a refused address, a host outside `allowed_hosts`, or answered a redirect. Fix the URL. |
| Deliveries `dead_letter` with `secret.not_set` | The webhook has no signing secret yet. Set it; see [Webhook signing secrets](webhook-secrets.md). |
| Nothing at all for a new webhook | The webhook is `paused`, `disabled` or `dead_letter`, or the event is older than the module's first run (`replay_history`). |
| One node never delivers | Only the node holding the `webhooks` outbox lease runs the intake; the others stand by. |

## Limits of this version

- The delivery log is a `webhook_delivery` document per attempt written by the platform, not a
  dedicated table. The circuit breaker state is in memory per node and starts closed after a restart.
- The `webhook` and `webhook_delivery` documents are managed through the generic resource routes;
  the native API contract has no webhook-specific operations, so there are no `/api/v1/webhooks/...`
  routes other than the signing secret.
