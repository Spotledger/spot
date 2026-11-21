Part of [Running Spot](../running-spot.md).

# Remedies: the catalogue an agent repairs a site with

An agent never improvises a data change. T2 repair is done by **remedies** from a catalogue: each is
a typed operation with a precondition, a mandatory dry-run, an undo and its own self-test
(`spot-remedies`). This module hosts the catalogue on a site and puts the controls around it.
Code: `crates/framework/spot-cli/src/services/remedies.rs` and `services/remedies/`.

The service is **optional**: if it cannot run, the site serves and readiness says
`degraded: remedies (...)`. It needs the **agent tier service** (the source of an agent's tier);
without it the remedy host stays closed (`remedies_host.tiers_unavailable`) rather than run with no
tier check.

## Settings

```toml
[services.remedies]
enabled = true              # default true
undo_window_secs = 86400    # how long an undo stays possible; 60 to 2592000 (30 days)
self_test_at_start = true   # run every remedy's own scenario when the site starts
```

Unknown keys and out-of-range numbers stop the site from starting. A catalogue that breaks a
registry rule (a duplicate id, an irreversible remedy above T3) does too.

## The operations

| Operation | What it does |
|---|---|
| list | The remedies the agent's tier, domain and status allow. An agent registered T1 sees T1 remedies; T3 remedies are never in a T2 listing; a suspended or retired agent, or one of another domain, sees none. |
| dry-run | Authorises, validates the parameters against the remedy's schema, checks the precondition and returns a sealed **plan**. Nothing changes. Records of the plan are on the audit chain; if they cannot be written the plan is withheld. |
| apply | Takes a plan this host's dry-run issued (one dry-run authorises one apply). Writes an **intent record** to the audit chain *before* anything changes; if that fails nothing runs. A reversible T1 remedy (a setting) applies directly; every other remedy goes through the **fix pipeline**. Until the pipeline is installed it is refused with `remedies_host.apply_closed`, and that refusal is audited. |
| undo | Reverses an apply by its token inside the undo window. Refused (and audited) for an unknown token, an expired window, a later change that would be overwritten, and for an irreversible remedy (which must be T3). |

The tier comes from stored registrations, never from a header or an argument. While
`services.agent_tiers.unattended_policy_open` is false every agent is T0, so no remedy runs: open
that gate first (see [agent-tiers.md](agent-tiers.md)).

A call about another tenant, or from an id that is not a registered agent of this site, is refused
and audited; the audit record never holds a parameter value.

## Audit

Every step is a record on the site's AI audit chain (scope `mcp-<site>`): `remedy.authorised`,
`remedy.refused`, `remedy.planned`, `remedy.apply_started` (the intent), `remedy.applied`,
`remedy.undo_started`, `remedy.undone`, `remedy.undo_refused`, `remedy.apply_closed`,
`remedy.undo_store_unavailable` and `remedy.call_refused`. Each names the remedy, the agent (`agent:<id>`) and a plain sentence.

## The world a remedy runs on

A remedy changes the site only through typed ports (jobs, cache, retrieval index, edge sync,
quarantine, BPM cases, documents). The implementation of those ports is provided by the modules
that own those subsystems (`WorldAccess`); a copy of the site (`Rehearsal`) is what the fix pipeline
rehearses on. Until a world is provided the host can list remedies and prove their self-tests but
a dry-run, apply or undo answers `remedies_host.no_world`.

## Self-tests

Every remedy ships the scenario that proves it: the precondition, the dry-run, the apply, the
invariants after apply, and the undo (or the irreversible refusal). They run on the in-memory
rehearsal world at start (a failing remedy degrades the site and is named in readiness) and in CI
(`cargo test -p spot-cli --test remedies_service`).

## Undo records

An undo token must still work after a restart, so the host keeps every undo record in the site's own
database, in the table `spot_remedy_undo` (created at start; one row per token: `site`, `remedy`,
`agent`, `applied_at`, `expires_at` and the record itself as `jsonb`). The record is what the
framework already holds for undo: the token, the plan hash, the instants and, per typed port, the
target with its **before and after fingerprints**. It holds no cell value of a business table.

- **Written first.** The row is written the moment an apply succeeds, before the answer returns. If it
  cannot be written the host **undoes the change at once**, appends `remedy.undo_store_unavailable` to
  the audit chain and answers `remedies_host.undo_store_unavailable`: a remedy is never left standing
  with no way back. (A remedy that is irreversible by design has no way back to protect; for it the
  gap is only audited.)
- **Restored at start.** The service's readiness check puts every unexpired row back into the live
  registry. Rows past their window are not restored; a sweep (every five minutes) deletes them.
  An undo removes its row.
- **Degraded.** A row that cannot be restored (another site's row, a row that does not parse, a
  record the registry refuses) is **kept** and counted:
  `degraded: remedies (remedies_host.undo_restore_failed: n undo records could not be restored)`.
  The site keeps serving and every other token works. Look at the rows (`SELECT token, site, agent,
  expires_at FROM spot_remedy_undo`), decide, and delete a row only after the change it belongs to was
  reviewed. A site with no database falls back to a memory store: tokens then die with the process.
- **How long.** A row lives as long as the undo window (`undo_window_secs`, one day by default) and
  is swept after it. Rehearsal runs on the fix pipeline's snapshot never write a row.

## The remedy MCP tools

The service installs four tools on the MCP service's `McpHandle` (contract `mcp.md`, section 5,
"Extension tools") when the site is built, again each time its check is healthy, and takes them off at
shutdown and while the check fails: `remedy_list`, `remedy_dry_run`, `remedy_apply` and `remedy_undo`.
They are listed only for a registered agent; the two write tools only when the agent's tier allows an
apply (T1 or above). The tenant is the site and the agent is the authenticated principal, never an
argument. `remedy_apply` takes the plan `remedy_dry_run` returned, unmodified, and answers the changes,
the `undo_token`, `undo_expires_at` (Unix seconds, never later than the real deadline) and `route`
(`direct` for a reversible T1 remedy, `pipeline` for any other). A refusal carries the code of the
framework or the host (`remedies.*`, `remedies_host.*`) and a fixed sentence that never quotes a remedy
id, a parameter or a token. Every apply and undo is still audited by the host before it runs.

## Limits

The registry's record of which plans a dry-run issued is still in memory: after a restart a plan
must be dry-run again before it is applied. Only undo tokens survive.

## Codes

`remedies_host.settings_invalid`, `remedies_host.tiers_unavailable`,
`remedies_host.self_test_failed`, `remedies_host.wrong_tenant`, `remedies_host.not_an_agent`,
`remedies_host.no_world`, `remedies_host.world_unavailable`, `remedies_host.apply_closed`,
`remedies_host.audit_unavailable`, `remedies_host.internal`,
`remedies_host.undo_store_unavailable`, `remedies_host.undo_restore_failed`; the framework's own `remedies.*`
codes pass through unchanged.
