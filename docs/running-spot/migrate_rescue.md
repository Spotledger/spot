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
