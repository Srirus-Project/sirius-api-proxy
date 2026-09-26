# Changelog

## 1.2.1 (unreleased)

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
