Part of [Running Spot](../running-spot.md).

# Journal access: reading the whole history from outside

`GET /api/v1/journal`, `GET /api/v1/journal/manifest/{generation}` and
`GET /api/v1/journal/segments/{generation}/{first_seq}-{last_seq}` let a warehouse, a lakehouse
or an auditor read the tenant's whole journal without loading the platform (ADR-061 section 7,
surface 2; contract 0.16.0). **The segment bytes are the whole tenant, unfiltered, by design**:
no row, field or permission filter applies. The controls are the grant and the audit, and both
fail closed.

## Who may read (default off)

- The role `journal_reader` holds one permission, `journal.read`, and no document right. **Nobody
  holds it until a tenant administrator grants it** to a named principal. A tenant administrator
  may read too, through the same audited path (their records say `grant = admin`).
- A grant has an expiry. The longest lifetime is `max_grant_days` (default 30, hard cap 365). A
  new grant for an **external** principal (an auditor, a lakehouse job) is at most 7 days; an
  administrator extends it only by an explicit renewal.
- Only a tenant administrator grants, renews, revokes or lists. A `journal_reader` cannot grant
  the role. A grant is **void when its grantor stops being an administrator**.
- A revoked or expired grant is refused at the next request: no decision is cached longer than
  5 seconds, and a revoke on this node drops the cache at once.
- Every grant action is a `sys_journal_grant` event (granted, renewed, revoked, refused) written
  in the same transaction as the grant row in table `spot_journal_grant`.

Every refusal of a caller without a usable grant is the same `403 permission.denied`; only an
expired grant adds `details.reason = grant_expired`. The status never says whether a grant once
existed.

## What is served

- `journal_info_get`: the generations (`first_seq`, `last_seq`, `sealed_until_seq`, `opened_at`),
  the durability policy name and the region.
- `journal_manifest_get`: the signed manifest of one generation (a readable summary plus
  `signed_bytes`, the stored byte form in hex, which a verifier checks with the tenant's public
  keys).
- `journal_segment_get`: one **sealed** segment, `application/octet-stream`, with `Range`
  (`bytes=a-b`, `a-`, `-n`) so a transfer resumes after a cut. `ETag` is the segment's recorded
  hash, `Cache-Control: private, no-store`.

A segment is served only if the signed manifest **lists exactly that segment**; the manifest is
read from the store and verified against the tenant's public keys on every call, so no name comes
from the URL. A tail chunk, a span of several segments, an unknown generation, `..`, an encoded
separator, a padded or non-numeric number is `404 journal.range_unavailable`. The segment is
checked against its recorded hash **before the first byte is sent**; a segment that does not match
is not served (`503`, a `failed` record). Segments are at most 64 MiB.

Direct object-store credentials (a signed URL) are not issued: the platform serves the bytes
(the standalone and edge profiles use a folder store). A site whose journal store is an object
store answers the direct form `501 service.not_configured`; it is a later task.

## Limits

Per principal: `max_concurrent_streams` simultaneous segment streams (default 4) and a
`daily_byte_budget` (default 64 GiB per UTC day, charged when the range is accepted). Over either
the answer is `429 api.rate_limited` with `Retry-After`. The bytes read per principal today are a
library metric (`Limits::snapshot`) the host exports.

## The audit

**Every call writes one `sys_journal_access` event**, denied, not found, expired and failed calls
included: who, which grant, the object, the generation and range, the outcome, a bounded reason
code, and the bytes sent (never content). A denial is written before the answer; a served segment
is written when its stream ends (a stream the client abandons is recorded as `failed`, reason
`aborted`). If the record cannot be written the call is refused with `503 server.unavailable`:
there is no unaudited read of the whole tenant.

## Configuration

`[services.journal_access]` in the site's common settings; unknown keys are refused:

```toml
[services.journal_access]
enabled = true              # false leaves the surface off
max_grant_days = 30         # 1 to 365
max_concurrent_streams = 4
daily_byte_budget = 68719476736
policy = "remote-sync"      # the name journal_info_get shows
region = "eu"
```

The table `spot_journal_grant` is created by `spot migrate`. The service is required once the
journal is hosted: the site refuses to start with the table missing and says to run
`spot migrate`. Without a hosted journal the three operations answer `501 service.not_configured`.
