# Upstream request policy

The `upstream` block belongs to each region's configuration. Omitting it keeps the
20-second deadline and 8 MiB response cap, disables automatic retries, and admits at
most 64 logical calls per region. Each configuration field affects the gRPC client.
Master CDN downloads have their separate existing limits.

| Field | Default | Allowed values |
| --- | --- | --- |
| `connect_timeout_ms` | 10000 | 100..300000 |
| `timeout_ms` | 20000 | 100..300000 |
| `max_response_bytes` | 8388608 | 1024..134217728 |
| `max_inflight` | 64 | 1..4096 |
| `anonymous_attempts` | 1 | 1..5 total attempts |
| `retry_delay_ms` | 250 | 1..10000 |
| `anonymous_max_inflight` | omitted: min(4, `max_inflight`) | 1..64 and at most `max_inflight` |
| `coalesce_public_reads` | false | `true` also shares identical ranking reads |

A logical call includes admission wait, per-account session or anonymous slot wait, protocol activation
wait, anonymous Version bootstrap, optional identity verification, retries and the
requested RPC. These share one deadline; entering another stage never resets it.
The gRPC timeout header advertises the remaining budget, rounded up to milliseconds.
Timeout/cancellation returns the regional admission permit. The response cap applies
to accumulated HTTP DATA bytes, including the five-byte gRPC frame header, before
protobuf decoding. Oversized or malformed data is rejected without retry.

`max_inflight` limits admitted logical calls, including those waiting for their session
lock. Additional requests wait within their own deadline. Per-account serialization
still applies when `session_lock` is true; increasing regional concurrency does not
implicitly enable concurrent use of one game session. Each region owns its semaphore.

With `session_lock: true`, calls without an account (Version, server list, announcements,
peer and cache-refresh calls of those routes) no longer share one regional lock: up to
`anonymous_max_inflight` of them run at once, so a slow Version does not hold back an
announcement read. `anonymous_max_inflight: 1` restores the 1.2.x serialization. The Version
bootstrap of an authenticated call stays single-flight under its own lock and does not take
an anonymous slot. With `session_lock: false` only `max_inflight` bounds anonymous calls.

## Shared in-flight reads

Identical concurrent public reads share one execution, independent of the
[response cache](RESPONSE_CACHE.md): it also applies with the cache disabled or a TTL of 0.
Version, server list and announcement list/detail always share; event, song and challenge
rankings share only with `coalesce_public_reads: true`, because one account's result (and
failure) then answers every joined request and is reported to account health once. Account
fields (`myRank`, `myScore`) are removed before a shared ranking is handed out. Profiles,
decks, private account data, logins, named-account calls and background cache refreshes never
share.

Two calls are identical when region, environment, endpoint, platform, client version,
protocol fingerprint and generation, the fingerprint a peer caller asserted, route and input
all match. A joined request:

- receives the outcome of the running call, errors and timeouts included, so a burst against a
  failing upstream causes one attempt (and one retry sequence);
- waits only within its own deadline and then returns a timeout on its own, without touching
  shared state;
- holds no admission permit, protocol barrier, anonymous slot or account while it waits.

If the running request is cancelled (client disconnect), one waiting request continues with
its own call and deadline; requests arriving after that start a new execution. A finished
call is never handed to a later request: this is not a cache, and the table lives in each
process only (not in Redis). A joined request can receive a response to an RPC sent up to one
round trip before it arrived. The Version bootstrap of authenticated calls never joins, since
it already holds admission and the protocol barrier.

## Upstream path health

Each region tracks the health of its game path (endpoint, proxy and network) apart from account
health, reusing `account_pool.failure_threshold` and `cooldown_seconds`; there is no separate
configuration. Global profiles with accounts track the SDK login path the same way.

- A sent attempt is a path fault when it gets no usable gRPC answer: a transport or proxy
  failure, a non-2xx or non-gRPC response, a missing gRPC status, gRPC 14 without an
  application code, or abandonment at the logical deadline. Any other gRPC status, including
  14 with `UNDER_MAINTENANCE` or `AEGIS_*`, is an answer and resets the count. Attempts never
  sent, cancelled by the caller before the deadline, or over the route's response limit do not
  count, and neither do waits for admission, a session lock or an anonymous slot.
- A run of faults becomes the path's once it involves two accounts or any anonymous call (every
  SDK fault is the SDK path's); the attribution rule and its effect on accounts are in
  [ACCOUNTS.md](ACCOUNTS.md). An attributed run of `failure_threshold` faults opens the path.
- While the path is open, a new logical call is refused with 503 `upstream_unavailable` before
  any upstream contact: before the Version bootstrap, before a Global SDK request or
  PlayerLogin (so no login is counted), before the RPC. A peer executor answers
  `unavailable_before_dispatch`, so routers fail over even authenticated reads. Response cache
  hits and a Global identity answered from an existing session are still served.
- After min(`cooldown_seconds`, 5 s), one call at a time is let through as a probe. Any gRPC
  answer closes the path; another fault restarts the interval.
- Admission is decided once per logical call. An admitted call keeps going (its bootstrap,
  anonymous retries and JP identity check) even if the path opens meanwhile, so the
  [retry policy](#retry-boundaries) is unchanged.

State is in memory, per region and per process; it survives account and protocol reloads.
`GET /internal/v1/accounts` reports it as `path` (and `sdk_path`). Transitions are logged
once each: `upstream_path_opened` and `upstream_path_probe_failed` (warn, with `region`,
`error_code` and `cooldown_ms`, the probe interval) and `upstream_path_recovered` (info);
the SDK path uses `sdk_path_opened`, `sdk_path_probe_failed` and `sdk_path_recovered`. Account
names are never logged.

A slow route that times out on two accounts (for example a large ranking lookup) also opens
the path for one probe interval; before 1.3.0 it cooled both accounts for `cooldown_seconds`.

## Retry boundaries

Retries are opt-in via `anonymous_attempts > 1`. Only the verified anonymous Version,
server-list and announcement-list/detail RPCs are eligible. Retry occurs only after a
transport failure or gRPC UNAVAILABLE (14), and only when the current observation does
not indicate maintenance. Delays are `retry_delay_ms`, twice that, four times that,
and so on, still bounded by the original logical deadline and total attempt count.
Each attempt gets a new request ID, while protocol generation and admission permit
remain pinned for the logical call.

Authenticated profile/ranking/account calls never automatically replay or switch
accounts within a request. Authentication errors, maintenance responses, rate limiting,
invalid protocol data and application failures do not trigger retries. Anonymous
Version bootstrap may retry before any account credential has been sent; its failure
does not penalize account health. This policy does not add registration or mutation RPCs.

No real credentials are needed for policy tests: local HTTP/2 fixtures verify exact
attempt counts, deadline headers, admission behavior and valid oversized wire data.
Optional [HTTP/HTTPS CONNECT transport](UPSTREAM_PROXY.md) is configured in the same
regional block. Inbound TLS, access logs and forwarding trust remain separate restoration items.
