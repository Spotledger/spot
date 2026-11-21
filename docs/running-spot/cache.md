Part of [Running Spot](../running-spot.md).

# The step cache

`ctx.cache()` in a step (a hook or a method) is the site's cache: bounded, per site, lossy and
metered. It is the `spot-cache` `CacheStore` of the site, shared across nodes when the Redis plugin
is on. Nothing that must survive belongs in it.

## What it is

- **Lossy.** Any entry may be missing: it expired, it was evicted, the process restarted, Redis was
  down. A step must always be able to reload the value from real storage. A failed cache call is
  never an error to the step: `get` answers nothing and `set` and `delete` do nothing.
- **Per site.** The store is addressed by the site first on every call. A step reaches only the
  keys of its own site under the `step:` prefix. Two apps' steps of one site share that namespace
  (the `AppCache` interface has no app parameter), so name keys with your app's prefix.
- **Bounded.** Memory is accounted as key + value + a fixed overhead per entry. Over the site's
  quota the site evicts its own least recently used entries (sampled LRU); over the process budget
  the most saturated site gives up entries. A step that stores a million keys evicts its own
  oldest ones and never grows the process past the budget. A value too big for the site's quota is
  refused and counted.
- **Expiring.** A `set` without a time to live gets `default_ttl_seconds`; a zero ttl stores
  nothing. Expired entries are swept as the cache is written, not only when read.

## Settings

`[services.cache]` in `common.toml`. The module is always on and has no `enabled` key. Unknown keys
are refused; a zero value, or `site_max_bytes` above `max_bytes`, stops the site at start-up with a
message naming the key.

| Key | Default | Meaning |
| --- | --- | --- |
| `max_bytes` | 64 MiB | In-process budget for the whole process (ignored with the Redis plugin) |
| `site_max_bytes` | 32 MiB | One site's share of it |
| `max_value_bytes` | 1 MiB | Largest value a step may cache; also bounds a key to 512 bytes |
| `default_ttl_seconds` | 3600 | Time to live of an entry stored without one |

## What is evicted, and when

On every write the site's bytes are kept at or under `site_max_bytes` by evicting its own oldest
entries; when the process total is over `max_bytes` the most saturated site is trimmed first. Reads
do not evict. An explicit `delete` and an expiry are not evictions.

## Why a step must always reload

Eviction is silent by design. Code like `get -> miss -> compute -> set` is correct; code that
`set`s and later assumes `get` finds it is not. A `set` that is dropped (quota, a lost Redis, a
value over the limit) is counted but never reported to the step.

## The backend

- **In-process** (default): memory of this process. A value set on node A is a miss on node B.
- **Redis**: with `[services.cluster_redis] enabled = true` the plugin's store is the `CacheStore`
  port and the step cache uses it, so a value set on one node is read on another. A Redis outage
  turns `get` into a miss and `set` into a no-op (counted as `reason="error"`); on recovery the
  plugin clears the site's namespace so nothing cached before the outage is read.

## What an operator can ask of the store

The store offers, besides the per-key commands a step uses, three site-wide operations for
operators and remedies. All are per site and never touch another site:

- `delete_prefix(prefix)`: removes every key starting with the prefix and returns how many live
  keys it removed. The `step:` namespace flush of the remedies uses it (never the whole site).
- `count_prefix(prefix)`: the number of **live** keys starting with the prefix, exactly what
  `delete_prefix` with the same prefix would remove. An expired key that has not been swept yet is
  not counted. It only reads: it changes no entry, no recency, no hit or miss counter and evicts
  nothing, so asking is free of side effects. The prefix is matched literally (glob characters in
  it are ordinary characters); the empty prefix counts every key of the site. A string, a hash, a
  list and a set count once each. The in-process store counts under its state lock; the Redis
  store scans its own site's keys in bounded batches and stops after at most a million keys
  examined, so a site with an extreme number of matching keys may read a lower bound there. The
  remedies' cache part uses it to report the exact number of `step:` entries before and after a
  flush (3 step keys among 8 report 3, then 0, and the other 5 stay).
- `clear_site`: removes every entry of the site.

## Metrics

On `/metrics` when the metrics service is on, read from the store every 15 seconds:

- `spot_cache_hits_total`, `spot_cache_misses_total`, `spot_cache_evictions_total`,
  `spot_cache_expired_total` (counters);
- `spot_cache_entries`, `spot_cache_bytes` (gauges);
- `spot_cache_step_dropped_total{reason}`: step calls that did nothing, with `reason` one of
  `unfilled`, `key_size`, `value_size`, `quota`, `error`.

No key, value or app name is a label. Alerts worth setting: evictions per minute above a few
hundred sustained (the budget is too small or a step caches too much); `spot_cache_bytes` near
`site_max_bytes` for hours; any rate of `step_dropped_total{reason="error"}`.

## Troubleshooting

| Symptom | Cause | What to do |
| --- | --- | --- |
| A step sees a miss it did not expect | Expired, evicted, or set on another node without the Redis plugin | Reload from storage (the contract); check `evictions` and the ttl |
| `step_dropped_total{reason="quota"}` rising | A step caches values larger than the site's quota | Cache less, or raise `site_max_bytes` and `max_bytes` |
| Memory grew after raising the budget | The budget is a ceiling the cache now fills | Lower `max_bytes`; the process total is bounded by it |
| Redis is down | `reason="error"` drops, every `get` is a miss | Steps keep working from storage; restore Redis, the namespace is cleared on recovery |
| A value is never cached | It is over `max_value_bytes` (`reason="value_size"`) or its key over 512 bytes (`reason="key_size"`) | Shorten the key; raise `max_value_bytes` or cache a reference |
| `reason="unfilled"` at start | A step ran before the cache module registered | Only during start-up; persistent means the module did not register: check the log |
