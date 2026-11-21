Part of [Running Spot](../running-spot.md).

# SoD analysis and access reviews on a running site

`spot-sod` finds conflicts; `spot-access-review` holds the campaign state machine and its stores.
`crates/framework/spot-cli/src/services/access_review.rs` runs them. Level: **optional** (without the
review env and stores: degraded with `access_review_host.no_env`).

- **Analysis**: after every assignment change (`ReviewChangeSink`) and every `analyze_seconds`
  (default one day). Overrides come only from `SodStore::active_overrides(tenant, now)`. The conflicts
  become the read model; new unaccepted ones raise one alert document.
- **Campaigns**: opened from a spec (the env snapshots assignments into items, reviewers from the org
  data); every `tick_seconds` (default 300) each open campaign is ticked: escalation, auto-remove or
  leave open exactly as its deadline policy says. The store's tick is idempotent across nodes and restarts.
- **Decisions**: a reviewer decides only their own items (an escalated item only by its escalation
  target), never an item about themselves, never after the campaign is completed; the owner cannot
  decide others' items.
- **Removals**: a remove decision is recorded as pending, then applied through the governed revoke
  path (audit, session snapshot rebuild, realtime revalidation), and cleared only after the assignment
  is read back as gone. A failed revoke leaves "decided, not applied": listed by `pending_removals`
  and retried every tick, never lost.

```toml
[services.access_review]
enabled = true
analyze_seconds = 86400   # 60 to 604800
tick_seconds = 300        # 10 to 86400
```

The permission-engine call, the throttle of refusal records and the route-table guard are the host
guard of WP-SEC-02.T6.
