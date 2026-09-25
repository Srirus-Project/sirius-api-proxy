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
endpoint. TW/EN/KR may share a schema fingerprint while remaining distinct identities. CN remains
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
or `status: failure` with `kind`. Failure types are `identity_mismatch`, `unsupported_operation`,
`account_unavailable`, `timeout`, `transport`, `protocol`, or `game` with `grpc_status`.
The echoed identity binds the response to the request; an identity failure does not claim
that identity was accepted. Malformed input and failed authorization use HTTP errors.
Raw upstream diagnostics and credentials are never serialized. Account-relative `myRank` and
`myScore` are removed from music/challenge ranking results, as on the public API.

A timeout/transport failure is not proof that the game query was never sent. This endpoint
alone does not authorize an automatic replay of authenticated queries.

## Restoration status

This is the executing node interface, not completed multi-node routing. Outbound node selection,
priorities, total-deadline failover, passive health, remote-only deployment and public-route
integration remain required restoration work. The full yhm01 acceptance and 1.2.0 release gates
remain pending. Local tests exercise real gRPC execution, errors without diagnostic leakage,
credential separation, request limits, Global identity/capability boundaries and protocol reload.
