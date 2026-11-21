Part of [Running Spot](../running-spot.md).

# Tenant links (`[services.links]`)

Links let two tenants (or a tenant and an external peer) exchange agreed topics and fields. The
service is **off by default**. Nothing flows between two tenants until **both** sides' administrators
have consented (each consent needs a fresh strong sign-in), and then only what the grants list allows.
The default grants list is empty.

## Enabling

```toml
[services.links]
enabled = true
tenant  = "<the tenant's UUIDv7>"
```

Unknown keys are refused. When enabled, the site needs three ports that the identity and audit
modules provide together as `LinksPorts`: the audit chain adapter, the step-up gate and the
`link.manage` check. If none is provided the site refuses to start and names the missing port: links
never run without audit or consent checks. The service's start-up check creates the tenant's link tables
(idempotent). It is an optional service: if it stops, the site stays ready and says it is degraded.

## What the operations do

Propose, accept, reject, grant, revoke, list and show are the operations of `spot-tenant-link-pg`
(`LinkAdmin`). Each checks the `link.manage` right first, writes its audit record **before** it acts,
and re-checks every consent through the step-up gate. Either side may revoke at any time; new
messages stop immediately (messages already acknowledged stay). Peer identity keys are pinned: a key
that changes is refused until an administrator confirms the new one again, with a fresh consent.

## The control plane's part

The control plane keeps a **directory** and a **registry**, nothing else, and relays nothing:

- the directory maps an opaque handle that a tenant chose to the https endpoint of its host, so the
  other tenant's host can find it for the handshake. A tenant's name appears only if its owner opted in;
- the registry records that a link exists, between whom, and whether it is live. It holds no grant,
  topic, field, key or consent.

Link data travels host to host over mutual TLS and never through the control plane.

## Status of the HTTP endpoints

The native API operations for links are not in the contract yet. Until they are, the service provides
the `LinksPort` to other modules and the handshake endpoints of `spot-link-transport` are mounted by
the transport module; the operator-facing endpoints follow with the contract.
