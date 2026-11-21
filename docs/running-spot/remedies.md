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
`remedy.undo_started`, `remedy.undone`, `remedy.undo_refused`, `remedy.apply_closed` and
`remedy.call_refused`. Each names the remedy, the agent (`agent:<id>`) and a plain sentence.

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

## Limits

The undo log lives in the process: a restart forgets the open undo tokens (the audit chain keeps the
record of what was applied). Persisting them needs a restore hook in `spot-remedies`.

## Codes

`remedies_host.settings_invalid`, `remedies_host.tiers_unavailable`,
`remedies_host.self_test_failed`, `remedies_host.wrong_tenant`, `remedies_host.not_an_agent`,
`remedies_host.no_world`, `remedies_host.world_unavailable`, `remedies_host.apply_closed`,
`remedies_host.audit_unavailable`, `remedies_host.internal`; the framework's own `remedies.*`
codes pass through unchanged.
