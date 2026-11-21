# Hosting the journal

How the journal shipper (WP-RTB-01.T3) works on a running server, what keeps the recoverable record true, and what to do when the chain is broken. The operator's configuration page is [running-spot/journal.md](running-spot/journal.md).

## The loop

One task per site, `spot_journal_ship::Shipper`, under the site's writer lease (a named lease with a fencing token in the site's coordinator). A step:

1. holds or renews the lease (a node that loses it forgets its relay and stops writing within one lease interval; the relay's own writer check is the second line of defence),
2. reads up to `batch_max_entries` outbox entries after what the relay already holds,
3. runs every line through the protect transform (personal fields sealed, `doc_state` images only),
4. appends the batch to the relay, then seals if the limits say so,
5. saves the watermark `(generation, seq)` in `spot_journal_ship_state` **only** to a position the relay reports durable: the chunk's last position under `remote-sync`, the last sealed segment otherwise.

A crash between any two of these converges. Before the append nothing was written. After the append and before the watermark write the relay holds entries the watermark does not cover: the shipper reads on from the relay's position, so each entry is offered once, and the watermark catches up at the next step. Under `local` and `replica-sync` an open segment is only in memory: a restart reopens the relay from the store and re-reads the buffered entries from the outbox, which the barrier below keeps.

## The outbox barrier

`feed::cleanup` prunes the outbox by age. When a shipper is configured it must never prune an entry above the persisted watermark, whatever the age, or a long journal outage plus retention would leave a silent gap in the recoverable record. The row in `spot_journal_ship_state` switches the barrier on; a database without the table, or without a row, prunes as before. The barrier is the lowest site-source watermark of the feed's current generation (a row of an older generation counts as 0). What it kept is reported by `Cleanup::held_back_events` and `held_back_seconds` (`feed.pruning_blocked_by_journal`): the outbox grows while the journal is unreachable, and `RPO_BREACH` says so.

## Generations

A restore starts a new outbox generation. The shipper opens a new relay generation with a fork record (`forked_from` = the last position shipped of the old generation, recorded before the relay is opened, so a restart reopens the very same record). The old generation stays in the journal forever. A journal that never held generation 1 cannot be forked from it: the shipper stops with `journal_ship.generation_unmatched` and the recovery path (reseal from the database) starts the journal.

## Verification

After every seal, and at least every `verify_every_seconds`, the shipper light-verifies the stored chain of its generation: the manifest signature and ownership, every listed segment present with the listed hash. It also verifies before it writes, once the interval has passed. A finding that is evidence of tampering or loss (anything except an unlisted segment, the ordinary crash between sealing and the manifest update) raises `JOURNAL_GAP`, stops the shipper and keeps the watermark where it was.

## Recovering from a broken chain

The shipper is stopped and readiness says `journal: stopped (<code>)`. It will not restart by itself.

1. Do not delete anything and do not create a manifest by hand. The segments and tail chunks under `<tenant>/` are the record.
2. Read the finding code (`journal_verify.*` or `journal_relay.chain_broken`) and the generation and range it names in the `JOURNAL_GAP` condition.
3. Compare the object with the replica or backup of the target. Restore the damaged or missing object under its exact key. Segments are immutable: a restored segment must be byte-identical (its hash is in the signed manifest).
4. If the manifest is missing while segments of the generation exist, restore the manifest from the replica; never create one.
5. Restart the site. The shipper reopens the relay (which resumes and repairs a crash between two steps), verifies, and continues from the watermark. If the outbox entries for a gap were already pruned (`journal_ship.outbox_gap`), restore the range from a database backup through the recovery path before restarting: the shipper never skips an entry.
6. `JOURNAL_GAP` clears on the first clean verification.

## Keys

The manifest signing key is derived from the site's key provider (`derive_key`, purpose `journal-manifest-sign`) at start and never stored; the tail-chunk keys wrap each batch's data key under the same provider. Rotating the provider's key gives a new key id; the verifying set holds the key for every generation of its id. A site without `[services.secrets]` cannot ship a journal.

## Not covered here

The acknowledgement gate (WP-RTB-01.T4), snapshots (T5), rebuild, audit, erasure keys (T8), an S3 or Azure provider, the access routes (WP-JRN-09.T4).
