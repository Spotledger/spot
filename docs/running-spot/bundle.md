Part of [Running Spot](../running-spot.md).

# Moving customisations between environments: `spot bundle`

A site's customisations (custom fields, property overrides, view layers, scripts, rules and
declared fixtures) travel between environments as a **signed hand-over file**. Nothing is applied
by the import itself: it produces a plan that goes through the migration lifecycle.

## Environments

```
spot bundle env link   --site S --kind dev|test|prod [--trust-anchor FILE --revocations FILE]
spot bundle env list
spot bundle env unlink --site S
```

The kind and the key policy are stored in `sites/S/environment.toml` and `sites/S/environment/`.
A kind never changes quietly: relinking to another kind is refused until the site is unlinked.
Transport goes one rung up only: dev to test, test to prod. Dev to prod is refused, and a bundle
from production is never imported anywhere (production data and customisations reach development
through a masked sandbox, `spot sandbox`).

The **key policy** of an environment is its trust anchor (`{"roots":[{"public_key":"<64 hex>",
"valid_from":0,"valid_to":null}]}`) and a signed revocation list. A site without a policy cannot
import.

## Export

```
spot bundle export --site S --out FILE --signing-key SEEDFILE --certificate CERT.json [--to KIND]
```

Writes `FILE` (never overwrites): one JSON header line, then the bundle archive. The signature
binds the source kind, the target kind, the bundle name, the creator and the content digest, so a
header edited to point at another environment no longer verifies. The seed file holds the Ed25519
seed as 64 hex characters; it is read, never printed.

## Diff

```
spot bundle diff A B [--base C]
```

Lists additions (`+`), changes (`~`) and removals (`-`) from A to B with their risk. With `--base`
(the common ancestor) it also lists the three-way conflicts. Hand-over files and raw archives are
both accepted.

## Import

```
spot bundle import FILE --site S [--apply]
```

In this order: the file is readable; it is signed; its digest matches; the signature verifies under
the target's key policy; the bundle is addressed to this environment and comes from the rung below;
every section passes its check (a script is re-transpiled; a failing file is named); then the plan,
the conflicts with the target's own customisations and the conflicts with the installed platform
version are computed and printed. Without `--apply` nothing is written. With `--apply` the
migration plan is created (`spot migrate plan` rules: stored by hash, approval, rehearsal), the
file is kept as `sites/S/bundle-pending/<hash>.handover`, and the command stops. Production always
goes through approval.

A refusal prints `refused (reason): ...` and exits 2. Reasons: `unreadable`, `unsigned`,
`digest_mismatch`, `signature_invalid`, `invalid_bundle`, `wrong_target`, `direction`,
`check_failed`, `plan_refused`, `no_key_policy`, `key_policy_invalid`, `not_linked`.

## What a live site exports today

The site image is the installed apps plus the applied custom field and property records of the
site's database. Views, scripts, rules and fixtures have no live store yet, so a live export
carries the `meta` section; imports and diffs handle every section.
