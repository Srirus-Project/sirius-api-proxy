# Changelog

## Unreleased

- Identical concurrent Version, server-list and announcement reads share one upstream call
  (and one peer POST under node routing), even with the response cache disabled; the outcome,
  errors and timeouts included, answers every joined request, each within its own deadline.
  Rankings join only with `upstream.coalesce_public_reads: true`. Profiles, decks, private data
  and named-account calls never do. API and peer wire format are unchanged.
- The per-region lock that serialized every call without an account under `session_lock: true`
  is replaced by `upstream.anonymous_max_inflight` (default 4, capped by `max_inflight`), so a
  slow Version no longer holds back announcement reads. This changes live traffic: up to four
  anonymous RPCs now overlap. `anonymous_max_inflight: 1` restores the 1.2.x serialization.

## 1.2.4

Behavior restored from a second comparison with the original (Haruki-Sekai-API `9a53714`),
adapted rather than copied.

- Maintenance answers 503. A failed game call whose response carries `UNDER_MAINTENANCE` now
  returns HTTP 503 with code `maintenance` (it was 502), is not retried, does not count against
  the account (JP accounts previously cooled down on gRPC 14 with that code) and is not a node
  fault. Peers keep the wire format: a 1.2.4 caller maps the executor's gRPC status plus
  `observation.maintenance` to 503, a 1.2.3 caller still answers 502.
- Error bodies add a stable `code` and, for game failures, `grpc_status`:
  `{"error": "...", "code": "...", "grpc_status": 2}`. The `error` text of `not_found` and
  `account_unavailable` no longer claims "environment not found" / "not configured". Framework
  rejections (malformed path, query or body, unknown route, wrong method, oversized or wrongly
  typed body) are now JSON of the same shape instead of plain text or an empty body, and never
  echo the input. Status codes are unchanged. See the code table in the README.
- Master CDN downloads retry: `master_update.network` defaults to 3 attempts (was 1) with the
  unchanged 250 ms doubling backoff, including when the block is only partly written. The
  `resource_snapshot` `.hash` request keeps one attempt. `attempts: 1` restores the old behavior.
- Asset dispatch classifies the updater's answer to a submission instead of recording every
  failed POST as `submission_ambiguous`: 429/503 (busy) resubmit with the same Idempotency-Key at
  the next cycle until the 10th busy answer (`submission_refused`); 400/404/405/413/415/422, 401/403 and 409
  fail at once as `submission_rejected`, `submission_unauthorized` and `idempotency_conflict`.
  These are not adoptable; fix the configuration and raise `profile_revision`. Transport errors
  and other 5xx remain ambiguous. Dispatch warnings carry `region`, `profile`, `target` (a
  destination digest prefix), `stage` and the HTTP `status`.
- Node routing logs health transitions once each (`node_cooldown_started`, `node_probe_failed`,
  `node_recovered`, `node_unavailable`, `node_router_ready`) with the node name and error code;
  target failures are logged at debug with `failover`.
- The Docker image includes `ssh-keygen`, so `master_git.signing.format: ssh` works in the
  official container (it failed every signed commit before). The container smoke test signs and
  verifies a commit offline.

## 1.2.3

- The Docker image includes `git`. `master_git` commits and pushes Master repositories by running
  the git executable, which the 1.2.0–1.2.2 images did not contain, so Master publication failed
  in the official container. The container smoke test now runs `git --version`.

## 1.2.2

- Global `profile` and `event_deck` lookups: `PLAYER_NOT_FOUND` names the looked-up player, so
  the proxy answers 404 and keeps the account session. 1.2.1 treated it as the account's own
  player: every unknown or other-region profile ID dropped the session (a new PlayerLogin, subject
  to `login_min_interval_seconds`), two in a row disabled the account, and the answer was 502.
  Other codes on these lookups (for example `TOKEN_*`) still apply to the account.
- Peer queries add the terminal failure kind `not_found` (no failover, no node cooldown).
  Pre-1.2.2 routing callers cannot parse it; upgrade callers before or with their executors.
- Live-verified on HK/EN/KR (2026-09-27): announcements, song ranking and profile lookup, now
  reported as `live_verified` in `/api/v1/regions`. A profile ID from another Global region
  answers like an unknown one: the servers do not share players, although one SDK guest identity
  gets its own player on each server.

## 1.2.1

- Global (HK/EN/KR) player accounts. An account references a private SDK guest identity file
  (`global_identity_file`, schema 1, mode 0600) and a `global_login` policy. The first
  authenticated request logs in under the account's session lock: SDK `cache.login` (signed like
  the Android SDK, official `l11`/`l12`/`l13-sdk-login-intl.biligame.net` HTTPS origins only, no
  redirects or retries) then `PlayerLoginService/PlayerLogin` (area 6, channel 2001, brand 5).
  The credential stays in memory. `TOKEN_*`/`PLAYER_NOT_*`/`CONCURRENT_DEVICE` drop the session,
  `BAN_*` and SDK CAPTCHA/refusals disable the account, `AEGIS_*` cools it down; logins are bounded
  by `login_min_interval_seconds` and `max_logins_per_day`. Failed requests are never replayed.
- Global offers the JP operations: profile by profile ID, event ranking and deck, song and
  challenge rankings, announcements, player data and account identity. Authenticated Global calls
  add `x-player-bid` and `x-resource-version`. Whoami is never sent on Global (production rejects
  it); the account identity is the PlayerLogin result. Login and player data are live-verified;
  the other reads are implemented but not yet exercised live. `/api/v1/regions` reports
  per-operation `operations` status and `capability: global_proxy`.
- New one-shot commands: `global-account bootstrap` (creates one SDK guest only with
  `--create-sdk-guest`, refuses to repeat an attempt) and `global-account verify` (one
  `cache.login` and one PlayerLogin for a configured account).
- `protocol/global/1.0.1` is now the client's own descriptor subset for these RPCs plus
  `PlayerLogin` (47 files), without Whoami. `ServerInfo` field 8 is validated by number; the client
  names it `areaID`, and `/api/v1/servers` keeps publishing `areaId`. The previous two-RPC bundle
  cannot be hot-reloaded into the new one, and 1.2.1 refuses to start with it
  (`Error: ProtocolDefinition`): a Global `protocol_directory` outside the release package must be
  replaced with the package's `protocol/global/1.0.1` before restarting.
- Global account status adds `session_state`, `last_login_at`, `logins_24h` and
  `last_error_code`. Response cache account scopes for Global use the account name and SDK uid,
  never the rotating credential. JP accounts and cache keys are unchanged.
- Upgrade note: Global profiles now reject static game credentials (`player_id_env`,
  `credentials_file`); Global authenticated operations answer 503 instead of 501 when no
  account is configured.
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
