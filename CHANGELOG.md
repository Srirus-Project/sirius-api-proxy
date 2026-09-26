# Changelog

## 1.2.1 (unreleased)

- Global (HK/EN/KR) resource snapshots, opt-in with `resource_snapshot`. `resource_version` is
  the Global VersionResponse field 2 `resourceVersion`; `x-asset-version: unknown` is ignored.
  `platform_hash` is the base catalog's `{default_cdn_root}/asset/{platform}/catalog_{version}.hash`,
  fetched with one bounded GET (256 bytes, no redirects). The result, including failures, is
  memoized per root and version for `catalog_hash_ttl_seconds`.
  `resource_snapshot.cdn_authorization` is `none` (verified for the Global resource CDN) or
  `basic`, validated like `master_update.cdn_authorization`. It is rejected for JP. A
  server-announced different root is never followed.
- Global snapshots use schema 3 with explicit `catalog_layout` (`global`), `catalog_url`,
  `bundle_base_url` and `cdn_authorization`; anonymous snapshots have an empty
  `credential_ref`. JP snapshots stay schema 2 without these fields.
- Upgrade note: updaters before 1.2.1 reject schema-3 snapshots. Upgrade the updater before
  enabling `resource_snapshot`. JP pairs are unaffected.
- The Traditional Chinese region is renamed from `tw` to `hk`, the identifier the game uses (CDN
  `/prod/hk_…`, `l12-prod-hk-…` endpoints, server list). `Region::Hk` serializes as `hk` in
  `/api/v1/regions`, `/api/v1/hk` and `/internal/v1/hk` routes (including `regional_paths`),
  scopes, manifests, content identity, receipts, Git commit messages, hints, notifications, sync,
  asset updater jobs, errors and logs. The area ID stays 2. `docs/examples/tw.yaml` is now
  `docs/examples/hk.yaml`.
- Deprecated: `tw` is accepted only as a configuration/CLI alias of `hk` (`region`, multi-region
  map keys, `scope.region` of `registry-serve`/`master-db-*`, `master-import --region`), with one
  `deprecated_region_alias` warning at startup. A `regions` map with both keys is rejected. Paths
  and wire formats never accept it (`/api/v1/tw` is 404).
- Upgrade note: snapshot receipts and Git state markers recorded as `tw` are read as `hk` (the
  marker is rewritten); `content_sha256` of such snapshots changes with the scope. PostgreSQL
  Master rows, `client_auth` grant rows and asset dispatch state keyed by the old name are not
  migrated. Upgrade peers, registries and consumers of this region together. See
  `docs/REGIONS.md#the-hk-identifier`.
- Master Git publication gains `master_git.layout: indented_root`: every table re-indented
  (whitespace only, tokens preserved) at the repository root plus `version.json` with
  `dataVersion` and `assetVersion`. `native` remains the default and its tree is unchanged.
- `master_git.branch` selects the published branch (default `master-data`), for example `main`.
- Master snapshots record the asset version from the same game VERSION response as the Master
  version (JP: the `x-asset-version` header). Manifests expose it as optional `resource_version`,
  excluded from `content_sha256`; owner-to-consumer sync carries it. A snapshot without it is
  reinstalled once when the game reports one for the same Master version.
  `master-import ... --resource-version VERSION` records it for local imports.
- `indented_root` publication without recorded provenance fails with
  `asset_version_unavailable`, leaving refs unchanged.
- Upgrade note: consumers reject unknown manifest fields, so upgrade consumers and registries
  before owners that record provenance.
- The Master pipeline is enabled for HK, EN and KR: `master_directory`, `master_update`,
  `master-import`, registry routes, `master_sync`, `master_notify`, `master_git`,
  `master_database` and `registry-serve`. CN stays rejected. Global downloads use the same
  `{CdnRoot}/master/{version}/…` layout and Master key/IV as JP. The asset version comes from the
  Global VersionResponse `resourceVersion`.
- `master_update.cdn_authorization`: `basic` (default, required for JP) or `none`. `none` sends no
  Authorization header. It is accepted only for HK/EN/KR, without `username_env`, and when
  `default_cdn_root` has no credential reference. It never follows a server-announced root. Global
  profiles no longer need a CDN credential reference. `master_update.username_env` is required
  only for `basic`.
- Snapshot receipts and the Master status record `region`. Receipts without one are legacy JP
  snapshots, so JP content hashes and Git trees are unchanged. Reads, registry and history,
  sync, Git and database publication, and new installations reject a snapshot recorded for
  another region. `master-import ... --region hk|en|kr` records a Global import.
- The standalone registry's `regional_paths` uses the scope's region (`/api/v1/{region}`,
  `/internal/v1/{region}`) instead of always `/jp`. Multi-region deployments reject shared Master
  directories, shared Git state directories and shared Git remotes across regions.
  `GET /api/v1/regions` adds a `master_data` flag.
- `docs/examples/{hk,en,kr}.yaml` and the multi-region example use the verified server-list CDN
  roots instead of placeholders, and ship commented Master blocks.
  `docs/examples/master-publisher.yaml` shows JP/HK/EN/KR `master_update` plus `indented_root`
  Git publication.

## 1.2.0

Restores the reusable service capabilities of the original Haruki API for Sirius. Sekai-specific
models, Ent code, CP/Nuverse login and unverified Global login are not restored; see
`docs/CONFIG_AUDIT.md` for the field-by-field audit and `docs/RESTORATION_1_2.md` for the ledger.

- Multi-region deployments with per-region protocol, credentials and isolated state; v1.1
  single-region configuration remains valid and CN stays reserved.
- Account pool with per-account session locking, health/cooldown and live credential reload.
- Scoped response cache with optional Redis backend and stale-while-revalidate; private data
  never enters public cache entries.
- Upstream proxies, deadlines, bounded retries, listener TLS, application and access logs,
  trusted forwarding and remote node routing with priorities and failover.
- Master registry: verified manifests, pinned tables, history, content-hash lookup, snapshot
  tar bundles, owner/consumer synchronization and update notifications.
- Optional PostgreSQL Master mirror (exact JSON bytes plus JSONB), history migration and
  configurable read pool; optional Git publication with signing, proxies and background retry,
  including Windows process-tree containment.
- Standalone `registry-serve` without game configuration: file or PostgreSQL backends,
  owner pull, local publication and outbound consumer notifications.
- Optional per-client API tokens (`client_auth`) with PostgreSQL users and region grants,
  failing closed unlike the original open mode.
- Durable asset-updater job dispatch with idempotent submission and completion tracking.
- All configuration structures reject unknown fields; every shipped example is parse-tested.

## 1.1.0

- Add explicit JP/TW/EN/KR identities and reserve CN without enabling unverified networking.
- Separate region, platform and environment; reject mismatched known service endpoints.
- Document capability boundaries and paired-service upgrade requirements in `docs/REGIONS.md`.
- Generate independent native JP and Global protobuf codecs, retaining compatible dynamic reload.
- Add authenticated region capability and Global server-discovery routes; expose resource version observations.
- Emit region-bearing schema-2 snapshots and select the requested platform hash.
- Refuse Global operations outside the verified discovery/version protocol rather than using JP messages.


## 1.0.0

- Add a default-on `session_lock` configuration switch for upstream RPC serialization.
- Initial public-release candidate for BanG Dream! Our Notes.
- Replace the pre-release Viola codename with Sirius configuration filenames,
  SIRIUS_* environment variables and container user names. Old names are not aliases.
- Retain the appropriate Haruki derived-from attribution and MIT notices.
- Include runtime files, configuration examples, documentation and licenses in release archives.
- Keep real credentials, downloaded content and private research out of public artifacts.

See README.md for supported features, validated scope and current limitations.
