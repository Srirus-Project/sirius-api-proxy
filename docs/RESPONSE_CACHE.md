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
  stale_while_revalidate_ms: 0 # opt in with a bounded nonzero window
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
lexicographic by digest, not LRU. Freshness and hard retention expiry are checked before serving.
By default expired responses are not returned; a configured stale window permits bounded stale
responses as described below. Failed responses are never stored.

```yaml
response_cache:
  backend: redis
  url_env: SIRIUS_JP_CACHE_REDIS_URL
  namespace: sirius_public
  ttl_ms: 1000
  stale_while_revalidate_ms: 0 # opt in with a bounded nonzero window
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
from a common Redis namespace. The selected TTL controls freshness; Redis PX expiry includes the optional stale window.
Both freshness TTL and stale window are part of the digest. Restart to change configuration.

Concurrent misses for a key are coalesced within a region/process. The winner fills
the cache; waiters check it again before executing. Waiting remains inside each
request's original deadline. Failed fills do not populate the cache, and cancelling
a winner releases its lock so another request can proceed. A fixed set of 64 lock
stripes bounds coordination memory; digest collisions can serialize unrelated fills.
Fresh hits do not wait for a fill lock. This is not a distributed Redis lease.

## Stale-while-revalidate

Both backends accept `stale_while_revalidate_ms`, default 0 (disabled), range 0..300000.
Between freshness expiry and TTL plus this window, return the cached public response immediately
and attempt one background refresh per key/process. Hard-expired entries are misses and wait for
normal request processing. Route TTL 0 still bypasses caching entirely. Changing the window changes
cache identity. Old records without a retention timestamp retain their original freshness expiry.

Refresh reservations and cache-fill coordination use separate bounded sets of 64 striped locks.
A background task acquires normal request admission before waiting for a fill lock, preventing a
queued refresh from blocking admitted cache misses. Stripe collisions can suppress unrelated
refresh attempts until a later request. Multiple processes do not share a refresh lease, even on
Redis. Cache hits do not wait for the account session lock; actual upstream refreshes still obey
session serialization, global request admission, request deadlines, retry and account-health rules.

The refresh pins the originating account by name and rechecks its complete cache scope after
admission and session-lock acquisition. Changed credentials, protocol/Master generations or
maintenance invalidate the scheduled work instead of refreshing another scope. Ranking-relative
fields are stripped on stale reads as well as writes. No private/profile/deck caching is enabled.
Successful refresh replaces the entry with a new TTL/window; failures leave its original hard
expiry unchanged. Later stale requests may try again under normal account-health limits. A server
outage can therefore be masked only within the explicitly configured window. Background work is
bounded by the normal request deadline and is disposable on process shutdown; it is not a durable
job. Cache responses keep the existing JSON contract and do not add a freshness metadata field.
