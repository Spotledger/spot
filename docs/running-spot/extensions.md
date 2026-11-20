Part of [Running Spot](../running-spot.md).

# PostgreSQL extensions

The product never installs an extension implicitly. A migration that needs one refuses and names
the operation `extension.install` (ADR-087 section 4.3); `spot extension` is how an operator runs
it for one site.

## The four allowed extensions

| Extension | Used for |
| --- | --- |
| `postgis` | geo fields and distance or box filters |
| `btree_gist` | exclusion constraints scoped by an equality column (bookings, holds) |
| `pg_trgm` | typo-tolerant search |
| `unaccent` | accent-insensitive search (always needed by a search projection) |

No other name is accepted; the command refuses it before it connects.

## The flow

1. `spot extension list --site S` shows each allowed extension, the installed version (`-` when
   absent), the platform minimum and what in the site's metadata needs it.
2. `spot extension plan --site S --name postgis` prints the plan (`extension`, `version`,
   `schema`, `already_present`, `privilege_held`, `plan_hash`) and changes nothing.
3. `spot extension install --site S --name postgis --plan-hash <hash from plan>` installs it. The
   hash is mandatory: there is no flag that skips it.
4. `spot audit verify --site S` checks the trail. Each call (plan and install, failures included)
   writes one event with the actor (`--actor`, default `cli:<user>`), the extension, the version,
   the result or failure code and the plan hash. The event goes into the site's hash chain, or into
   `sites/<site>/identity-audit.log` when the chain's tables are not prepared yet.

Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed.

## Refusals

| Code | Meaning and what to do |
| --- | --- |
| `migrate.extension_not_allowed` | The name is not one of the four. |
| `migrate.extension_privilege_missing` | The connecting role may not create it: run `spot extension install` once as a database superuser, or ask the hosting provider to enable the extension. |
| `migrate.extension_version` | The server offers an older version than the minimum: upgrade the package. |
| `migrate.extension_unavailable` | The server does not have the extension's files: install the operating-system package (for PostGIS see the desktop installer packaging, WP-PRD-02.T7). |
| `migrate.extension_plan_stale` | The database changed since `plan`: run `plan` again and use the new hash. |

Output never contains a connection string or password.
