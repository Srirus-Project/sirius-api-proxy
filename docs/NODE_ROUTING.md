# Node routing

Configure `node_routing` per region to route existing public game queries between the local
client and remote Sirius API nodes. Omitting it preserves local-only behavior. Remote nodes
must enable their dedicated `peer_token_env`; see [PEER_QUERIES.md](PEER_QUERIES.md).

```yaml
node_routing:
  local_priority: 0 # null excludes local execution from public query selection
  timeout_ms: 20000 # whole logical request, including admission and all targets
  max_inflight: 64
  failure_threshold: 3
  cooldown_ms: 30000
  transport:
    connect_timeout_ms: 5000
    request_timeout_ms: 20000
    max_response_bytes: 16777216
  targets:
    - name: secondary
      origin: https://secondary.example.invalid
      token_env: SIRIUS_JP_SECONDARY_PEER_TOKEN
      priority: 10
      regional_paths: true # false for a single-region peer deployment
      allow_http: false
```

Lower priorities run first. Local wins equal-priority ties; remote ties preserve configuration
order. At most 16 remote targets are allowed, with unique names and destinations. `local_priority:
null` plus at least one remote supports a public-query frontend without local game accounts.
The normal region, endpoint, client version and protocol configuration still identify the game
contract; no fabricated account or protocol is required. An empty target set with local disabled
is an error. Restart after changing routing configuration or token environment values.

Every peer request carries the selected region, environment, platform, client version and exact
semantic protocol fingerprint. Global operations remain limited to verified version/server
queries; shared HK/EN/KR schemas do not merge their identities. CN is reserved. Outgoing peer
credentials cannot reuse public/admin/game/CDN/updater credentials or credentials for another
region. A region's dedicated peer credential may be shared among its cooperating nodes.

## Failover and health

All target attempts and admission share one absolute deadline. Request/connect/body bounds still
apply to each HTTP peer attempt. Anonymous reads can continue after target transport/protocol
failures while budget remains. Authenticated queries continue only when the attempt is known not
to have executed: connection failure, contract/capability rejection, or an explicit account
admission rejection before game dispatch. Ambiguous timeout/transport/protocol failures return
immediately for authenticated reads. No concurrent hedging or same-target retry is performed by
the router. The executing node retains its existing local anonymous retry policy.

Game gRPC outcomes, including maintenance/unavailable results, are terminal rather than presumed
target failures. Target failures increment a passive counter; after the threshold they enter
cooldown. An expired cooldown admits one probe while other calls use remaining targets. Successful
execution or a valid game outcome resets the counter. Cancelling a probe releases its slot.
Health is in-memory and resets on process restart. If every target is cooling down, the request
returns unavailable. Bounds: 100..300000 ms total timeout/cooldown, 1..4096 inflight calls and
1..100 failures before cooldown.

`GET /internal/v1/nodes` (or `/internal/v1/{region}/nodes`) requires the administrative token and
shows ordered names, priorities, failure counts, probe status and remaining cooldown. Origins,
credential references and values are not returned. The public bearer cannot access this status.

## Scope

System/version, server discovery, announcements, public profiles, event decks and ranking routes
use this router. Incoming peer queries execute locally even if this node has routing configured,
so peers cannot form forwarding loops. Account inspection/reload, protocol management, Master
storage/updating, resource snapshots and asset-dispatch workers remain local. In particular,
`local_priority: null` controls public queries; it does not disable explicitly configured local
background workers. Master table HTTP reads remain local snapshot reads.

`/system` returns the selected executor's observation. Remote observations never update the local
resource snapshot, account pool or CDN credentials. Each executor owns its local response cache;
there is no new shared cross-node cache. Ranking account-relative fields remain stripped at the
public boundary. Schema reload changes the identity used for subsequent peer requests; an old
identity queued at the executor is rejected before dispatch.

This restores service routing locally; yhm01 multi-node/failure acceptance and the complete 1.2.0
production/release gates remain required. Library callers using `GameClient::call` deliberately
retain local execution; public route handlers use `public_call`/`public_query`.
