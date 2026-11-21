Part of [Running Spot](../running-spot.md).

# sync_policy

What syncs is a policy on the DocType in metadata (class, direction, owner scope, excluded
fields). Each metadata generation's policies are linted at load; a generation with any problem is
refused whole and the previous one keeps serving (`sync_policy: refused generation 15 (...)`).
A DocType with no policy is `local`: nothing of it leaves the node. An edge cannot change a
`master` record (`sync_policy.master_edge_write`). A policy that narrows scope applies to new
entries and does not retract what a node already holds.

## Hosting (`services/sync_policy.rs`, every role)

The module owns one policy host for the site. It loads the generation of the metadata the site
serves and loads again on every metadata swap; a generation that fails its lints is refused whole
and the previous one keeps serving. It provides two ports: the policy handle (what may leave, what an
edge may write, the verified generation for the hub's providers, a watch of the generation number)
and the readiness line.

```toml
[services.sync_policy]
default_preset = "local"          # local | master | record | handover
default_owner_scope = "warehouse"  # the key type a record or handover default is owned by

[services.sync_policy.declare.Customer]
sync_class = "master"
sync_direction = "down"

[[services.sync_policy.scope_field]]
doctype = "Invoice"
key = "warehouse"
field = "warehouse"
```

**Presets.** The four presets of ADR-063 read as the library's classes: `local` is `local`,
`master` is `master` (down), `record` is `transactional` (up), `handover` is `shared` (both ways).
Where the ADR and the library differ the library wins (it has no separate `handover` class).

**The default preset** applies to DocTypes nobody declared a policy for and that no installed app
owns (runtime-created ones). It is declared into the generation **before** the lints run, so a
default that cannot hold (a link to a `local` DocType, a `record` default with no owner scope) refuses
the generation, visibly, like any other policy problem.

**Excluded fields.** On the way down the hub strips them from every image before queueing (the
destination's redactor, built from the same generation); on the way up the node strips them from an
event payload before offering it, by name at every depth. A document received with an excluded name
is refused.

**The hub's destinations.** Each destination is built from its **hub-side** subscription and the
verified generation: it covers a record only when the policy sends that DocType down and the
subscription holds every scope key the record requires. A record whose scope value cannot be read is
not covered. When the generation changes the hub rebuilds its destinations and re-reads its
subscriptions (the policy handle's generation watch).

**Handover.** An arriving handover record gets the preset's `adopt` setting: `manual` (the default)
leaves it for a person at the hub, `auto` requests adoption at once, `never` leaves it owned by its
creating node.

**`spot sync policy show`** prints, read only and names only, the default preset, the declared
policies (class, direction, preset, owner scope, excluded field names) and the scope fields. The
verified generation, and why one was refused, are on the readiness line.
