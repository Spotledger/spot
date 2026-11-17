Part of [Running Spot](../running-spot.md).

# Assignment rules on a running site

`spot-iam-assign` plans; `crates/framework/spot-cli/src/services/assignment_rules.rs` runs. Level:
**optional** (no store port: degraded with `assignment_rules_host.no_store`).

- **Reconcile**: on a change of the business data a rule reads (the outbox subscriber calls
  `RuleChangeSink::changed(users)`) and every `reconcile_seconds` (default 300) for the whole tenant,
  which also catches drift and fires expiry. A rule that fails validation is refused and logged
  (`assignment_rules_host.rule_refused`); the others keep running.
- **One transaction**: the plan's changes, its events on the outbox and the expiry marker are one
  `RuleStore::apply` call. Expiry events fire exactly once: the marker is written with the events.
- **Same governed path as a manual grant**: every grant goes through the segregation-of-duties gate
  (`GrantCheck`). A blocked grant, or a gate that cannot answer, is reported
  (`assignment_rules_host.grant_blocked`) and not applied. With no gate port nothing is granted.
- **One node per tenant**: the coordinator lease (`Lease`). A node without it does nothing and the
  lease is checked again right before the write. No lease port means a single-node site.
- **Guard**: a plan that would end more than `bulk_hold_percent` (default 25) of one role's active
  assignments is held (`assignment_rules_host.plan_held`). Preview shows the plan and its digest;
  running with that digest confirms it.
- **Preview and run** (`RuleRunner`, used by `identity_assignment_rules_preview|run`): preview applies
  nothing; run applies exactly the plan whose digest preview returned, a stale digest is
  `422 identity.rules_invalid`. Answers carry counts and ids, never facts.

```toml
[services.assignment_rules]
enabled = true
reconcile_seconds = 300   # 10 to 86400
bulk_hold_percent = 25    # 1 to 100
```

Facts are read as the system principal and only the named assignment writes are performed.
