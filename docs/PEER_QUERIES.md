# Read-only node query contract

The optional `peer_token_env` enables `POST /internal/v1/peer/query` in a single-region
service, or `POST /internal/v1/{region}/peer/query` in a multi-region service. Leave it
unset to disable these routes. Set it inside each region's configuration for a multi-region
service. Restart after changing the credential reference/value.

```yaml
peer_token_env: SIRIUS_JP_PEER_TOKEN
```

Use a dedicated Bearer token per region. Deployment startup rejects reuse across regions
or with public API, administrative, configured game credential environment values, CDN or
asset-updater tokens. Peer authorization does not grant account inspection, protocol reload,
resource snapshot or updater control. Protect the listener with the existing TLS configuration
or a trusted encrypted private transport; this setting does not configure transport encryption.

## Contract version 1

A request contains a UUID `request_id`, an exact `identity` and a typed `operation`:

```json
{
  "request_id": "681e3c10-293b-4487-ac70-0dcb3f73947d",
  "identity": {
    "contract_version": 1,
    "region": "jp",
    "environment": "release",
    "platform": "iOS",
    "client_version": "1.0.3",
    "protocol_sha256": "<configured bundle's semantic fingerprint>"
  },
  "operation": { "type": "version" }
}
```

The schema fingerprint is the `sha256` from the existing administrative protocol status
endpoint. It is not a game asset or executable hash. A peer token cannot read that administrative
endpoint. HK/EN/KR may share a schema fingerprint while remaining distinct identities. CN remains
reserved. Compatibility requires all identity fields to match; a schema mismatch does not
silently downgrade or retry against an arbitrary protocol.

Operations and parameters mirror the existing public API validation:

| Type | Parameters |
| --- | --- |
| `version`, `servers` | None; `servers` requires a verified Global protocol |
| `announcements` | `tab`: 0..2 |
| `announcement` | Positive integer `id` |
| `profile` | Positive integer `profile_id` |
| `event_ranking` | Positive `event_id`, 1..100 unique positive `ranks` |
| `event_deck` | Positive `event_id`, bounded alphanumeric/hyphen/underscore `player_id` |
| `music_ranking` | Positive integer `music_id` |
| `challenge_ranking` | Positive integer `challenge_music_id` |

JP supports the verified JP public query operations; Global currently supports only version
and server discovery. There is no arbitrary URL, RPC name, account name, credential, login or
mutation field. Unknown fields/operations fail validation. Request bodies are limited to 16 KiB.
Each accepted query executes on this node's local GameClient, under its normal admission,
timeout, account serialization and response-cache policies. It never invokes another node.
The schema hash is rechecked after acquiring admission and the protocol read barrier, preventing
a reload from changing a queued request's contract before execution.

HTTP 200 contains the request identity/id and a typed outcome: `status: success` with `data`,
or `status: failure` with `kind`. The reply also carries the executor's sanitized `observation`. Failure types are `identity_mismatch`, `unsupported_operation`,
`account_unavailable`, `unavailable_before_dispatch`, `timeout`, `transport`, `protocol`, or `game` with `grpc_status`.
The echoed identity binds the response to the request; an identity failure does not claim
that identity was accepted. Malformed input and failed authorization use HTTP errors.
Raw upstream diagnostics and credentials are never serialized. Account-relative `myRank` and
`myScore` are removed from music/challenge ranking results, as on the public API.

A timeout/transport failure is not proof that the game query was never sent. This endpoint
alone does not authorize an automatic replay of authenticated queries.

## Restoration status

Public query routing now uses this contract; see [NODE_ROUTING.md](NODE_ROUTING.md) for
priorities, bounded failover and health. `unavailable_before_dispatch` is emitted only when
account selection fails before a game request; later account errors remain ambiguous. Full
yhm01 acceptance and release gates remain pending.

## Outbound transport foundation

`peer_transport::Client` executes one request against one configured origin. It builds only the
single-region or selected-region peer path, never a caller-supplied path. HTTPS and certificate
validation are mandatory unless the constructor explicitly permits HTTP for a private network.
The token is attached only to that origin; redirects, ambient proxies and HTTP-library retries
are disabled. A successful HTTP status alone is insufficient: JSON content type, response UUID,
all identity fields, tagged outcome and body size are checked. Unknown response fields, malformed
outcomes and impossible failure gRPC statuses are rejected. Protobuf JSON int64 strings remain
strings.

The transport policy defaults to a 5-second connect timeout, 20-second request timeout and
16 MiB response bound. Timeouts are bounded to 100..300000 ms, connection timeout cannot exceed
request timeout, and response bounds are 1 KiB..128 MiB. Every call also takes an absolute
caller deadline that bounds headers and streamed bodies, so subsequent targets cannot receive
a fresh overall timeout budget. Both declared and chunked response sizes are enforced.

`Config`, `NotSent` (already expired) and `Connect` errors prove this transport did not submit
the request. `Timeout`, `Transport`, `Protocol` and HTTP status errors do **not** prove that;
an executing node may have completed the game query before its response was lost. Typed game
outcomes are returned separately. Dropping a call does not promise remote cancellation.
No replay or fallback occurs in the transport itself. Node routing will own that decision.

Tests cover a real HTTP peer executing local gRPC, single/multi-region path selection, token
scope, exact response identity, unknown fields, int64 strings, fixed/chunked oversized bodies,
redirect refusal, untrusted TLS, header/body stalls under both timeout budgets and connection
versus mid-body failures. The service node router uses this transport with explicit outbound configuration.
