# Changelog

## 1.3.3

- Global accounts: the regions of a deployment share one SDK session per identity.
  - Before, each region ran its own `cache.login` for the same identity, and again after every
    restart: three SDK logins within a second per restart. On 2026-10-01 production identities
    were blocked by the SDK's risk control (code 900200).
  - Now `cache.login` runs once per identity and the other regions reuse the result while the
    `id_token` is valid (JWT `exp` minus 5 minutes) and no `TOKEN_*` signal marked it stale.
- An SDK refusal (`SDK_REFUSED`, and now also `SDK_CAPTCHA`) is shared too. Every region
  disables the account without sending anything, and the identity is not retried for
  `global_login.sdk_refusal_retry_seconds` (default 86400; 3600..2592000), across account
  reloads too.
- A request that waited for another region's `cache.login` does not repeat it when it failed;
  it answers 503 `upstream_unavailable`. A `TOKEN_*` signal about an `id_token` that another
  region has already replaced no longer invalidates the new one.
  - Before, a reload retried at once. A reload still starts a new session when the identity
    file holds another access key.
- New optional `global_login.state_directory` keeps each identity's SDK session and refusal
  across restarts.
  - Files are private and atomic, and never hold the access key. A restart within the `id_token`
    lifetime sends no SDK request; a restart under a refusal sends nothing.
  - `global-account verify` and the service share it safely. They serialize `cache.login` with a
    lock file and keep each other's newer refusal.
  - Every region of a deployment must name the same directory.
- Account status adds `sdk_session` (`none`, `valid`, `stale`, `expired`, `refused`).
  `global-account verify` reports `sdk_cache_login: reused` when it reused a persisted session.

## 1.3.2

- JP protocol 1.0.4 is the default (`protocol/sirius/1.0.4`, native codecs). It is the same
  ten-RPC subset as 1.0.3, extracted from the official 1.0.4 client, and only adds definitions:
  `Announcement.platform` (`AnnouncementPlatform`), the costume fields of `PlayerData` and
  `Notification` (new `entity/character_costume.proto`) and
  `ResourceType.RESOURCE_TYPE_CHARACTER_UNLOCKED_COSTUME`. Before, a 1.3.1 service dropped these
  fields. The JP examples use `client_version: 1.0.4`, which the JP server now requires.
  `protocol/sirius/1.0.3` stays for a restart-based rollback; a running service on 1.0.4 cannot
  hot-reload back to 1.0.3 (that would remove fields).
- The reduced test baseline `tests/fixtures/proxy-descriptors.pb` is regenerated for 1.0.4 from
  the client's embedded descriptors with the same reduction that reproduces the 1.0.3 baseline
  byte for byte.
- JP resource snapshots now report `protocol_version` and `client_version` 1.0.4. A JP asset
  updater profile must set both to 1.0.4 (the updater's JP default is still 1.0.3).
- Live-verified with a JP 1.0.4 guest account (2026-10-01): Whoami, own player data, profile,
  event/song/challenge rankings, and the JP `PLAYER_NOT_FOUND` → 404 mapping from 1.3.0.

## 1.3.1

- Global accounts: an `SDK_REFUSED` login now reports the SDK's numeric refusal code as
  `last_sdk_code` in the account status and as `sdk_code` in the warning, so an operator can tell
  why the SDK refused an identity. Previously only `SDK_REFUSED` was visible.

## 1.3.0

The remaining batches of the second comparison with the original (Haruki-Sekai-API `9a53714`), adapted
rather than copied: upstream robustness, Master publication and storage operations, smaller
improvements and hardening. Includes everything in 1.2.4.

- Node routing: authenticated public reads (profile, event ranking and deck, music and challenge
  ranking) now fail over to the next node when a peer answers HTTP 400, 401, 404, 405, 413, 415
  or 422, which executors 1.2.0 and later send only before dispatch (wrong peer token, route or
  region not served, rolling-upgrade contract skew). Such answers still count as target
  failures and cool the node down; other statuses stay terminal. No wire, configuration or
  error-code change.
- `node_target_failed`, `node_cooldown_started` and `node_probe_failed` carry the peer's HTTP
  `status` when it answered one.
- Development: the ignored `perf_stages` test prints per-stage latency (min, median, p90) for
  native versus dynamic decoding and encoding on every JP and Global route, the Master import
  stages (SHA-256, Rijndael, gunzip, JSON parse and validation, snapshot install), Master and
  registry reads, and memory response-cache serialization. It runs only on request
  (`cargo test --release --locked perf_stages -- --ignored --nocapture --test-threads=1`),
  asserts no timings and changes no runtime behavior or configuration.
- Master table reads (`/master-data/tables/{name}`, `snapshots/{id}/tables/{name}/{hash}`,
  `database/by-hash/{hash}/tables/{name}` and the standalone registry's table route) are admitted
  through 16 process-wide permits, held until the connection has taken the last 64 KiB piece of
  a 200 body (a client that does not read keeps its permit). After waiting 5 s in FIFO order
  they answer 503 `master_unavailable`; 304s and errors release at once, Content-Length is kept,
  and manifests, history and bundles are unaffected. No configuration.
- Master tables are validated as JSON without building an in-memory document (the same
  accepted and rejected inputs, recursion limit, UTF-8 and number checks), which lowers
  per-read memory and CPU on registry, pinned, bundle, import and database table reads.
- Unchanged CDN and sync polls no longer reparse tables whose byte length and SHA-256 match the
  snapshot index (`tables.json`), which is written only after the tables were validated;
  legacy 1.1 snapshots without an index are still parsed, and serving reads still validate.
- New Master Git commits end with a second paragraph holding one trailer,
  `Sirius-Content-SHA256: <content_sha256>`, the scoped content identity served at
  `by-hash/{content_sha256}/manifest`. The subject, tree, manifest, receipt and adoption are
  unchanged, existing commits are not rewritten, and identical content still reuses the previous
  commit, so a reused commit may carry another identity's trailer or none. The trailer is
  informational: Sirius never reads it, and it is not proof of content. Tooling that compares
  the full message (`%B`) byte-for-byte with 1.2.x sees the extra paragraph; `%s` is unaffected.
- Optional negotiated response compression: root `http_compression: {enabled: true}` (single
  file, multi-region root only, standalone registry) encodes status 200 `application/json`
  bodies of at least 1024 bytes with gzip or zstd (fastest level) on the public API and the
  registry's public Master routes, with a weak ETag and `Vary: Accept-Encoding`. `/health`,
  internal, peer and asset dispatch admin routes, error bodies, 304s and bundles stay identity;
  request bodies are never decoded. Absent or disabled, responses are byte-identical to 1.2.x.
  Outbound SDK, CDN, peer, dispatch and sync requests still send no `Accept-Encoding`.
- `json_client_errors` now also drops `Content-Encoding` when it replaces a non-JSON error body.
- `GET /api/v1/master-data` and `/master-data/tables/{name}` (and their regional paths) now
  return a strong content ETag (the SHA-256 of the exact bytes; for a table it equals the pinned
  table ETag) with `Cache-Control: private, no-cache`, and answer `If-None-Match` with an empty
  304 only after the region and index integrity checks, so corruption still answers 503.
  Additive: a client that sends `If-None-Match: *` or a matching validator now gets 304 where
  it used to get 200.
- JP `profile` lookups answering gRPC 2 or 7 with `x-sirius-error-code: PLAYER_NOT_FOUND` now
  return 404 `not_found` and leave the account alone, as Global has since 1.2.2. They used to
  answer 502, and on gRPC 7 the JP account was disabled. The evidence is static (the iOS 1.0.3
  client), not a live test; JP `event_deck`, other statuses and `PLAYER_NOT_EXISTS` are
  unchanged. A JP executor now emits the terminal peer kind `not_found`, which callers parse
  since 1.2.2; no wire-format change.
- `/health` (single-profile, multi-region and registry servers) adds `uptime_secs`: whole
  seconds since the process entered `main()`, on a monotonic clock, reset on restart. The key is
  additive and `/health` stays liveness only, with no readiness, account or upstream data.
- Identical concurrent Version, server-list and announcement reads share one upstream call
  (and one peer POST under node routing), even with the response cache disabled; the outcome,
  errors and timeouts included, answers every joined request, each within its own deadline.
  Rankings join only with `upstream.coalesce_public_reads: true`. Profiles, decks, private data
  and named-account calls never do. API and peer wire format are unchanged.
- The per-region lock that serialized every call without an account under `session_lock: true`
  is replaced by `upstream.anonymous_max_inflight` (default 4, capped by `max_inflight`), so a
  slow Version no longer holds back announcement reads. This changes live traffic: up to four
  anonymous RPCs now overlap. `anonymous_max_inflight: 1` restores the 1.2.x serialization.
- Path outages no longer cool accounts. Transport, protocol, deadline and bare gRPC 14 failures
  stay an account's only while that account alone fails; once a second account or an anonymous
  call fails in the same run, the run's charges are withdrawn and, at
  `account_pool.failure_threshold`, the region's path opens: new calls get 503
  `upstream_unavailable` (instead of 504/502 per request) before any upstream contact or Global
  login, peers answer `unavailable_before_dispatch`, and one probe per min(`cooldown_seconds`,
  5 s) closes it again. Cache hits are still served.
- Global SDK transport and malformed-response failures no longer cool the account; they open a
  separate SDK path the same way. `GET /internal/v1/accounts` adds `path` and, on Global,
  `sdk_path`. No configuration or wire change.
- A dead or blackholed game connection is detected by HTTP/2 PING within about interval +
  acknowledgement timeout (15 s by default) while a call is open on it: the call fails as 502
  `upstream_transport` instead of 504 `upstream_timeout` at the deadline, and the next call
  reconnects. Idle connections are not pinged. New keys `upstream.http2_keepalive_interval_ms`
  (0 disables) and `upstream.http2_keepalive_timeout_ms`; profiles with `timeout_ms` below 4000
  keep the 1.2.x behavior unless a key is set. API and peer wire format are unchanged.
- `x-master-version` no longer stays at the first observed value for the life of the process: a
  call first refreshes it by a Version call once it is older than the new
  `upstream.version_max_age_seconds` (600, 60..86400; other calls do not wait, the refresh uses at
  most half the remaining deadline, a failure keeps the old header for 30 s), and after a response
  carried `MASTER_VERSION_MISMATCH`, in which case the next call waits for one Version call sent
  without the rejected header. The failed call is not replayed.
- `MASTER_VERSION_MISMATCH` and `CLIENT_UPDATE_REQUIRED` no longer count against JP or Global
  accounts, whatever their gRPC status (7/16 used to disable the account; on Global 16 dropped the
  session), including on the JP identity check before private data, and are never retried as
  anonymous reads. Both codes are in the JP 1.0.3 and Global 1.0.1 clients; their gRPC status and
  whether the game enforces a fresh version are unverified, and the design depends on neither.
  API and peer wire format are unchanged.
- Response-cache hits (fresh, or stale inside `stale_while_revalidate_ms`) no longer queue behind
  `upstream.max_inflight`, the protocol reload barrier or account selection: they take no permit,
  do not count in `active_calls` and never touch account or path health. Misses, maintenance,
  calls that must bootstrap the Master version and peer queries with another schema hash are
  admitted as before.
- With every account cooling or disabled, a region with a stale window now answers a query from
  an entry still retained for any pool account instead of 503 `account_unavailable` (peers:
  instead of `unavailable_before_dispatch`), without refreshing it. With a window of 0 nothing
  changes. No configuration or wire change.
- New one-shot `master-git-adopt GIT_STATE_DIRECTORY REMOTE_URL` recovers Master Git
  publication stuck on `remote_history` after lost local state or a manual remote commit such
  as a README: it fast-forwards the local managed branch to the remote head when the newest
  `Sirius Master` commit among the last 64 is a publication of the profile's scope and layout,
  and refuses divergence (`RemoteChanged`) or unrecognized history (new static error
  `NotAdoptable`). It never pushes; the next publication continues on the adopted head, and
  manually added files leave the next tree. Publication, configuration and the worker are
  unchanged (the worker never adopts).
- `master_git.timeout_seconds` (default 120, range 10–600) sets the single time budget for all
  Git commands of one publication or adoption attempt, previously fixed at 120 s.
  `SIRIUS_MASTER_GIT_TIMEOUT_SECONDS` overrides it for one `master-git-commit`, `-push` or
  `-adopt` run. HTTP(S) Git transfers now also abort after 30 s below 1000 bytes/s. The Git
  status endpoint reserves the static error code `timeout_config` for an invalid budget, which
  configuration validation already rejects. Omitting the field keeps 1.2.x behavior.
- PostgreSQL Master history keeps each event's `version`, `resource_version`, `file_count` and
  `total_size`, also after the snapshot is pruned, and now reports its `published_at`. File
  history entries add `resource_version`. All fields are additive; events written before 1.3.0 or
  by a 1.2.x writer report null metadata and are not backfilled.
- The first 1.3 publication or migration adds the history columns once (an exclusive lock on the
  history table for that transaction; the role must own it). 1.2.x readers and writers keep
  working against the upgraded table, and migration receipts written by 1.2.x still replay.
- PostgreSQL Master reads (`/api/v1/master-data/database/*` and registry `backend: postgres`)
  have their own deadline, `connection.read_timeout_seconds` (1–600, default
  min(`timeout_seconds`, 30)), which also sets the read connections' server statement/lock
  timeouts; waiting for a read connection gives up after 5 s. Reads that previously waited up to
  `timeout_seconds` (default 120) now answer 503 `master_unavailable` sooner; set
  `read_timeout_seconds` to keep the old budget. Publication, import and migration keep
  `timeout_seconds`. No circuit breaker or file fallback is added.
- New `GET /internal/v1/asset-dispatch/status` (regional: `/internal/v1/{region}/...`, internal
  bearer) reports the asset dispatch worker's phase (`pending`, `observing`, `reconciling`,
  `idle`, `stopped` with `stop_reason`), the last observation and reconciliation outcomes,
  working-ledger counts including busy retries, and failures by known code (others as `other`).
  It always answers 200 without the 16-command queue, so a busy worker is distinguishable from a
  stopped one where the entry routes answer 503. No configuration is added; existing routes,
  the list's constant `status: "ready"`, the ledger and log events are unchanged.
- Optional file snapshot retention: `master_retention.keep_snapshots` (2..10000, with
  `master_update` or `master_sync`) and registry `owner.retention` keep the newest installations
  along the committed chain and remove older ones after each settled update or sync pass (also
  unchanged ones; at most 64 per pass, oldest first). The boundary is recorded in
  `retention.json`; staging, orphan and legacy directories are never touched. Off by default:
  nothing is deleted and no output changes unless configured.
- Master history (and `master-db-migrate` receipts) gain `retention_boundary`, present only when
  true, and stop there; pruned snapshots, tables, content identities and bundles answer 404.
  Update and sync results gain `pruned_snapshots` when retention is configured. Downgrade
  warning: after a pass has pruned, 1.2.x history, by-hash lookup and `master-db-migrate` fail on
  the missing directories (current reads, updates and sync are unaffected).
- The Docker builder compiles dependencies in a separate cargo-chef 0.1.78 layer (installed
  pinned and `--locked`), so source-only changes, version bumps and other `VERSION` build
  arguments skip the dependency compile. The runtime stage, image contents and paths are
  unchanged. The build context is now a whitelist (Cargo files, `build.rs`, `src`, `protocol`,
  `LICENSE*`), and the Docker workflow also runs for pull requests that change `build.rs`.

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
