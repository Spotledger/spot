Part of [Running Spot](../running-spot.md).

# Seeded system rows: what `spot migrate --apply` adds

A site created before a role was added to `apps/platform/bootstrap.toml` gains that role the next
time `spot migrate --apply` runs, the way it already gains new platform DocTypes and grants. The
seed runs **after** the schema step (and on the lifecycle path, after `spot migrate-apply` finished
its steps, never in the middle of a checkpoint), in one transaction: all of it or none of it.

## What it does

* Writes each system role (and any other non-identity row `bootstrap.toml` declares) whose fixed id
  is not in the database yet. Today that is the roles.
* Prints `seed: added role "NAME"` for each, or `seed: nothing to add`.
* A plan-only run (`spot migrate` without `--apply`) writes nothing. It prints `seed: would add role
  "NAME"`, or `seed: after the schema is applied` while the plan still has schema steps.

## What it never does

* Create the `Administrator`, a user, a role assignment, a password, a key or a session. Those exist
  only at site creation (`spot new-site`).
* Overwrite or update an existing row. An edited description, or a role the operator renamed, stays
  exactly as it is (rows are matched by their fixed id, not by name).
* Run on a `--bare` site (it has no platform app); that is skipped silently.
* Anything else is not turned off by a flag: the seed is always part of `--apply`.

A role an operator deleted on purpose is created again by the next `--apply`: the grants name the
roles, so a missing one would silently break them.

## Audit

Seeded system rows are reference data. They are written as the system principal through the
document store directly (as `seed` does at site creation), so they carry **no audit entry**, and
`Role.origin` is `reference`.

## When it fails

The command exits non-zero and says: the schema step is already applied, the seed was rolled back
and wrote nothing, and running `spot migrate --apply` again is safe (the plan is then empty and the
seed is retried). Typical cause: a row of another id already holds a seeded role's name.

## For app authors

Add a `[[roles]]` entry with a fixed UUIDv7 id to `apps/platform/bootstrap.toml`. Existing sites get
it on their next `spot migrate --apply`. Removing or renaming an entry later does not remove it from
sites that already have it.
