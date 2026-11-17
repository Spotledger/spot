# The `edi` connector kind

An operator guide for the built-in connector kind `edi` (WP-INT-06.T2).

An X12 or EDIFACT interchange posted to a connector's inbound ingress (or dropped through the
file service) becomes documents, one per transaction set, through the lifecycle as the connector's
**service principal** (an ordinary user with narrow roles, never the administrator). A 997 or
CONTRL acknowledgement is built and queued on the connector's outbound path.

## What is checked, in order

1. The input is bounded (`Limits`: bytes, segments, elements per segment). Oversize or malformed
   input is refused and audited; nothing is acknowledged because nothing parsed.
2. The interchange control number is claimed per tenant, connector and sender. A repeat is
   acknowledged with every set rejected as `duplicate_interchange`; nothing is created twice.
3. Each set stands alone. Rejected in the acknowledgement, never escalated: no mapping
   (`unsupported_set`), a schema violation, a mapping failure (`mapping_failed`), a refusal by the
   principal's rights (`not_permitted`), an invalid or unavailable lifecycle.

Element values are plain text in the mapped document; they are never read as a path, a query or a
formula. The audit records control numbers, counts and the outcome, never a segment value.

## Sending

`send` fills a template interchange from a document through the mapping, takes the next control
number (durable, never repeated, scoped per tenant and partner) and delivers through the
connector's egress frame (allow-list, no private or metadata address).

## Configuration

A mapping per set id (`850`, `ORDERS`): the DocType and a mapping spec over the message view
(`segments.BEG.s0.e2` is the second element of the first `BEG`), and an optional schema.
