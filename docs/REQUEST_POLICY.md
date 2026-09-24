# Upstream request policy

The `upstream` block belongs to each region's configuration. Omitting it keeps the
20-second deadline and 8 MiB response cap, disables automatic retries, and admits at
most 64 logical calls per region. Each configuration field affects the gRPC client.
Master CDN downloads have their separate existing limits.

| Field | Default | Allowed values |
| --- | --- | --- |
| `timeout_ms` | 20000 | 100..300000 |
| `max_response_bytes` | 8388608 | 1024..134217728 |
| `max_inflight` | 64 | 1..4096 |
| `anonymous_attempts` | 1 | 1..5 total attempts |
| `retry_delay_ms` | 250 | 1..10000 |

A logical call includes admission wait, per-account session wait, protocol activation
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
HTTP proxy transport, inbound TLS, access logs and forwarding trust remain separate
restoration items; this configuration does not claim to implement them.
