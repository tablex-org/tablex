//! Pins the Redis classifier mirrored in `engine_agent::classify` to the
//! driver's own `classify_command`.
//!
//! The rule the mirror must keep: **never more permissive than the driver**.
//! A command the driver blocks is blocked here; a command the driver does not
//! call `Allowed` is not a read here; a read here is `Allowed` there. The
//! mirror may be stricter (`PUBLISH`, `AUTH` are reads for the driver but
//! writes for an agent, `FUNCTION` / `FCALL` / `DEBUG` are blocked).

use dbengine_redis::{classify_command, RedisCommandSafety};
use engine_agent::classify::{
    classify_redis, StatementClass, REDIS_AGENT_BLOCKED_EXTRA, REDIS_AGENT_WRITE_OVERRIDES,
    REDIS_BLOCKED_VERBS, REDIS_READ_VERBS,
};

/// Every command of Redis 7 (core, no modules) that this test knows of, read
/// and write alike. A command missing here is simply not covered; the two
/// verb lists in the crate are covered in full by the first test.
const REDIS_COMMANDS: &[&str] = &[
    "ACL",
    "APPEND",
    "ASKING",
    "AUTH",
    "BGREWRITEAOF",
    "BGSAVE",
    "BITCOUNT",
    "BITFIELD",
    "BITFIELD_RO",
    "BITOP",
    "BITPOS",
    "BLMOVE",
    "BLMPOP",
    "BLPOP",
    "BRPOP",
    "BRPOPLPUSH",
    "BZMPOP",
    "BZPOPMAX",
    "BZPOPMIN",
    "CLIENT",
    "CLUSTER",
    "COMMAND",
    "CONFIG",
    "COPY",
    "DBSIZE",
    "DEBUG",
    "DECR",
    "DECRBY",
    "DEL",
    "DISCARD",
    "DUMP",
    "ECHO",
    "EVAL",
    "EVALSHA",
    "EVALSHA_RO",
    "EVAL_RO",
    "EXEC",
    "EXISTS",
    "EXPIRE",
    "EXPIREAT",
    "EXPIRETIME",
    "FAILOVER",
    "FCALL",
    "FCALL_RO",
    "FLUSHALL",
    "FLUSHDB",
    "FUNCTION",
    "GEOADD",
    "GEODIST",
    "GEOHASH",
    "GEOPOS",
    "GEORADIUS",
    "GEORADIUSBYMEMBER",
    "GEOSEARCH",
    "GEOSEARCHSTORE",
    "GET",
    "GETBIT",
    "GETDEL",
    "GETEX",
    "GETRANGE",
    "GETSET",
    "HDEL",
    "HELLO",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HMSET",
    "HRANDFIELD",
    "HSCAN",
    "HSET",
    "HSETNX",
    "HSTRLEN",
    "HVALS",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "INFO",
    "KEYS",
    "LASTSAVE",
    "LATENCY",
    "LCS",
    "LINDEX",
    "LINSERT",
    "LLEN",
    "LMOVE",
    "LMPOP",
    "LOLWUT",
    "LPOP",
    "LPOS",
    "LPUSH",
    "LPUSHX",
    "LRANGE",
    "LREM",
    "LSET",
    "LTRIM",
    "MEMORY",
    "MGET",
    "MIGRATE",
    "MODULE",
    "MONITOR",
    "MOVE",
    "MSET",
    "MSETNX",
    "MULTI",
    "OBJECT",
    "PERSIST",
    "PEXPIRE",
    "PEXPIREAT",
    "PEXPIRETIME",
    "PFADD",
    "PFCOUNT",
    "PFDEBUG",
    "PFMERGE",
    "PFSELFTEST",
    "PING",
    "PSETEX",
    "PSUBSCRIBE",
    "PSYNC",
    "PTTL",
    "PUBLISH",
    "PUBSUB",
    "PUNSUBSCRIBE",
    "QUIT",
    "RANDOMKEY",
    "READONLY",
    "READWRITE",
    "RENAME",
    "RENAMENX",
    "REPLCONF",
    "REPLICAOF",
    "RESET",
    "RESTORE",
    "RESTORE-ASKING",
    "ROLE",
    "RPOP",
    "RPOPLPUSH",
    "RPUSH",
    "RPUSHX",
    "SADD",
    "SAVE",
    "SCAN",
    "SCARD",
    "SCRIPT",
    "SDIFF",
    "SDIFFSTORE",
    "SELECT",
    "SET",
    "SETBIT",
    "SETEX",
    "SETNX",
    "SETRANGE",
    "SHUTDOWN",
    "SINTER",
    "SINTERCARD",
    "SINTERSTORE",
    "SISMEMBER",
    "SLAVEOF",
    "SLOWLOG",
    "SMEMBERS",
    "SMISMEMBER",
    "SMOVE",
    "SORT",
    "SORT_RO",
    "SPOP",
    "SPUBLISH",
    "SRANDMEMBER",
    "SREM",
    "SSCAN",
    "SSUBSCRIBE",
    "STRLEN",
    "SUBSCRIBE",
    "SUBSTR",
    "SUNION",
    "SUNIONSTORE",
    "SUNSUBSCRIBE",
    "SWAPDB",
    "SYNC",
    "TIME",
    "TOUCH",
    "TTL",
    "TYPE",
    "UNLINK",
    "UNSUBSCRIBE",
    "UNWATCH",
    "WAIT",
    "WAITAOF",
    "WATCH",
    "XACK",
    "XADD",
    "XAUTOCLAIM",
    "XCLAIM",
    "XDEL",
    "XGROUP",
    "XINFO",
    "XLEN",
    "XPENDING",
    "XRANGE",
    "XREAD",
    "XREADGROUP",
    "XREVRANGE",
    "XSETID",
    "XTRIM",
    "ZADD",
    "ZCARD",
    "ZCOUNT",
    "ZDIFF",
    "ZDIFFSTORE",
    "ZINCRBY",
    "ZINTER",
    "ZINTERCARD",
    "ZINTERSTORE",
    "ZLEXCOUNT",
    "ZMPOP",
    "ZMSCORE",
    "ZPOPMAX",
    "ZPOPMIN",
    "ZRANDMEMBER",
    "ZRANGE",
    "ZRANGEBYLEX",
    "ZRANGEBYSCORE",
    "ZRANGESTORE",
    "ZRANK",
    "ZREM",
    "ZREMRANGEBYLEX",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREVRANGE",
    "ZREVRANGEBYLEX",
    "ZREVRANGEBYSCORE",
    "ZREVRANK",
    "ZSCAN",
    "ZSCORE",
    "ZUNION",
    "ZUNIONSTORE",
];

fn driver_class(command: &str) -> RedisCommandSafety {
    classify_command(command)
}

fn assert_not_more_permissive(command: &str) {
    let mine = classify_redis(command).class;
    let theirs = driver_class(command);
    match theirs {
        RedisCommandSafety::Blocked => {
            assert_eq!(
                mine,
                StatementClass::Blocked,
                "`{command}` is blocked by the driver"
            );
        }
        RedisCommandSafety::Confirm => {
            // A write here, or blocked outright (server-side code, `DEBUG`).
            assert_ne!(
                mine,
                StatementClass::Read,
                "`{command}` needs confirmation in the driver"
            );
        }
        RedisCommandSafety::Allowed => {
            assert_ne!(
                mine,
                StatementClass::Blocked,
                "`{command}` is allowed by the driver"
            );
        }
    }
    if mine == StatementClass::Read {
        assert_eq!(
            theirs,
            RedisCommandSafety::Allowed,
            "`{command}` is a read here, not in the driver"
        );
    }
}

#[test]
fn the_mirrored_lists_equal_the_drivers() {
    for verb in REDIS_READ_VERBS {
        assert_eq!(driver_class(verb), RedisCommandSafety::Allowed, "{verb}");
        assert_eq!(
            driver_class(&verb.to_lowercase()),
            RedisCommandSafety::Allowed,
            "{verb} lower case"
        );
        let mine = classify_redis(&format!("{verb} key")).class;
        let expected = if REDIS_AGENT_WRITE_OVERRIDES.contains(verb) {
            StatementClass::Write
        } else {
            StatementClass::Read
        };
        assert_eq!(mine, expected, "{verb}");
    }
    for verb in REDIS_AGENT_BLOCKED_EXTRA {
        assert_eq!(
            classify_redis(&format!("{verb} x")).class,
            StatementClass::Blocked,
            "{verb}"
        );
        assert!(!REDIS_READ_VERBS.contains(verb), "{verb}");
        assert_ne!(
            driver_class(verb),
            RedisCommandSafety::Allowed,
            "{verb} would be a read in the driver"
        );
    }
    for verb in REDIS_BLOCKED_VERBS {
        assert_eq!(driver_class(verb), RedisCommandSafety::Blocked, "{verb}");
        assert_eq!(
            classify_redis(&format!("{verb} x")).class,
            StatementClass::Blocked,
            "{verb}"
        );
    }
    // The overrides are driver reads that an agent must treat as writes.
    for verb in REDIS_AGENT_WRITE_OVERRIDES {
        assert!(REDIS_READ_VERBS.contains(verb), "{verb}");
    }
}

#[test]
fn no_known_redis_command_is_classified_more_permissively_than_the_driver_does() {
    for command in REDIS_COMMANDS {
        assert_not_more_permissive(command);
        assert_not_more_permissive(&format!("{command} key value"));
        assert_not_more_permissive(&command.to_lowercase());
    }
}

#[test]
fn quoting_and_odd_input_follow_the_drivers_parser() {
    for command in [
        "",
        "   ",
        "\"get\" k",
        "\"flushall\"",
        "  Keys   *",
        "GET \"unterminated",
        "SET k \"a b\"",
        "\"\"",
        "get\tk",
        "\\get k",
        "con\"fig\" get *",
    ] {
        assert_not_more_permissive(command);
    }
    // The driver reads an empty command as harmless; so does the mirror.
    assert_eq!(classify_redis("").class, StatementClass::Read);
    assert_eq!(driver_class(""), RedisCommandSafety::Allowed);
}
