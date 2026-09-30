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
| `http2_keepalive_interval_ms` | omitted: min(10000, `timeout_ms` / 2); off when `timeout_ms` < 4000 and neither keepalive key is set | 0 disables keepalive, otherwise 1000..300000 |
| `http2_keepalive_timeout_ms` | omitted: min(5000, `timeout_ms` / 4) | 1000..60000; not with interval 0 |
| `version_max_age_seconds` | 600 | 60..86400 |

A logical call includes admission wait, per-account session or anonymous slot wait, protocol activation
wait, anonymous Version bootstrap or refresh, optional identity verification, retries and the
requested RPC. These share one deadline; entering another stage never resets it.
The gRPC timeout header advertises the remaining budget, rounded up to milliseconds.
Timeout/cancellation returns the regional admission permit. The response cap applies
to accumulated HTTP DATA bytes, including the five-byte gRPC frame header, before
protobuf decoding. Oversized or malformed data is rejected without retry.

`max_inflight` limits admitted logical calls, including those waiting for their session
lock. Additional requests wait within their own deadline. Since 1.3.0 response-cache hits are
answered before admission and neither take nor wait for a permit (see
[hits before admission](RESPONSE_CACHE.md#hits-before-admission)); misses are admitted as before. Per-account serialization
still applies when `session_lock` is true; increasing regional concurrency does not
implicitly enable concurrent use of one game session. Each region owns its semaphore.

With `session_lock: true`, calls without an account (Version, server list, announcements,
peer and cache-refresh calls of those routes) no longer share one regional lock: up to
`anonymous_max_inflight` of them run at once, so a slow Version does not hold back an
announcement read. `anonymous_max_inflight: 1` restores the 1.2.x serialization. The Version
bootstrap of an authenticated call and the [freshness refresh](#version-header-freshness) stay
single-flight under their own lock and do not take an anonymous slot. With `session_lock: false` only `max_inflight` bounds anonymous calls.

## Connection liveness

Game RPCs share pooled HTTP/2 connections. A connection that silently stops delivering packets
(a dropped NAT entry, a dead proxy tunnel) is detected with HTTP/2 PING frames instead of every
call on it waiting for its whole deadline:

- A PING is sent only while a call is open and the connection has received nothing for
  `http2_keepalive_interval_ms`; a call that starts on a connection already silent that long
  pings at once. Idle connections are never pinged and are closed after 90 s unused, so a busy,
  healthy connection sends almost no PINGs.
- A PING not acknowledged within `http2_keepalive_timeout_ms` closes the connection. Calls in
  flight on it fail as `upstream_transport` (502) about interval + timeout after the connection
  went quiet (at most 15 s with the defaults) instead of `upstream_timeout` (504) at the
  deadline, and the next call opens a new connection. The failure is a path fault like any
  transport failure (see [path health](#upstream-path-health)).
- Verified anonymous reads with `anonymous_attempts > 1` may retry on a new connection inside
  the same deadline; authenticated calls are never replayed. A request is resent automatically
  only when it was never written to a connection.

When either key is set explicitly, interval + timeout must be less than `timeout_ms`, so a dead
connection fails the call before its deadline. The derived values sum to at most three quarters
of `timeout_ms`; they are not used below a 1-second acknowledgement (`timeout_ms` < 4000), which
keeps such configurations exactly as in 1.2.x. `http2_keepalive_interval_ms: 0` turns PINGs off
for an upstream that objects to them. The official client also configures HTTP/2 keepalive on
its connections.

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
round trip before it arrived. The Version bootstrap of authenticated calls and the version
refresh never join, since they already hold admission and the protocol barrier.

## Version header freshness

Every game RPC except Global PlayerLogin carries `x-master-version`, the version of the last
successful Version call. Authenticated calls without one first run Version and fail with its
error if it fails, as before. Afterwards the header is kept fresh, whatever the game enforces:

- **By age.** A call that finds the header older than `version_max_age_seconds` first runs
  Version (single-flight). Other calls arriving meanwhile do not wait; they go out with the
  current header. The refresh shares the call's deadline and uses at most half of what remains.
  It only happens while traffic flows, so an idle region sends nothing.
- **On `MASTER_VERSION_MISMATCH`.** When a response carries this `x-sirius-error-code` for the
  header currently in use, the version becomes suspect: the next call waits for one Version call,
  which is sent without `x-master-version` (exactly like the first Version call of the process),
  before it sends its own RPC. The call that received the code is not replayed; it fails as
  before (502 `upstream_grpc`, or 503 for gRPC 14) and never counts against the account (see
  [ACCOUNTS.md](ACCOUNTS.md)). The code is recognized whatever gRPC status comes with it. A late
  answer to a header that was already replaced is ignored, and the version in an error response
  is never adopted: only Version sets the version pair.
- **Failures back off.** A failed refresh keeps the previous headers, is logged
  (`error_code` is the refresh's error), and no refresh is tried for 30 s. So is a Version call
  that still returns the version the game called stale, to avoid one Version per call.
- `CLIENT_UPDATE_REQUIRED` is logged once (warn, until the next successful answer) and needs a
  new `client_version`; it does not trigger a refresh and never penalizes an account.

Workers' own Version calls (Master update, resource snapshots) refresh the header too. A
protocol activation clears it together with the other version observations.

Both codes are string literals next to the version check of the official JP 1.0.3 and Global
1.0.1 clients. Which gRPC status the game pairs with them, and whether it rejects an old
`x-master-version` at all, is not verified; the design depends on neither.

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
invalid protocol data and application failures do not trigger retries. Neither do
`MASTER_VERSION_MISMATCH` and `CLIENT_UPDATE_REQUIRED`, even with gRPC 14: the same headers
would be sent again, so the next call refreshes the version instead. Anonymous
Version bootstrap may retry before any account credential has been sent; its failure
does not penalize account health. This policy does not add registration or mutation RPCs.

No real credentials are needed for policy tests: local HTTP/2 fixtures verify exact
attempt counts, deadline headers, admission behavior and valid oversized wire data.
Optional [HTTP/HTTPS CONNECT transport](UPSTREAM_PROXY.md) is configured in the same
regional block. Inbound TLS, access logs and forwarding trust remain separate restoration items.
