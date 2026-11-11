# `spot audit verify`

Checks the hash chain of a site's identity audit trail (sign-in, lockout, password and key-rejection records). It only reads: there is no `--apply`, and it needs nothing beyond the site's own database connection.

```
spot audit verify --site <SITE> [--from <SEQ>]
```

The chain checked is always the identity chain of the named site. The scope is derived from the site name, so the command cannot be pointed at another site's chain or at the document trail.

## What it checks

- The hash of every entry matches its stored body.
- Every entry links to the one before it.
- The sequence numbers are continuous (no gap, no repeat, no reorder).

`--from SEQ` verifies the entries from `SEQ` on (1 or more; `--from 0` is a usage error), anchored on the entry before `SEQ`. Use it to check only what is new since a previous run.

## What it does not check

- **Checkpoint signatures.** Checkpoints stay unsigned until WP-SEC-03; this command does not verify any.
- **A deleted tail.** Removing the newest entries leaves a shorter chain that is still internally consistent. It is detected only against a head you recorded earlier: save the `head` of each run (the number in `head <seq>: ok`) and compare it with the next run's. A head lower than the recorded one means entries were deleted from the tail.

## Output

On success, one line:

```
identity chain: 12 entries checked, head 12: ok
```

An empty chain verifies (`0 entries checked, head 0: ok`). On a break:

```
identity chain: BROKEN
seq 4: audit.hash_mismatch
...
12 entries checked, head 12, first broken sequence 4
```

One line per failure, in sequence order, with the stable failure code. Output never contains a record's content.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | The chain verified. |
| 1 | Usage error (missing `--site`, `--from` that is not a whole number of 1 or more). |
| 2 | The chain has a break. The operator must act. |
| 3 | The verification could not run (no such site, no usable PostgreSQL backend, an audit store that has not been prepared, a read error, a `--from` past the chain head, which checks nothing). It is not a pass. |

## Scheduling

Run it from cron or a systemd timer and alert on any non-zero exit; treat 3 as an alert too, since a check that did not run proves nothing.

```
17 3 * * *  spot --root /srv/spot audit verify --site main >> /var/log/spot-audit-verify.log || alert
```

To keep the recorded head, append the summary line to a file outside the database and compare the new head with the last one on each run.
