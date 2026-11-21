Part of [Running Spot](../running-spot.md).

# The fix pipeline: the one path by which an agent changes a site

Every agent action above a reversible T1 setting changes a site only through the fix pipeline:
**plan, rehearse on a snapshot, apply, verify, and roll back automatically**, with a change journal
and an undo window in PostgreSQL and the side-effect hold on while a change is unverified. The engine
is `spot-fix-pipeline` (journal in `spot-fix-pipeline-pg`); this module hosts it and installs itself as
the apply path of the remedy host ([remedies.md](remedies.md)).
Code: `crates/framework/spot-cli/src/services/fix_pipeline.rs` and `services/fix_pipeline/`.

The service is **optional**. It needs the remedy host, a **world** to run remedies on, the **effect
hold** (the `spot-effect-hold` service; `require_effect_hold = false` waives it) and the PostgreSQL
journal. When one is missing the module is present but degraded, readiness names it
(`fix_pipeline_host.world_unavailable`, `.hold_unavailable`, `.remedies_unavailable`), and it does
**not** install itself: the remedy host keeps refusing T2 and T3 applies.

## Settings

```toml
[services.fix_pipeline]
enabled = true               # default true
undo_window_secs = 86400     # how long a verified fix stays undoable; 60 to 2592000
require_effect_hold = true   # never apply without the hold
deployment_epoch = 1         # carried by every audit record
maintenance_seconds = 60     # recovery and undo-window sweep; 10 to 3600
```

Unknown keys and out-of-range numbers stop the site from starting. The PostgreSQL pool needs at least
3 connections (the journal reaches the database through the shared blocking bridge).

## One run

1. The remedy's plan is normalised into a sealed plan and a fix id is derived. **One run per site at a
   time**: a second is refused, not queued.
2. The run is journaled (`Proposed`, `Planned`) and the plan is **rehearsed on the rehearsal
   snapshot, never on the live site**. A failed rehearsal ends the fix with nothing live attempted.
3. A T3 plan needs approval: a registered deep agent may self-approve (policy), nobody else can here
   (no approval inbox is hosted by this module).
4. The apply journals `ApplyStarted`, takes a snapshot for a T3 plan (a provider must be installed:
   without one a T3 fix fails here, before any change), captures the pre-images (state fingerprints
   of every affected target) and the pre-state fingerprint, **engages the effect hold**, then applies
   the remedy live and journals every change.
5. **Verification** re-reads every changed target and runs the site's checks (invariants and the
   symptom re-check). A failure runs the remedy's undo, re-reads the targets and compares the
   post-rollback fingerprint with the pre-state: only an empty difference is `rolled_back`; anything
   else is `failed`, never a claim that the site is back.
6. The hold is **released** for a verified fix (held effects replay in order with their original keys)
   and **discarded** for any other end. If it cannot be settled the effects stay held, the site is
   degraded and says so.

The journal (`spot_control.spot_fix_journal`) is append-only: the application role can only insert and
select, and a trigger refuses update, delete and truncate for every role. Every transition is also on
the AI audit chain (`fix.*` records, actor `agent:<id>`), and a refusing chain stops the run.

## Undo and recovery

A verified fix stays undoable for `undo_window_secs`. Undo checks that nothing changed the targets
since, journals the intent, runs the remedy's undo, proves the state reads back as it was, and is
audited. After the window it is refused.

The sweep (at start, as the readiness check, and every `maintenance_seconds`) recovers fixes the
journal shows unfinished and closes the undo window of verified fixes whose time is up. Limit: the
remedy framework keeps its undo log in memory, so a fix that was *applying* when the process ended
cannot be reversed by its remedy; it ends `failed` with evidence and readiness says how many fixes
could not be recovered.

## Codes

`fix_pipeline_host.settings_invalid`, `.remedies_unavailable`, `.world_unavailable`,
`.hold_unavailable`, `.recovery_failed`; the engine's own `fix_pipeline.*` codes and the remedy
framework's `remedies.*` codes pass through unchanged.
