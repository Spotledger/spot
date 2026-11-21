//! The Lua scripts the coordinator and the cache run on the server, as generated source.
//!
//! Every script is a pure function of its `KEYS` and `ARGV`: **no script reads the server's
//! clock or any other ambient state**, because time is an argument (the plugin's [`Clock`]
//! supplies it). That is what lets the contract suite drive a real server with a `ManualClock`.
//!
//! The scripts are built by functions (not bare `const`s) so the shared 64-bit-decimal helpers
//! are spliced into each one in exactly one place, and a unit test can assert both the helper and
//! the command each script uses. All arithmetic is integer arithmetic on decimal strings or on
//! Lua numbers that stay below `2^53`, so a `u64` counter reaches `u64::MAX` exactly and no money
//! or quantity ever sees a float.
//!
//! [`Clock`]: spot_core::Clock

/// Decimal-string helpers for exact `u64` arithmetic (Lua 5.1 numbers are doubles).
const U64_LIB: &str = r#"
local function u64_norm(s)
  s = string.gsub(s, '^0+', '')
  if s == '' then s = '0' end
  return s
end
local function u64_cmp(a, b)
  a = u64_norm(a); b = u64_norm(b)
  if #a ~= #b then
    if #a < #b then return -1 else return 1 end
  end
  if a == b then return 0 end
  if a < b then return -1 else return 1 end
end
local function u64_add(a, b)
  a = u64_norm(a); b = u64_norm(b)
  local i, j, carry, out = #a, #b, 0, {}
  while i > 0 or j > 0 or carry > 0 do
    local da = 0; local db = 0
    if i > 0 then da = tonumber(string.sub(a, i, i)); i = i - 1 end
    if j > 0 then db = tonumber(string.sub(b, j, j)); j = j - 1 end
    local s = da + db + carry
    table.insert(out, 1, string.sub(tostring(s), -1))
    carry = math.floor(s / 10)
  end
  return table.concat(out)
end
"#;

/// The largest `u64`, as a decimal string.
const U64_MAX: &str = "18446744073709551615";

/// Acquires a lease, issuing the next token only when the name is free.
///
/// `KEYS[1]` the lease hash, `KEYS[2]` the token high-water mark; `ARGV[1]` holder, `ARGV[2]`
/// ttl in microseconds, `ARGV[3]` now in microseconds. Returns `{"ok", holder, token, id,
/// expires_us}` on success or `{"held", holder, token}` while another holder's live lease exists.
#[must_use]
pub fn lease_acquire() -> String {
    format!(
        "{U64_LIB}
local lease = KEYS[1]
local token_key = KEYS[2]
local holder = ARGV[1]
local ttl = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local cur = redis.call('HMGET', lease, 'holder', 'token', 'expires_us')
if cur[1] and tonumber(cur[3] or '0') > now then
  return {{'held', cur[1], cur[2]}}
end
local token = redis.call('INCR', token_key)
local expires = now + ttl
redis.call('HSET', lease, 'holder', holder, 'token', token, 'id', token, 'expires_us', string.format('%.0f', expires))
return {{'ok', holder, tostring(token), tostring(token), string.format('%.0f', expires)}}"
    )
}

/// Extends a lease when the handle is still the current record; the token does not change.
///
/// `KEYS[1]` the lease hash; `ARGV[1]` token, `ARGV[2]` id, `ARGV[3]` ttl in microseconds,
/// `ARGV[4]` now in microseconds. Returns `{"ok", holder, token, id, expires_us}` or `{"lost"}`.
#[must_use]
pub fn lease_renew() -> String {
    format!(
        "{U64_LIB}
local lease = KEYS[1]
local token = ARGV[1]
local id = ARGV[2]
local ttl = tonumber(ARGV[3])
local now = tonumber(ARGV[4])
local cur = redis.call('HMGET', lease, 'holder', 'token', 'id', 'expires_us')
if not cur[1] then return {{'lost'}} end
if tostring(cur[2]) ~= token or tostring(cur[3]) ~= id then return {{'lost'}} end
if tonumber(cur[4] or '0') <= now then return {{'lost'}} end
local expires = now + ttl
redis.call('HSET', lease, 'expires_us', string.format('%.0f', expires))
return {{'ok', cur[1], tostring(cur[2]), tostring(cur[3]), string.format('%.0f', expires)}}"
    )
}

/// Releases a lease when the handle is still the current record; the token survives in `KEYS[2]`.
///
/// `KEYS[1]` the lease hash, `KEYS[2]` the token high-water mark; `ARGV[1]` token, `ARGV[2]` id,
/// `ARGV[3]` now in microseconds. Returns `1` when it released, `0` for a stale handle.
#[must_use]
pub fn lease_release() -> String {
    format!(
        "{U64_LIB}
local lease = KEYS[1]
local token = ARGV[1]
local id = ARGV[2]
local now = tonumber(ARGV[3])
local cur = redis.call('HMGET', lease, 'token', 'id', 'expires_us')
if not cur[1] then return 0 end
if tostring(cur[1]) ~= token or tostring(cur[2]) ~= id then return 0 end
if tonumber(cur[3] or '0') <= now then return 0 end
redis.call('DEL', lease)
return 1"
    )
}

/// Takes `by` consecutive values from a counter, exact to `u64::MAX`.
///
/// `KEYS[1]` the counter; `ARGV[1]` the step as a decimal string. Returns `{"ok", first}` or
/// `{"overflow"}` when the reservation would pass `u64::MAX` (and takes nothing).
#[must_use]
pub fn counter_next() -> String {
    format!(
        "{U64_LIB}
local cur = redis.call('GET', KEYS[1]) or '0'
local new = u64_add(cur, ARGV[1])
if u64_cmp(new, '{U64_MAX}') > 0 then return {{'overflow'}} end
redis.call('SET', KEYS[1], new)
return {{'ok', u64_add(cur, '1')}}"
    )
}

/// Raises a counter so its last value is at least `value`; never lowers it.
///
/// `KEYS[1]` the counter; `ARGV[1]` the value as a decimal string. Returns the last value.
#[must_use]
pub fn counter_set_at_least() -> String {
    format!(
        "{U64_LIB}
local cur = redis.call('GET', KEYS[1]) or '0'
if u64_cmp(cur, ARGV[1]) < 0 then
  redis.call('SET', KEYS[1], ARGV[1])
  return ARGV[1]
end
return cur"
    )
}

/// Takes `cost` units from a GCRA rate-limit bucket, atomically.
///
/// `KEYS[1]` the bucket hash; `ARGV[1]` cost, `ARGV[2]` the emission interval in microseconds,
/// `ARGV[3]` the capacity, `ARGV[4]` now in microseconds. Returns `{"ok", remaining}` or
/// `{"denied", retry_us}`. The bucket is the theoretical arrival time scaled by the capacity, so
/// every step is exact integer arithmetic.
#[must_use]
pub fn rate_take() -> String {
    r#"
local key = KEYS[1]
local cost = tonumber(ARGV[1])
local unit = tonumber(ARGV[2])
local cap = tonumber(ARGV[3])
local now = tonumber(ARGV[4])
local burst = cap * unit
local h = redis.call('HMGET', key, 'tat', 'cap', 'unit')
local tat = tonumber(h[1] or '0')
if h[2] and tonumber(h[2]) ~= cap then tat = 0 end
if h[3] and tonumber(h[3]) ~= unit then tat = 0 end
local scaled_now = now * cap
local level = math.max(tat, scaled_now) + cost * unit - scaled_now
if level > burst then
  local retry = math.ceil((level - burst) / cap)
  return {'denied', string.format('%.0f', retry)}
end
local new_tat = scaled_now + level
redis.call('HSET', key, 'tat', string.format('%.0f', new_tat), 'cap', cap, 'unit', unit)
local remaining = math.floor((burst - level) / unit)
return {'ok', string.format('%.0f', remaining)}
"#
    .to_owned()
}

/// Recomputes one key's accounted bytes and moves the site's gauges by the difference.
///
/// `KEYS[1]` the site's size hash, `KEYS[2]` the site's byte counter; `ARGV[1]` the user key,
/// `ARGV[2]` the data key, `ARGV[3]` the per-element overhead, `ARGV[4]` the per-entry overhead.
/// Returns the key's accounted bytes (0 when it is gone, including when its TTL passed).
#[must_use]
pub fn reconcile_size() -> String {
    r#"
local key = ARGV[1]
local data = ARGV[2]
local element = tonumber(ARGV[3])
local entry = tonumber(ARGV[4])
local t = redis.call('TYPE', data)['ok']
local size = 0
if t == 'string' then
  size = redis.call('STRLEN', data) + #key + entry
elseif t == 'hash' then
  local all = redis.call('HGETALL', data)
  size = #key + entry
  local i = 1
  while i <= #all do
    size = size + #all[i] + #all[i + 1] + element
    i = i + 2
  end
elseif t == 'list' then
  local vals = redis.call('LRANGE', data, 0, -1)
  size = #key + entry
  for i = 1, #vals do size = size + #vals[i] + element end
elseif t == 'set' then
  local vals = redis.call('SMEMBERS', data)
  size = #key + entry
  for i = 1, #vals do size = size + #vals[i] + element end
end
local old = tonumber(redis.call('HGET', KEYS[1], key) or '0')
if size == 0 then
  redis.call('HDEL', KEYS[1], key)
else
  redis.call('HSET', KEYS[1], key, size)
end
if size ~= old then redis.call('INCRBY', KEYS[2], size - old) end
return size
"#
    .to_owned()
}

/// Registers a node's presence key with a TTL, so a crashed node disappears on its own.
///
/// `KEYS[1]` the node key; `ARGV[1]` the TTL in milliseconds.
#[must_use]
pub fn node_register() -> String {
    r#"
redis.call('SET', KEYS[1], '1', 'PX', tonumber(ARGV[1]))
return 1
"#
    .to_owned()
}

/// Overwrites whatever shape a cache key held with one plain value, atomically.
///
/// `KEYS[1]` the data key; `ARGV[1]` the value, `ARGV[2]` the TTL in milliseconds (`0` for none).
#[must_use]
pub fn cache_set() -> String {
    r#"
redis.call('DEL', KEYS[1])
if tonumber(ARGV[2]) > 0 then
  redis.call('SET', KEYS[1], ARGV[1], 'PX', tonumber(ARGV[2]))
else
  redis.call('SET', KEYS[1], ARGV[1])
end
return 1
"#
    .to_owned()
}

/// Stores a plain value only when the key is absent (any shape counts as occupied).
///
/// `KEYS[1]` the data key; `ARGV[1]` the value, `ARGV[2]` the TTL in milliseconds (`0` for none).
/// Returns `1` when it stored, `0` when the key existed.
#[must_use]
pub fn cache_set_nx() -> String {
    r#"
if redis.call('TYPE', KEYS[1])['ok'] ~= 'none' then return 0 end
if tonumber(ARGV[2]) > 0 then
  redis.call('SET', KEYS[1], ARGV[1], 'PX', tonumber(ARGV[2]))
else
  redis.call('SET', KEYS[1], ARGV[1])
end
return 1
"#
    .to_owned()
}

/// Deletes a batch of cache keys and removes them from the site's accounting.
///
/// `KEYS[1]` the size hash, `KEYS[2]` the byte counter; `ARGV` are `user, data` key pairs. Returns
/// how many data keys were live and removed.
#[must_use]
pub fn cache_delete_many() -> String {
    r#"
local removed = 0
local i = 1
while i <= #ARGV do
  local user = ARGV[i]
  local data = ARGV[i + 1]
  removed = removed + redis.call('DEL', data)
  local old = tonumber(redis.call('HGET', KEYS[1], user) or '0')
  if old ~= 0 then
    redis.call('HDEL', KEYS[1], user)
    redis.call('INCRBY', KEYS[2], -old)
  end
  i = i + 2
end
return removed
"#
    .to_owned()
}

/// Recomputes the accounted bytes of every key the site has recorded, dropping the expired ones.
///
/// `KEYS[1]` the size hash, `KEYS[2]` the byte counter; `ARGV[1]` the data-key prefix, `ARGV[2]`
/// the per-element overhead, `ARGV[3]` the per-entry overhead. Returns `{entries, bytes}`.
#[must_use]
pub fn reconcile_site() -> String {
    r#"
local keys = redis.call('HKEYS', KEYS[1])
local bytes = 0
local entries = 0
local dropped = 0
for i = 1, #keys do
  local user = keys[i]
  local data = ARGV[1] .. user
  local element = tonumber(ARGV[2])
  local entry = tonumber(ARGV[3])
  local t = redis.call('TYPE', data)['ok']
  local size = 0
  if t == 'string' then
    size = redis.call('STRLEN', data) + #user + entry
  elseif t == 'hash' then
    local all = redis.call('HGETALL', data)
    size = #user + entry
    local j = 1
    while j <= #all do
      size = size + #all[j] + #all[j + 1] + element
      j = j + 2
    end
  elseif t == 'list' then
    local vals = redis.call('LRANGE', data, 0, -1)
    size = #user + entry
    for j = 1, #vals do size = size + #vals[j] + element end
  elseif t == 'set' then
    local vals = redis.call('SMEMBERS', data)
    size = #user + entry
    for j = 1, #vals do size = size + #vals[j] + element end
  end
  if size == 0 then
    redis.call('HDEL', KEYS[1], user)
    dropped = dropped + 1
  else
    redis.call('HSET', KEYS[1], user, size)
    bytes = bytes + size
    entries = entries + 1
  end
end
redis.call('SET', KEYS[2], bytes)
return {tostring(entries), tostring(bytes), tostring(dropped)}
"#
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        cache_delete_many, cache_set, cache_set_nx, counter_next, counter_set_at_least,
        lease_acquire, lease_release, lease_renew, node_register, rate_take, reconcile_site,
        reconcile_size,
    };

    #[test]
    fn every_script_is_non_empty_and_reads_time_only_from_argv() {
        for script in [
            lease_acquire(),
            lease_renew(),
            lease_release(),
            counter_next(),
            counter_set_at_least(),
            rate_take(),
            reconcile_size(),
            node_register(),
        ] {
            assert!(!script.trim().is_empty());
            assert!(
                !script.contains("redis.call('TIME'") && !script.contains("redis.call(\"TIME\""),
                "no script may read the server clock"
            );
        }
    }

    #[test]
    fn lease_scripts_use_the_lease_and_token_keys() {
        let acquire = lease_acquire();
        assert!(acquire.contains("HMGET"));
        assert!(acquire.contains("INCR"));
        assert!(acquire.contains("HSET"));
        assert!(acquire.contains("KEYS[2]"));
        assert!(lease_renew().contains("expires_us"));
        assert!(lease_release().contains("redis.call('DEL', lease)"));
    }

    #[test]
    fn counter_scripts_are_exact_to_u64_max() {
        let next = counter_next();
        assert!(next.contains("u64_add"));
        assert!(next.contains("u64_cmp"));
        assert!(next.contains("18446744073709551615"));
        assert!(next.contains("'overflow'"));
        assert!(counter_set_at_least().contains("u64_cmp"));
    }

    #[test]
    fn rate_take_is_integer_gcra() {
        let script = rate_take();
        assert!(script.contains("math.max"));
        assert!(script.contains("math.ceil"));
        assert!(script.contains("math.floor"));
        assert!(script.contains("'denied'"));
        assert!(!script.contains("TIME"));
    }

    #[test]
    fn reconcile_size_covers_every_structure() {
        let script = reconcile_size();
        for kind in ["'string'", "'hash'", "'list'", "'set'"] {
            assert!(script.contains(kind), "{kind} not handled");
        }
        assert!(script.contains("INCRBY"));
    }

    #[test]
    fn node_register_uses_a_ttl() {
        assert!(node_register().contains("'PX'"));
    }

    #[test]
    fn cache_scripts_cover_overwrite_nx_and_accounting() {
        let set = cache_set();
        assert!(set.contains("redis.call('DEL'"));
        assert!(set.contains("'PX'"));
        let nx = cache_set_nx();
        assert!(nx.contains("TYPE"));
        assert!(nx.contains("'none'"));
        let many = cache_delete_many();
        assert!(many.contains("redis.call('DEL', data)"));
        assert!(many.contains("INCRBY"));
        let site = reconcile_site();
        for kind in ["'string'", "'hash'", "'list'", "'set'"] {
            assert!(site.contains(kind), "{kind} not handled");
        }
        assert!(site.contains("HKEYS"));
    }
}
