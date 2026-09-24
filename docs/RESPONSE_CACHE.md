# Response cache

Caching is opt-in per region and disabled by default. Only public announcement
list/detail and event/song/challenge ranking responses are eligible. Profile queries,
decks, identity, private player data, server discovery and Version always execute
normally. Service-account `myRank`/`myScore` fields are removed before ranking values
enter storage. Errors and maintenance responses are never inserted.

```yaml
response_cache:
  backend: memory
  ttl_ms: 1000
  route_ttl_ms:
    announcements: 30000
    announcement: 30000
    event_rankings: 500
    song_rankings: 1000
    challenge_rankings: 1000
  max_entries: 1024
  max_bytes: 33554432
  max_entry_bytes: 1048576
```

Memory storage enforces both entry count and serialized-byte budgets; oversized
entries are skipped and stored entries can be evicted. Eviction order is
lexicographic by digest, not LRU. TTL is checked before serving. No expired response
is returned and no failed response is stored.

```yaml
response_cache:
  backend: redis
  url_env: SIRIUS_JP_CACHE_REDIS_URL
  namespace: sirius_public
  ttl_ms: 1000
  max_entry_bytes: 1048576
  operation_timeout_ms: 100
```

The environment variable holds a `redis://` or verified-TLS `rediss://` connection
URL, including credentials/database when needed. Never commit its actual value.
Namespaces accept letters, digits, underscores and hyphens. Redis URLs with query
or fragment are rejected. Startup validates configuration without requiring Redis
to be online. Redis errors become cache misses or skipped writes; no raw Redis
error/URL is sent to API clients. Cache operations consume the logical request's
remaining deadline in addition to their own short operation timeout. Each lookup/write
is further capped at one quarter of the remaining request budget so an unavailable
cache cannot consume the entire budget before the game call. Account health is based
on the game response, independently of cache insertion.

Redis entries use a server TTL plus an embedded expiry timestamp. Reads request at
most `max_entry_bytes + 1` bytes and reject oversized/invalid content, including
externally written entries. Global Redis capacity/eviction is configured by its
operator; unlike the memory backend, this block does not impose a database-wide
entry count. Cache storage should have access controls appropriate to public game
query results. Do not point it at an unrelated application's Redis namespace.

## Identity and invalidation

Keys are SHA-256 digests covering cache schema, region, environment, platform,
upstream origin, client version, protocol fingerprint/generation, observed Master
version, route, request JSON and account identity/credential digest/generation.
No raw game credential is part of a Redis key or cached value. Two accounts with
the same public query do not share entries. Different regions/protocols/upstreams
cannot collide through a common URL or namespace.

Account/protocol reload changes generations before subsequent calls can access
entries. Changed credentials and protocol fingerprints also change keys across
process restarts. Entries from previous scopes expire naturally without flushing
an operator's shared database. A cache hit does not count as an upstream health
success and cannot re-enable a quarantined account. Known maintenance bypasses hits.

TTL accepts 1..300000 ms. Memory accepts 1..100000 entries, a 1 KiB..1 GiB total
budget and a 256-byte..8 MiB entry cap no larger than the total. Redis entry caps
have the same bounds and operation timeout accepts 1..2000 ms. Unknown options fail
startup. HTTP caching headers/browser caches are separate from this server cache.

## Per-route policy and concurrent fills

Both backends accept the optional `route_ttl_ms` map shown above. These five names
are the entire allowlist; private/profile/deck routes cannot be added through config.
Each override accepts 0..300000 ms; 0 bypasses both lookup and insertion for that
route. Omitted routes inherit the backend's `ttl_ms`. Effective TTL is part of the
cache key, so processes with different policies cannot reuse a longer-lived entry
from a common Redis namespace. The selected TTL controls both embedded expiry and
Redis PX expiry. Restart to change configuration.

Concurrent misses for a key are coalesced within a region/process. The winner fills
the cache; waiters check it again before executing. Waiting remains inside each
request's original deadline. Failed fills do not populate the cache, and cancelling
a winner releases its lock so another request can proceed. A fixed set of 64 lock
stripes bounds coordination memory; digest collisions can serialize unrelated fills.
Fresh hits do not wait for a fill lock. This is not a distributed Redis lease.

The original stale-while-revalidate policy remains tracked restoration work. Expired
entries are still never served and this implementation does not launch background
refreshes or hide an outage behind an expired response.
