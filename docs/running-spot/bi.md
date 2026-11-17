Part of [Running Spot](../running-spot.md).

# `spot bi`: reporting views and a read-only login for BI tools

People point Power BI, Excel and similar tools straight at the database. Those tools do **not**
pass through Spot's permission engine, so what they can see is decided in one place: a curated,
versioned set of reporting views, and a login role that can read those views and nothing else.

```
spot bi plan     --site S
spot bi apply    --site S [--apply]
spot bi list     --site S
spot bi password --site S [--out FILE]
spot bi revoke   --site S
```

Exit codes: `0` done, `1` usage, `2` refused (nothing changed), `3` failed. Every change is
recorded in the site's identity audit chain, and an operation that cannot be audited does not run.

## What is exposed

Nothing, until the site has a policy: `sites/<site>/bi.toml`.

```toml
expose = ["sales_invoice", "party"]   # entities that get a view (documents only)

[entity.sales_invoice]
version = 1                           # view name v1_sales_invoice; a breaking change needs v2
posted_only = true                    # only posted rows (docstatus = 1); the default
children = true                       # child tables as sibling views
include = ["customer", "posting_date", "total"]   # the only columns shown
```

A column is shown only when **both** the policy allows it and the metadata does not close it. The
policy can never open what the metadata closes:

| Column | Shown? |
| --- | --- |
| a `secret` field, or a field tagged `secret` | never |
| a member of a protection group | never |
| a field tagged `pii` or `health` | never |
| an unclassified field | only when the entity's `include` lists it by name |
| a field the policy's `exclude` names | never |
| JSON, bytes, geo and range fields | never (no BI representation) |

Asking for a closed column by name is an error (`bi.protected_column`) that names it, so a mistake
is noticed rather than silently ignored. `spot bi plan` prints the views, their columns and types,
and **every column left out with the reason**; it is read-only and needs no database. Run it
before `apply`, and after any change to a doctype.

Views read the site's own base schema only. One database holds one tenant, and `apply` refuses a
plan in which a view would read from any other schema.

## Applying

`spot bi apply --site S` prints the plan; `--apply` makes it so:

1. creates schema `bi` and the views (typed: money and quantities as `numeric`, instants as
   `timestamptz`, never formatted strings) and `bi.catalogue`, one row per view column, so tools
   can discover them;
2. creates the login role `spot_bi_<site>_ro`, a member of the shared read-only role
   `spot_bi_reader`, with `SELECT` on schema `bi` only and **no access to any base table**; it is
   read-only by default (`default_transaction_read_only = on`), has a `statement_timeout` of 60
   seconds, an idle-in-transaction timeout and a connection limit. These are set on the login role
   itself, because PostgreSQL does not hand a group role's settings to its members;
3. checks, as the BI role, that it can read the views and cannot read a base table, write or
   change anything (`verified: N checks passed`).

Running it again changes nothing unless the catalogue or the policy changed (the views record a
fingerprint). A column added to an entity is added to its view; a column removed or retyped never
changes a published view: raise the entity's `version`, and the new view is created beside the old
one (`bi_v2` next to `bi_v1`) so existing reports keep working. (There is no command yet to retire an old version;
the library has one, and a follow-up can expose it.)

The role has **no password** until you set one, so it cannot sign in yet.

## Password

```
spot bi password --site S --out /root/bi-S.password
```

Sets a new password for the role and writes it, once, to the file: created with mode `0600`, and
refused if the file exists. The password is never written to a pipe: it is printed to the terminal
only when standard output is a terminal and `--out` is not given. A new password replaces the old
one. The statement that sets it carries the password to the server; if `log_statement` is `ddl` or
`all` it can reach the server log, so set `log_statement = none` for the administrative role or
keep that log private.

## Connecting

Require TLS from BI clients, and make the server enforce it with a `hostssl` line in
`pg_hba.conf` for the BI role (and no `host` line for it), for example:

```
hostssl  <database>  spot_bi_<site>_ro  <bi-network>  scram-sha-256
```

Point the tool at the `bi` schema. `bi.catalogue` lists every view and column with its type and
description.

A **read replica** for BI is the better place for heavy reports, so they never compete with
transactions. It is documented here and not provided by this command: create the replica, apply the
views on the primary (they replicate), and give the BI network the replica's address.

## Revoking

`spot bi revoke --site S` drops the login role. The views stay (they hold no secret). Reports that
still point at the role stop working at once.
