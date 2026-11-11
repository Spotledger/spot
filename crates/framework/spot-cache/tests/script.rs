//! Table-driven replay: a short command script whose replies are the ones `redis-cli` gives,
//! written out by hand next to each command.

mod common;

use common::{bytes, cache, sid};
use spot_cache::{CacheError, CacheStore};

/// Runs one script line — `COMMAND key args…` — and renders the reply the way `redis-cli`
/// would: `(integer) n`, `"text"`, `(nil)`, `(empty)`, `1) "a"` lists joined by `|`, or
/// `ERR wrongtype`.
async fn run(cache: &impl CacheStore, line: &str) -> String {
    let site = sid("script.test");
    let parts: Vec<&str> = line.split_whitespace().collect();
    let (command, key, args) = (parts[0], parts[1], &parts[2..]);
    let values: Vec<_> = args.iter().map(|arg| bytes(arg)).collect();
    let int = |n: u64| format!("(integer) {n}");
    let flag = |b: bool| int(u64::from(b));
    let one = |v: Option<spot_cache::Bytes>| match v {
        Some(v) => format!("{:?}", String::from_utf8_lossy(&v)),
        None => "(nil)".to_owned(),
    };
    let many = |v: Vec<spot_cache::Bytes>| {
        if v.is_empty() {
            "(empty)".to_owned()
        } else {
            v.iter()
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .collect::<Vec<_>>()
                .join("|")
        }
    };
    let n = |i: usize| args[i].parse::<i64>().unwrap_or(0);
    macro_rules! reply {
        ($result:expr, $render:expr) => {
            match $result {
                Ok(v) => $render(v),
                Err(CacheError::WrongType) => "ERR wrongtype".to_owned(),
                Err(e) => format!("ERR {e}"),
            }
        };
    }
    match command {
        "SET" => reply!(cache.set(&site, key, values[0].clone(), None).await, |()| {
            "OK".to_owned()
        }),
        "GET" => reply!(cache.get(&site, key).await, one),
        "HSET" => reply!(
            cache.hset(&site, key, args[0], values[1].clone()).await,
            flag
        ),
        "HGET" => reply!(cache.hget(&site, key, args[0]).await, one),
        "HDEL" => reply!(cache.hdel(&site, key, args).await, int),
        "HLEN" => reply!(cache.hlen(&site, key).await, int),
        "HEXISTS" => reply!(cache.hexists(&site, key, args[0]).await, flag),
        "HINCRBY" => reply!(
            cache.hincr_by(&site, key, args[0], n(1)).await,
            |v: i64| format!("(integer) {v}")
        ),
        "LPUSH" => reply!(cache.lpush(&site, key, &values).await, int),
        "RPUSH" => reply!(cache.rpush(&site, key, &values).await, int),
        "LPOP" => reply!(cache.lpop(&site, key).await, one),
        "RPOP" => reply!(cache.rpop(&site, key).await, one),
        "LLEN" => reply!(cache.llen(&site, key).await, int),
        "LRANGE" => reply!(cache.lrange(&site, key, n(0), n(1)).await, many),
        "LTRIM" => reply!(cache.ltrim(&site, key, n(0), n(1)).await, |()| "OK"
            .to_owned()),
        "SADD" => reply!(cache.sadd(&site, key, &values).await, int),
        "SREM" => reply!(cache.srem(&site, key, &values).await, int),
        "SCARD" => reply!(cache.scard(&site, key).await, int),
        "SISMEMBER" => reply!(cache.sismember(&site, key, args[0].as_bytes()).await, flag),
        "EXISTS" => flag(cache.exists(&site, key).await),
        "DEL" => flag(cache.delete(&site, key).await),
        other => panic!("script uses an unknown command {other}"),
    }
}

#[tokio::test]
async fn a_short_script_replies_like_redis() {
    let (cache, _clock) = cache();
    let script: &[(&str, &str)] = &[
        ("RPUSH q a b c", "(integer) 3"),
        ("LPUSH q z y", "(integer) 5"),
        ("LRANGE q 0 -1", "y|z|a|b|c"),
        ("LRANGE q -3 -1", "a|b|c"),
        ("LRANGE q 3 100", "b|c"),
        ("LRANGE q 7 9", "(empty)"),
        ("LPOP q", "\"y\""),
        ("RPOP q", "\"c\""),
        ("LTRIM q 1 1", "OK"),
        ("LRANGE q 0 -1", "a"),
        ("LLEN q", "(integer) 1"),
        ("LTRIM q 4 9", "OK"),
        ("EXISTS q", "(integer) 0"),
        ("LPOP q", "(nil)"),
        ("HSET h f 1", "(integer) 1"),
        ("HSET h f 2", "(integer) 0"),
        ("HINCRBY h f 40", "(integer) 42"),
        ("HINCRBY h g -1", "(integer) -1"),
        ("HGET h f", "\"42\""),
        ("HLEN h", "(integer) 2"),
        ("HEXISTS h g", "(integer) 1"),
        ("HDEL h f nope", "(integer) 1"),
        ("HDEL h g", "(integer) 1"),
        ("EXISTS h", "(integer) 0"),
        ("HGET h f", "(nil)"),
        ("SADD s a b a", "(integer) 2"),
        ("SADD s b c", "(integer) 1"),
        ("SCARD s", "(integer) 3"),
        ("SISMEMBER s c", "(integer) 1"),
        ("SISMEMBER s q", "(integer) 0"),
        ("SREM s a q", "(integer) 1"),
        ("SREM s b c", "(integer) 2"),
        ("EXISTS s", "(integer) 0"),
        ("SET str v", "OK"),
        ("LPUSH str x", "ERR wrongtype"),
        ("HGET str f", "ERR wrongtype"),
        ("SADD str x", "ERR wrongtype"),
        ("RPUSH l x", "(integer) 1"),
        ("GET l", "ERR wrongtype"),
        ("HSET l f v", "ERR wrongtype"),
        ("SCARD l", "ERR wrongtype"),
        ("SET l v", "OK"),
        ("GET l", "\"v\""),
        (
            "HINCRBY h2 f 9223372036854775807",
            "(integer) 9223372036854775807",
        ),
        (
            "HINCRBY h2 f 1",
            "ERR increment would overflow a 64-bit integer",
        ),
        ("HSET h2 t abc", "(integer) 1"),
        ("HINCRBY h2 t 1", "ERR value is not an integer"),
    ];
    for (line, expected) in script {
        assert_eq!(&run(&cache, line).await, expected, "`{line}`");
    }
}
