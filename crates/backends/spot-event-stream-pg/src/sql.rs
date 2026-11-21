//! Every statement of the store, as constants. All values are bound parameters; nothing here
//! is built from caller input. Every statement filters on `site` first.

/// Insert or replace the consumer row. `$1` site, `$2` name, `$3` spec, `$4` partitions,
/// `$5` registered_at.
pub const UPSERT_CONSUMER: &str = "\
INSERT INTO spot_event_consumer (site, name, spec, partitions, registered_at) \
VALUES ($1, $2, $3, $4, $5) \
ON CONFLICT (site, name) DO UPDATE \
SET spec = EXCLUDED.spec, partitions = EXCLUDED.partitions, registered_at = EXCLUDED.registered_at";

/// Insert the consumer row only when absent; the affected-row count says whether this call
/// created it (`1`) or lost to an existing consumer (`0`). Same parameters as
/// [`UPSERT_CONSUMER`].
pub const INSERT_CONSUMER_IF_ABSENT: &str = "\
INSERT INTO spot_event_consumer (site, name, spec, partitions, registered_at) \
VALUES ($1, $2, $3, $4, $5) \
ON CONFLICT (site, name) DO NOTHING";

/// Drop partition rows at or above the new partition count. `$3` partitions.
pub const DELETE_EXTRA_PARTITIONS: &str = "\
DELETE FROM spot_event_partition WHERE site = $1 AND consumer = $2 AND part >= $3";

/// Insert or replace every partition row of a consumer from arrays. `$3` parts, `$4`
/// generations, `$5` seqs, `$6` parked, `$7` parked_since.
pub const UPSERT_PARTITIONS: &str = "\
INSERT INTO spot_event_partition (site, consumer, part, cp_generation, cp_seq, parked, parked_since) \
SELECT $1, $2, p.part, p.cp_generation, p.cp_seq, p.parked, p.parked_since \
FROM unnest($3::int4[], $4::int8[], $5::int8[], $6::bool[], $7::timestamptz[]) \
    AS p (part, cp_generation, cp_seq, parked, parked_since) \
ON CONFLICT (site, consumer, part) DO UPDATE \
SET cp_generation = EXCLUDED.cp_generation, cp_seq = EXCLUDED.cp_seq, \
    parked = EXCLUDED.parked, parked_since = EXCLUDED.parked_since";

/// One consumer with its partitions, in partition order. `$1` site, `$2` name.
pub const SELECT_ONE: &str = "\
SELECT c.name, c.spec, c.partitions, c.registered_at, \
       p.part, p.cp_generation, p.cp_seq, p.parked, p.parked_since \
FROM spot_event_consumer c \
LEFT JOIN spot_event_partition p ON p.site = c.site AND p.consumer = c.name \
WHERE c.site = $1 AND c.name = $2 \
ORDER BY p.part";

/// Every consumer of the site with its partitions, in name then partition order. `$1` site.
pub const SELECT_ALL: &str = "\
SELECT c.name, c.spec, c.partitions, c.registered_at, \
       p.part, p.cp_generation, p.cp_seq, p.parked, p.parked_since \
FROM spot_event_consumer c \
LEFT JOIN spot_event_partition p ON p.site = c.site AND p.consumer = c.name \
WHERE c.site = $1 \
ORDER BY c.name, p.part";

/// One checkpoint. `$1` site, `$2` name, `$3` part.
pub const SELECT_CHECKPOINT: &str = "\
SELECT cp_generation, cp_seq FROM spot_event_partition \
WHERE site = $1 AND consumer = $2 AND part = $3";

/// The compare-and-set. `$1` site, `$2` name, `$3` part, `$4` new generation, `$5` new seq,
/// `$6` expected generation, `$7` expected seq.
pub const ADVANCE: &str = "\
UPDATE spot_event_partition SET cp_generation = $4, cp_seq = $5 \
WHERE site = $1 AND consumer = $2 AND part = $3 AND cp_generation = $6 AND cp_seq = $7";

/// Move a checkpoint anywhere and clear parking. `$4` generation, `$5` seq.
pub const RESTART: &str = "\
UPDATE spot_event_partition \
SET cp_generation = $4, cp_seq = $5, parked = false, parked_since = NULL \
WHERE site = $1 AND consumer = $2 AND part = $3";

/// Park a partition, keeping the first parked-since. `$4` instant.
pub const PARK: &str = "\
UPDATE spot_event_partition \
SET parked_since = CASE WHEN parked THEN parked_since ELSE $4 END, parked = true \
WHERE site = $1 AND consumer = $2 AND part = $3";

/// Unpark a partition.
pub const UNPARK: &str = "\
UPDATE spot_event_partition SET parked = false, parked_since = NULL \
WHERE site = $1 AND consumer = $2 AND part = $3";

/// The partition count of a consumer, to tell a missing consumer from a missing partition.
/// `$1` site, `$2` name.
pub const SELECT_PARTITIONS: &str = "\
SELECT partitions FROM spot_event_consumer WHERE site = $1 AND name = $2";

/// Delete a consumer; its partitions go with it (cascade).
pub const DELETE_CONSUMER: &str = "\
DELETE FROM spot_event_consumer WHERE site = $1 AND name = $2";
