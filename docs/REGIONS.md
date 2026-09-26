# Region support

Region is distinct from deployment environment and UI language. Deploy separate instances or
use the [multi-region service](MULTI_REGION.md), with isolated client state and regional routes.
Changing region requires a restart; hot reload changes compatible protobuf definitions within
one protocol family and cannot switch regions or account identity.

| Region | Game selection | Area ID | Default platform | Protocol family | Current capability |
| --- | --- | --- | --- | --- | --- |
| `jp` | Japan | Not inferred | `iOS` | JP 1.0.3 | Existing JP proxy, verified download/export pipeline and Master data |
| `hk` | TW/HK/MO | 2 | `Android` | Global 1.0.1 | Server discovery, version, SDK guest accounts and the JP proxy operations (see [Global operations](#global-operations)), Master data and resource snapshots |
| `en` | EN Region | 3 | `Android` | Global 1.0.1 | Same as `hk` |
| `kr` | Korea | 4 | `Android` | Global 1.0.1 | Same as `hk` |
| `cn` | Reserved | Unknown | Not operational | Not supplied | Configuration is recognized but startup/check rejects it |

The Traditional Chinese (TW/HK/MO) region is `hk`, the identifier the game itself uses (CDN
path `/prod/hk_…`, `l12-prod-hk-…` endpoints, server list); see
[the `hk` identifier](#the-hk-identifier).

`global` is not a region. HK, EN and KR have distinct API roots and Master versions. EN and KR
may share CDN hosts but use distinct base paths. Known production endpoints and CDN paths
that belong to another region are rejected; custom deployment origins remain configurable.
The `|`-separated entries returned by discovery are alternate URLs: select exactly one URL,
never paste the whole list into an endpoint. No automatic endpoint switching is performed.

## API proxy

Omitting `region` preserves JP behavior. `platform` accepts exactly `iOS` or `Android`; when
omitted it follows the table. A Global instance selects `protocol/global/1.0.1` by default;
an explicit protocol path must have the matching family. The existing JP default path remains
`protocol/sirius/1.0.3`. Both bundles generate native prost/pbjson codecs at build time. Exact
fingerprints select native codecs; compatible changed definitions use dynamic startup/reload.

`GET /api/v1/regions` reports the selected region and capability/reservation metadata.
`GET /api/v1/servers` calls the anonymous Global server-list RPC; it is not supported by JP.
`GET /api/v1/system` includes region, platform, protocol family, supported RPCs and observed
Master/resource versions. All three endpoints require the API token.

### Global operations

HK/EN/KR offer the same public and internal operations as JP. Authenticated operations use a
Global SDK guest account (see [Global accounts](ACCOUNTS.md#global-accounts)); without an account
they return 503 without sending a request. The Global bundle contains the client's own message
definitions for exactly these RPCs plus the internal `PlayerLogin`. It has no Whoami: Global
production rejects it ("native whoami is disabled in production"), so the proxy never sends it
and the account identity is the `PlayerLogin` result. JP credentials are never used for Global.

`GET /api/v1/regions` reports each operation's status in `operations`:

| Operation | Global RPC | Status |
| --- | --- | --- |
| `version` | `MasterdataService/Version` | `live_verified` |
| `servers` | `PlayerLoginService/GetServerList` | `live_verified` |
| `account_login` | SDK `cache.login` + `PlayerLoginService/PlayerLogin` | `live_verified` |
| `player_data` | `PlayerService/GetPlayerData` | `live_verified` |
| `account_identity` | none (PlayerLogin result) | `implemented_unverified` |
| `announcements` | `AnnouncementService/GetList`, `Get` | `implemented_unverified` |
| `profile` | `FriendService/FindByProfileID` | `implemented_unverified` |
| `event_ranking` | `EventService/GetRankingList` | `implemented_unverified` |
| `event_deck` | `EventService/GetDeck` | `implemented_unverified` |
| `music_ranking` | `LiveMusicService/GetRanking` | `implemented_unverified` |
| `challenge_ranking` | `EventService/GetChallengeMusicRanking` | `implemented_unverified` |

`live_verified` was exercised against production on all three servers (2026-09-26).
`implemented_unverified` uses paths, request fields and authentication options identical to JP in
the verified Global descriptors, and is covered by local mock tests only. JP reports
`live_verified` for its operations, `static_credentials` for `account_login` and `unsupported`
for `servers`; CN reports `reserved`. `capability` is `jp_proxy`, `global_proxy` or `reserved`.

Notes:

- Profile IDs, event IDs and player IDs are server-scoped: query the region that owns them.
- The client's descriptor names `ServerInfo` field 8 `areaID`. The proxy validates it by field
  number and keeps publishing it as `areaId` in `/api/v1/servers`.
- `GetPlayerData` responses include the Global-only fields `chatReportUsedToday` and `roomIds`.
- Upgrading from the earlier two-RPC Global bundle changes field names and file names, so a
  running instance cannot hot-reload across that change; restart with the new bundle.

## Master data

The Master pipeline (`master_directory`, `master_update`, `master-import`, the plaintext
registry, `master_sync`, `master_notify`, `master_git` and `master_database`, plus the standalone
`registry-serve`) works for `jp`, `hk`, `en` and `kr`. `cn` is rejected everywhere. Global
clients fetch Master data the same way as JP: `{CdnRoot}/master/{version}/MasterManifest.json`
and `{CdnRoot}/master/{version}/{name}.bin`, with the same manifest shape, encryption and
compression (the same `SIRIUS_MASTER_KEY_HEX`/`SIRIUS_MASTER_IV_HEX` values). `version` is field 1
of the Global VersionResponse. Its field 2 `resourceVersion` is recorded as the snapshot's asset
version. JP takes that value from the `x-asset-version` header instead.

`default_cdn_root` must be one URL from the region's server-list `cdn_root` (the `|`-separated
list holds alternate lines). The shipped examples use the first line of each region:

| Region | Master CDN root (first server-list line) |
| --- | --- |
| `hk` | `https://l14-prod-hk-patch-sirius.gamerfusiontech.com/prod/hk_27f3c91e8b62d6056c7a19f2e83b6d10` |
| `en` | `https://l14-prod-sg-patch-sirius.bilibiligame.net/prod/en_3e8a72c5f1d9066b9a37c2e85f619db0` |
| `kr` | `https://l14-prod-sg-patch-sirius.bilibiligame.net/prod/kr_461b4e9a7c2385f0e2d966a1b73c8f52` |

### CDN authorization

`master_update.cdn_authorization` selects how Master downloads authenticate:

- `basic` (default): HTTP Basic with `username_env` and the credential that `cdn_credential_env`
  references for the effective CDN root. JP always uses this, and a JP profile must reference a
  credential for its `default_cdn_root`.
- `none`: no Authorization header is sent. This is accepted only for `hk`, `en` and `kr`, only
  without `username_env`, and only when `cdn_credential_env` has **no** entry for
  `default_cdn_root`. Downloads use exactly the configured root. If the game announces a
  different CDN root, the update fails and the installed snapshot is kept.

The Global Master CDNs were verified to serve Master data without authentication. No Global CDN
credential is verified or shipped; do not reuse the JP credential. A Global profile may omit
`cdn_credential_env` entries (`cdn_credential_env: {}`). Resource snapshots are configured
separately; see [Resource snapshots](#resource-snapshots).

```yaml
region: en
master_directory: ./data/en/master
master_update:
  cdn_authorization: none
  key_hex_env: SIRIUS_MASTER_KEY_HEX
  iv_hex_env: SIRIUS_MASTER_IV_HEX
  interval_seconds: 300
default_cdn_root: https://l14-prod-sg-patch-sirius.bilibiligame.net/prod/en_3e8a72c5f1d9066b9a37c2e85f619db0
cdn_credential_env: {}
```

## Resource snapshots

`GET /internal/v1/resources/snapshot` tells the asset updater which catalog to download.

**JP** is unchanged: the snapshot comes from the `x-asset-version` header and the `x-sirius-*`
CDN headers, uses schema version 2 and carries no layout fields.

**HK/EN/KR** snapshots are opt-in with `resource_snapshot` and use schema version 3. Global
responses carry `x-asset-version: unknown`, so the header is ignored there. The proxy builds the
snapshot from:

- `resource_version`: field 2 `resourceVersion` of the latest successful VERSION response
  (for example `1.0.0.104`). An empty, unsafe or `unknown` value produces no snapshot.
- `platform_hash`: the trimmed body of `{default_cdn_root}/asset/{platform}/catalog_{resource_version}.hash`,
  the base (Japanese) catalog's version token. It must be exactly 32 hex digits, and it is
  stored in lower case.
- `effective_cdn_root`: exactly `default_cdn_root`. A server-announced different root is
  never followed.

Schema 3 adds four explicit fields. Consumers must recompute them from the layout and reject
any difference:

| Field | Global value |
| --- | --- |
| `catalog_layout` | `global` |
| `catalog_url` | `{root}/asset/{platform}/catalog_{resource_version}.bin` |
| `bundle_base_url` | `{root}/asset/{platform}` (no version directory; bundle names are content-addressed) |
| `cdn_authorization` | `none` (with an empty `credential_ref`) or `basic` (with the credential's environment reference) |

The `.hash` request is one bounded GET: at most 256 bytes, no redirects, and the timeouts,
attempts and optional proxy of `resource_snapshot.network`. The same fields as
[`master_update.network`](MASTER_NETWORK.md) apply. The result, including a failure, is reused
for the same root and resource version for `catalog_hash_ttl_seconds` (default 60, 10–300).
Builds are serialized, so repeated snapshot reads or dispatch polls never multiply CDN
requests. No request is made until a snapshot is read or the [asset dispatch](ASSET_DISPATCH.md)
worker refreshes one.

`resource_snapshot.cdn_authorization` follows the rules of `master_update.cdn_authorization`:

- `none`: no `username_env`, and no `cdn_credential_env` entry for `default_cdn_root`.
- `basic`: needs `username_env` and a credential reference for `default_cdn_root`.

The Global resource CDN was verified to serve `.hash`, catalogs and bundles without
authorization. `resource_snapshot` is rejected for JP.

```yaml
region: hk
resource_snapshot:
  cdn_authorization: none
  catalog_hash_ttl_seconds: 60   # optional
  network: {request_timeout_ms: 10000, attempts: 2}   # optional
default_cdn_root: https://l14-prod-hk-patch-sirius.gamerfusiontech.com/prod/hk_27f3c91e8b62d6056c7a19f2e83b6d10
cdn_credential_env: {}
```

Localized catalogs (`catalog_{version}_{locale}.bin`) are chosen per updater profile. The
snapshot always describes the base catalog. Dispatch identities already include the profile,
so give each locale its own updater profile.

Upgrade note: asset updaters older than 1.2.1 reject schema-3 snapshots, which carry unknown
fields. Such updaters could not download Global assets anyway, so they fail closed. JP
snapshots stay at schema 2, so JP consumers can be upgraded in either order.

### Region identity

Snapshot receipts record `region`. Receipts without one (every snapshot written before 1.2.1)
are JP snapshots, so existing JP directories, content hashes and Git trees are unchanged. A
directory holds one region's history: reads, registry routes, history, Git and database
publication, sync and new installations all refuse a snapshot recorded for another region.
`master-import IN OUT --region hk|en|kr` records a Global import; without `--region` it records
JP as before. Content identity, update hints and notifications carry the scope's region.
Regional routes use `/api/v1/{region}/master-data/...` and `/internal/v1/{region}/...`. Git
commit messages are `Sirius Master <region> <version>`. In a multi-region deployment, each
region needs its own `master_directory`, `master_git.state_directory`, Git remote and Git token.
See [the multi-region publisher example](examples/master-publisher.yaml) and
[Master snapshot publication](MASTER_REGISTRY.md).

### The `hk` identifier

Before 1.2.1 this region was named `tw`. Since 1.2.1 the canonical name is `hk` everywhere Sirius
writes or serves a region: `/api/v1/regions`, `/api/v1/hk` and `/internal/v1/hk` routes
(including `regional_paths`), scopes, manifests and content identity, receipts, Git commit
messages (`Sirius Master hk …`), update hints and notifications, sync, asset updater jobs, errors
and logs. The area ID (2), endpoints and CDN roots are unchanged.

`tw` remains a **deprecated input alias**, accepted only in configuration and CLI arguments: a
profile's `region`, a multi-region `regions` map key, the `scope.region` of `registry-serve` and
`master-db-*` configurations, and `master-import --region`. It is read as `hk`, and the process
logs one `deprecated_region_alias` warning at startup. A `regions` map with both keys is
rejected. The alias will be removed in a future release; configure `hk`. Paths and wire formats
never accept it: `/api/v1/tw/...` and `/internal/v1/tw/...` return 404, and peer queries, update
hints, published manifests and asset updater replies naming it are rejected. Upgrade peers,
registries and consumers of this region together.

Data written by earlier builds:

- Snapshot receipts recorded as `tw` are read as `hk`, so an existing `master_directory` keeps
  working. New receipts record `hk`; existing receipts are not rewritten. Because content identity
  includes the scope, the snapshot's `content_sha256` is now computed for `hk`, so consumers and
  Git publication see one new content identity for unchanged tables.
- A `master_git.state_directory` whose ownership marker records `tw` is accepted and its marker is
  rewritten to `hk`. The next publication commits the new identity as `Sirius Master hk …`; earlier
  commit messages stay as published.
- Not migrated: PostgreSQL Master rows keyed by the old scope (the `hk` scope starts empty; run
  `master-db-migrate` to rebuild its history from the snapshot directory), `client_auth` grant rows
  (`UPDATE sirius_api_user_regions SET region = 'hk' WHERE region = 'tw'`), asset dispatch state
  containing jobs for the old name (it fails to open; use a new `state_directory` after the
  pending jobs finish), and response cache entries (they go cold).

## Asset updater

Configure the same region, platform, client version and protocol version as the proxy.
`protocol_version` defaults to 1.0.3 for JP and 1.0.1 for HK/EN/KR; it can be pinned explicitly
when deploying a new verified bundle. `cdn_roots` matches the entire HTTPS base URL, including
its path. Username/password environment references belong only to that configured base URL.
Redirects remain disabled and unknown roots/references are rejected before CDN requests.

New snapshots use schema version 2 and require explicit region identity. A regionless schema-1
snapshot is accepted only for legacy JP/iOS/protocol-1.0.3. A mismatched or missing schema-2
region, platform, environment, client or protocol version fails before any CDN request.
Publication names contain region and platform; receipts and export summaries retain identity.
Cache keys additionally include region, environment and platform, even for shared CDN URLs.
Old ciphertext caches are not deleted but use a different namespace and may be downloaded again.

Global asset downloads use the schema-3 snapshots described in
[Resource snapshots](#resource-snapshots). A Global updater never accepts a schema-2 (JP layout)
snapshot, and JP never accepts a Global layout. See the updater's `docs/REGIONS.md` for
`catalog_locale`, the `dummy.net` bundle placeholder and anonymous CDN roots. Keep separate
output, cache and export directories for each region. Real credentials and keys are never
shipped; do not reuse JP credentials for Global.

## Upgrade from v1.0.0

Upgrade the paired proxy and updater together: the old updater cannot consume schema-2
snapshots. Existing JP configuration remains valid with omitted region/platform. Existing
schema-1 JP receipts remain readable for offline verification/export. Export summary schema 2
adds region and platform. Keep published outputs immutable; directory names are opaque and
must not be parsed as the previous `catalog-UUID` naming convention.

`cn` has no default API/CDN, invented area ID, copied Global protocol, accounts or fallback to JP.
Enabling it later requires verified endpoints, login/protobuf contracts and resource behavior.
