# Region support

Region is distinct from deployment environment and UI language. Deploy separate instances or
use the [multi-region service](MULTI_REGION.md), with isolated client state and regional routes.
Changing region requires a restart; hot reload changes compatible protobuf definitions within
one protocol family and cannot switch regions or account identity.

| Region | Game selection | Area ID | Default platform | Protocol family | Current capability |
| --- | --- | --- | --- | --- | --- |
| `jp` | Japan | Not inferred | `iOS` | JP 1.0.3 | Existing JP proxy, verified download/export pipeline and Master data |
| `tw` | TW/HK/MO | 2 | `Android` | Global 1.0.1 | Server discovery, anonymous version query and Master data |
| `en` | EN Region | 3 | `Android` | Global 1.0.1 | Server discovery, anonymous version query and Master data |
| `kr` | Korea | 4 | `Android` | Global 1.0.1 | Server discovery, anonymous version query and Master data |
| `cn` | Reserved | Unknown | Not operational | Not supplied | Configuration is recognized but startup/check rejects it |

`global` is not a region. TW, EN and KR have distinct API roots and Master versions. EN and KR
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

Global player/profile/ranking/announcement/account operations return HTTP 501 without sending
a request. The Global bundle deliberately includes only the two verified RPCs and their
necessary types. JP authentication, account registration and Master decryption are not assumed
to work on Global; no SDK registration/login implementation is included in this release.
Global Master data is supported as described below; nothing else about Global accounts is.

## Master data

The Master pipeline (`master_directory`, `master_update`, `master-import`, the plaintext
registry, `master_sync`, `master_notify`, `master_git` and `master_database`, plus the standalone
`registry-serve`) works for `jp`, `tw`, `en` and `kr`. `cn` is rejected everywhere. Global
clients fetch Master data the same way as JP: `{CdnRoot}/master/{version}/MasterManifest.json`
and `{CdnRoot}/master/{version}/{name}.bin`, with the same manifest shape, encryption and
compression (the same `SIRIUS_MASTER_KEY_HEX`/`SIRIUS_MASTER_IV_HEX` values). `version` is field 1
of the Global VersionResponse. Its field 2 `resourceVersion` is recorded as the snapshot's asset
version. JP takes that value from the `x-asset-version` header instead.

`default_cdn_root` must be one URL from the region's server-list `cdn_root` (the `|`-separated
list holds alternate lines). The shipped examples use the first line of each region:

| Region | Master CDN root (first server-list line) |
| --- | --- |
| `tw` | `https://l14-prod-hk-patch-sirius.gamerfusiontech.com/prod/hk_27f3c91e8b62d6056c7a19f2e83b6d10` |
| `en` | `https://l14-prod-sg-patch-sirius.bilibiligame.net/prod/en_3e8a72c5f1d9066b9a37c2e85f619db0` |
| `kr` | `https://l14-prod-sg-patch-sirius.bilibiligame.net/prod/kr_461b4e9a7c2385f0e2d966a1b73c8f52` |

### CDN authorization

`master_update.cdn_authorization` selects how Master downloads authenticate:

- `basic` (default): HTTP Basic with `username_env` and the credential that `cdn_credential_env`
  references for the effective CDN root. JP always uses this, and a JP profile must reference a
  credential for its `default_cdn_root`.
- `none`: no Authorization header is sent. This is accepted only for `tw`, `en` and `kr`, only
  without `username_env`, and only when `cdn_credential_env` has **no** entry for
  `default_cdn_root`. Downloads use exactly the configured root. If the game announces a
  different CDN root, the update fails and the installed snapshot is kept.

The Global Master CDNs were verified to serve Master data without authentication. No Global CDN
credential is verified or shipped; do not reuse the JP credential. A Global profile may omit
`cdn_credential_env` entries (`cdn_credential_env: {}`). Global resource snapshots then stay
unavailable, as they already were.

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

### Region identity

Snapshot receipts record `region`. Receipts without one (every snapshot written before 1.2.1)
are JP snapshots, so existing JP directories, content hashes and Git trees are unchanged. A
directory holds one region's history: reads, registry routes, history, Git and database
publication, sync and new installations all refuse a snapshot recorded for another region.
`master-import IN OUT --region tw|en|kr` records a Global import; without `--region` it records
JP as before. Content identity, update hints and notifications carry the scope's region.
Regional routes use `/api/v1/{region}/master-data/...` and `/internal/v1/{region}/...`. Git
commit messages are `Sirius Master <region> <version>`. In a multi-region deployment, each
region needs its own `master_directory`, `master_git.state_directory`, Git remote and Git token.
See [the multi-region publisher example](examples/master-publisher.yaml) and
[Master snapshot publication](MASTER_REGISTRY.md).

## Asset updater

Configure the same region, platform, client version and protocol version as the proxy.
`protocol_version` defaults to 1.0.3 for JP and 1.0.1 for TW/EN/KR; it can be pinned explicitly
when deploying a new verified bundle. `cdn_roots` matches the entire HTTPS base URL, including
its path. Username/password environment references belong only to that configured base URL.
Redirects remain disabled and unknown roots/references are rejected before CDN requests.

New snapshots use schema version 2 and require explicit region identity. A regionless schema-1
snapshot is accepted only for legacy JP/iOS/protocol-1.0.3. A mismatched or missing schema-2
region, platform, environment, client or protocol version fails before any CDN request.
Publication names contain region and platform; receipts and export summaries retain identity.
Cache keys additionally include region, environment and platform, even for shared CDN URLs.
Old ciphertext caches are not deleted but use a different namespace and may be downloaded again.

Global transport supports HTTPS CDN prefixes and Android paths, but end-to-end Global asset
acquisition/decryption has not been verified. A successful version query does not imply a
ready resource snapshot: `x-asset-version: unknown` or a missing Android hash produces no ready
snapshot. The updater refuses to manufacture hashes or substitute an iOS/JP snapshot. Keep
separate output, cache and export directories for each region. Real credentials and keys are
never shipped; do not reuse JP credentials for Global.

## Upgrade from v1.0.0

Upgrade the paired proxy and updater together: the old updater cannot consume schema-2
snapshots. Existing JP configuration remains valid with omitted region/platform. Existing
schema-1 JP receipts remain readable for offline verification/export. Export summary schema 2
adds region and platform. Keep published outputs immutable; directory names are opaque and
must not be parsed as the previous `catalog-UUID` naming convention.

`cn` has no default API/CDN, invented area ID, copied Global protocol or fallback to JP.
Enabling it later requires verified endpoints, login/protobuf contracts and resource behavior.
