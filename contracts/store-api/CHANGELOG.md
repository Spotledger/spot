# Store API contract changelog

Every change is an ADR plus a semantic version bump (blueprint §4, contracts-first rule),
following the same rule `contracts/native-api/CHANGELOG.md` uses. Additive changes bump the
minor version and keep the `/store/v1` prefix; breaking changes need a new major version and
prefix (see `openapi.yaml`, "Versioning").

## 0.2.1 (draft, awaiting architecture-owner approval; WP-STR-03.T6, lead decisions of 12 Nov 2026)

Reconciles the contract with `spot-bundle-signing` as WP-STR-03.T5 delivered it
(`FreshRevocationList`), so the store server (T3) and every site verify one form. **This is a
public contract change.** `VERSION` moves to 0.2.1 only because 0.2.0 is already on `dev` and gate
G6 needs a higher number; the contract has not shipped, so the revocation list text of 0.2.0 was
never served, and nothing is renamed or removed: no operation, error code or schema name changes,
and the one list a store could have served before is replaced by its corrected form before any
server exists.
- revocation list V2: the signed bytes use the domain `SPOT-REVOCATION-LIST-V2\n` (not `V1`),
  with `next_update` inside the signed body, times as RFC 3339 `Z` strings on the wire and integer
  UTC seconds in the signed bytes, a bundle entry's `digest`. The "open follow-up, known wire
  divergences" paragraph is deleted: the crate and the contract now agree, and the golden vector
  of `tests/fresh.rs` (bytes and signature) is quoted as the worked example and used as the
  example response;
- pinned limits (as the crate enforces them): `generation` 0 to 2^53; `reason` at most 2000
  bytes; `publisher_id` `^[A-Za-z0-9_-]{1,128}$`; digest 64 lower-case hex; `issued_by` and every
  revoked `key_id` a 32-digit lower-case hex key id (new schema `KeyId`; the example `root-2026`
  and `maxLength: 256` are gone); `signature` the base64 of 64 bytes; `next_update` after
  `issued_at`, at most 7 days later. Recommended lifetime 24 hours, reissue every 6 hours;
  `ETag` stays `"<generation>"` and `Cache-Control` `max-age=300`;
- the wire object is exactly what `to_wire_json` emits, with no envelope of its own: the
  `{data: ...}` wrapper stays the serving layer's;
- key custody reworded: the store holds no key that can sign a bundle or certify a publisher; it
  holds one revocation-issuing key behind a port (hardware allowed), whose public key is a root
  sites pin, used for revocation lists only (and licences in WP-STR-08);
- recorded: approval, rejection and withdrawal of a listing are operator routes outside this
  contract, and an unlisted version is answered as unknown to everyone but its owning publisher.
- Compatible for clients: a client that ignores unknown fields and verifies per the text above
  needs no change other than the domain string, which was never usable (the crate had no
  `next_update`).

## 0.2.0 (draft, pre-freeze revision; lead decisions of 8 Nov 2026 on PR 2924)

The contract has not shipped, so this stays a 0.x revision of the same draft. Four changes:

- revocation freshness: `RevocationList` gains `next_update` (RFC 3339, in the signed body)
  next to `issued_at`; `revocations_get` documents `Cache-Control: max-age=300` and the
  normative fail-closed rule: a client refuses NEW installs and updates when its newest list
  is past `next_update`, while already-installed bundles keep running and are re-checked
  against the next fresh list. The signing crate (`spot-bundle-signing::RevocationBody`) must
  gain the field before the contract ships (follow-up);
- yank: error code `store.yanked` renamed `store.already_yanked` (it is only the 409 for a
  second yank). Normative text: a yanked bundle stays downloadable by digest for existing
  installs, is not newly resolvable as installable (search omits it, listings mark it
  yanked), there is no un-yank, takedowns go through the revocation list;
- upload limits stated: bundle 64 MiB (`spot-bundle` `Limits::max_compressed_bytes`), manifest
  64 KiB, app name 63 bytes (`AppName`). New codes `store.too_large` (413, oversize bundle) and
  `store.invalid_manifest` (422, oversize, unparsable, invalid or path-mismatched manifest);
  a manifest-name mismatch moves from `store.bundle_invalid` to `store.invalid_manifest`.
  Sixteen error codes in all;
- the placeholder host `store.spotledger.example` and the boolean entitlement are kept until
  WP-STR-08, now stated in the file header.

## 0.1.0 (draft, awaiting architecture-owner approval)

First version (WP-CON-03.T55, lead lane; ADR-21, the app-store decision in
`docs/blueprint/platform.md` section WS11; contract house conventions carried over from
ADR-055/ADR-057). This is the public surface for publishers, sites and the review desk that
`spot-registry` (WP-STR-03.T1) and `spot-registry-pg` (WP-STR-03.T2) had no interface for:

- publish: `POST /store/v1/apps/{name}/versions` (`app_publish`), a bundle plus its detached
  signature, answers `202` with a `review_id` — the review (`spot-app-review`, WP-STR-05)
  runs asynchronously and a human still decides whether to list the app;
- discovery: `GET /store/v1/apps` (`app_search`, public listings only), `GET
  /store/v1/apps/{name}` (`app_get`) and `GET /store/v1/apps/{name}/versions/{version}`
  (`app_version_get`), each carrying the digest, the signature id and the `yanked` flag;
- distribution: `GET /store/v1/apps/{name}/versions/{version}/bundle` (`app_bundle_download`),
  `ETag` the content digest, RFC 9110 range support; the contract states plainly that a site
  never trusts this response and verifies the signature and the digest itself (WP-STR-02)
  before writing anything;
- `POST /store/v1/apps/{name}/versions/{version}/yank` (`app_version_yank`): the owning
  publisher marks a version unavailable for new installs; existing installs and digest-pinned
  fetches are unaffected (`spot-registry`'s `Release::active`);
- `GET /store/v1/revocations` (`revocations_get`): the signed revocation list
  (`spot-bundle-signing::RevocationList`) sites poll, with an `ETag` for conditional requests;
- `GET /store/v1/reviews/{id}` (`review_get`): the publisher's own view of the review report
  summary. The verdict is one of `spot-app-review`'s own values (`looks_clean` at best); the
  contract never lets a response say an app is safe, and a human still approves listing;
- fourteen error codes under `store.*`, each with an `explanation`: `store.invalid_request`,
  `store.unauthorized`, `store.forbidden`, `store.not_entitled`, `store.publisher_revoked`,
  `store.app_not_found`, `store.version_not_found`, `store.review_not_found`,
  `store.name_taken`, `store.yanked`, `store.version_not_monotonic`, `store.signature_invalid`,
  `store.bundle_invalid` (references the `bundle.*` problem codes of the lower-level archive
  and manifest checks), `store.rate_limited`;
- authentication: a publisher API key bound to one publisher id (`publisherKey`) for publish,
  yank and the review desk; a site licence token (`siteLicence`) for downloading a paid app's
  bundle; reads of public metadata, search and the revocation list need no credential. Rate
  limits are per credential (per publisher key, per site licence, or per client address for
  anonymous reads);
- payments and purchases are explicitly out of scope (WP-STR-08): entitlement is a boolean the
  store checks, not a price or a transaction;
- the store is a separate deployable, not part of `spot serve` (decided by the lead on
  31 Oct 2026; stated in `openapi.yaml`'s `servers`);
- `check.py` rules 1 to 9 (with a self-test per rule, plus a deliberately broken-copy
  self-test): `openapi.yaml` is a valid OpenAPI 3.1 document; no schema uses `type: number`
  (ADR-12, money and quantities are never floats); every path is under `/store/v1`; every
  operation has an `operationId` and a `default` error response; the error registry is well
  formed and every registered code carries a non-empty `explanation`; every schema states
  `additionalProperties` explicitly; every example validates against its schema and uses a
  registered code; the download operation never lets the server claim trust it has not
  earned (its description carries the site-verifies-itself sentence, and no response schema
  or header of that operation names a "trusted"/"verified"/"safe" claim); the review
  operation's schema never offers a verdict value beyond `spot-app-review`'s own three.

**compatibility.** First version; nothing to be compatible with. A client reads
`version` from the `info.version` field or a future health probe once one exists for this
deployable (none is defined in 0.1.0).

Lead fixes on PR 2924 (review findings, still 0.2.0, no wire change except as noted):

- `info.license` is now `Apache-2.0` (identifier `Apache-2.0`; no `url`, OpenAPI 3.1 forbids `url` next to `identifier`), matching
  `contracts/native-api/openapi.yaml`; metadata-only, no behaviour change;
- existence-oracle rule made consistent: yank of an unknown or not-yours app or version, and
  `review_get` of an unknown or not-yours review, all answer 404 (`store.version_not_found`,
  `store.review_not_found`); `store.forbidden` is never used for ownership;
- `revocations_get` states how a client rebuilds the signed bytes and records three open wire
  divergences from `spot-bundle-signing` (`next_update` in the signed body, RFC 3339 versus
  `i64` seconds, `digest` versus `digest_hex`) as follow-up for the serving task;
- clarifications: `Idempotency-Replayed` header declared on the 202, `416` and `304` shapes,
  multipart encoding of `certificate_chain`, free-app download ignores a siteLicence token,
  reference to a non-existent version-list endpoint removed.
