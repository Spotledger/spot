Part of [Running Spot](../running-spot.md).

# Rescue: `spot migrate-rescue`

Use it when an apply failed (`spot migrate-apply` printed the next action) or verification found a
problem after the gate opened. The decision table:

| Situation | Do |
| --- | --- |
| The apply stopped at a step and the cause is fixed | `spot migrate-apply apply HASH --site S --resume` |
| Every applied step is reversible and nothing important was written since | `--mode down` (needs declared down-patches) |
| The new version is wrong and users already worked in it, or a step is irreversible | `--mode restore` |

Rescue can destroy data written since the snapshot, so it is two steps and two people.

```
spot migrate-apply maintenance enter --site S --reason "rescue"
spot migrate-hold hold --site S --reason "rescue"
spot migrate-rescue HASH --site S --mode restore                 # preview: snapshot, counted loss, TOKEN
spot migrate-rescue HASH --site S --mode restore --confirm TOKEN --approved-by SECOND_PERSON
```

* The preview changes nothing. It lists the **counted loss window** (rows per document type a
  restore replaces) and a token over the plan hash, the mode, the snapshot time and those counts. If
  more rows are written before you confirm, the token no longer matches and the rescue is refused.
* The rescue is refused unless: you hold `migration.run`; the plan is `failed` or `applied`; the
  pre-apply snapshot exists and is verified; the gate is closed and the effect hold is active; a
  second person, different from you, confirmed; the token matches.
* `restore` restores the snapshot over the live name. The current database is renamed aside to
  `<db>_replaced_<seconds>` and kept; it is never dropped, and its restrictions stay.
* Re-entry of what was written after the snapshot needs a journal service. Where none exists the
  report lists the loss window precisely (rows per document type) and nothing is re-entered.
* The plan becomes `superseded`. The gate **stays closed**: inspect the restored database, then
  `spot migrate-apply maintenance leave --site S`.
* `down` needs the declared down-patches of the plan; this host does not hold them, so it refuses
  and says to use `restore`.

Exit codes: 0 done or previewed, 1 usage, 2 refused (nothing changed), 3 failed.

## Resuming a rescue

A rescue writes a durable journal (PostgreSQL schema `spot_rescue_control`, table `spot_rescue_journal`, one row
per record, per site) before each step. If the process dies, the journal says exactly how far it got.

**Seeing an unfinished rescue.** The maintenance gate stays closed and the effect hold stays active,
and the journal's last records show a started run with neither `gate_opened` nor `halted`. The
verb reads the same journal: a second `spot migrate rescue` for the same plan **resumes** it and
reports `resumed: true` and the attempt number.

**What a resume repeats, and what it never repeats.** It never restores the snapshot again once the
journal says the restore completed, and it never presents an item that already has an outcome to the
old version again. It runs the items that have no outcome, runs the invariant check, and only then
opens the gate. A resume that finds a different snapshot than the unfinished run restored is
**refused** (`migration.rescue_refused`) before anything runs: two rescues never interleave on one site.

**While it is unfinished.** The closed gate means users are not admitted and writes are refused;
that is intended. Do not open the gate by hand: finish the rescue (repeat the same command with the
same confirm token, or re-run `spot migrate rescue --preview` to get a fresh one if the loss window
changed).

**The post-migration database is never deleted.** A restore keeps the database that was live when the
rescue began, renamed aside, so nothing is lost if the rescue is abandoned. Removing it is a separate,
explicit operator action.

The control schema is created by the first rescue of a site (the verb calls `prepare`, which is
idempotent and safe from several nodes at once); a creation-time call in `new-site` would be cleaner.
