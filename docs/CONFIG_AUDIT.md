# Configuration audit against the original

This document is the field-by-field configuration audit that the 1.2.0 release gate in
[RESTORATION_1_2.md](RESTORATION_1_2.md) requires for the API: every configuration field of the
original is accounted for, with no placeholder or silently ignored field, and every
game-specific non-applicability is backed by evidence.

- **Original:** Haruki-Sekai-API `07da6b80e6a59ece89251f4694afe94bea72e131` (MIT), cited as
  `Haruki-Sekai-API@07da6b80:path:line`.
- **Sirius:** this repository at the commit that adds this file. Sirius paths are
  repository-relative, and line numbers refer to that commit (the code is unchanged from
  `1054b40`). A bare `:line` continues the previous path in the same table cell.
- **Method:** every `Deserialize` struct reachable from the original `Config`
  (`Haruki-Sekai-API@07da6b80:src/config.rs:515-538`) and every key in its example file
  (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml`) was traced to its runtime use in the original, then to
  the corresponding Sirius field and the code that reads it. Environment variables read by either
  codebase were checked the same way. Sirius config structs were checked in reverse for fields
  that are parsed and never used.

Status legend:

| Status | Meaning |
| --- | --- |
| REUSED | Same semantics; the name or location may differ. |
| ADAPTED | The capability is present with a different shape, such as an env reference instead of an inline secret, or an interval instead of cron. The difference is stated. |
| DECISION | A generic capability that is intentionally not restored in its original form. The reason is under [Decisions](#decisions). |
| NOT_APPLICABLE | Sekai-specific (CP/Nuverse login, Sekai cipher, Sekai data feeds, app hash). The evidence is given in the row. |
| IGNORED_BY_ORIGINAL | The original parsed or shipped the key but never acted on it. See [the last section](#original-fields-that-were-ignored-by-the-original-itself). |

**Unknown fields are rejected.** Every Sirius configuration struct and tagged enum uses
`#[serde(deny_unknown_fields)]`. This covers `Config` (`src/config.rs:5-7`), `MultiConfig`
(`src/deployment.rs:13-15`), the registry `Config` and `Backend` (`src/registry_service.rs:20-38`)
and every nested section cited below. The only `Deserialize` types without it are unit-variant
enums, such as regions, log levels and signing formats, which already reject unknown values, and
wire or response types that are not configuration. Misspelled keys and keys copied from the
original therefore fail at startup instead of being ignored. The original has no
`deny_unknown_fields` anywhere in `src/`.

## Top level

Original: `Config`, `Haruki-Sekai-API@07da6b80:src/config.rs:515-538`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `proxy` | DECISION | Per-transport explicit proxies: `upstream.proxy_url_env` / `proxy_authorization_env` (game gRPC, `src/config.rs:69-70`, `src/transport.rs:34-52`), `master_update.network.proxy_url_env` / `proxy_authorization_env` (Master CDN, `src/master_update.rs:51-52`, `:106-120`), `master_git.remote.proxy_url_env` (Git, `src/master_git.rs:525`) | Original: one global proxy used by the game client (`Haruki-Sekai-API@07da6b80:src/main.rs:81-82`) and the updater (`Haruki-Sekai-API@07da6b80:src/updater/scheduler.rs:27`), and inherited by Git and music_metas. See [Decisions](#decisions). |
| `jp_sekai_cookie_url` | NOT_APPLICABLE | none | Sekai JP cookie bootstrap (`Haruki-Sekai-API@07da6b80:src/main.rs:80`), used only when `region == Jp && require_cookies` (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:95`). Sirius game RPC is gRPC and has no cookie step (`src/client.rs:777-797`). |
| `git` | ADAPTED | `master_git` (`src/config.rs:11`) | See [`git`](#git). |
| `redis` | ADAPTED | `response_cache: {backend: redis, ...}` (`src/response_cache.rs:49-59`); the auth-cache use moved to `client_auth` | See [`redis`](#redis). |
| `backend` | ADAPTED | Root `listen`, `tls`, `logging`, `access_log`, `internal_token_env`, `client_auth.signing_key_env` | See [`backend`](#backend). |
| `database` | ADAPTED | `client_auth.database` (`src/client_auth.rs:32-48`) | See [`database`](#database-user-database). |
| `master_database` | ADAPTED | `master_database: {connection, interval_seconds}` (`src/master_database_worker.rs:9-15`) | See [`master_database`](#master_database). |
| `apphash_sources[]` (`type`, `dir`, `url`) | IGNORED_BY_ORIGINAL / NOT_APPLICABLE | none | The original marks it deprecated and ignored (`Haruki-Sekai-API@07da6b80:src/config.rs:497-506`) and warns at startup (`:570-587`). Sekai app hash. Sirius client identity is the static `client_version` (`src/config.rs:41`). |
| `asset_updater_servers[]` | ADAPTED | `asset_dispatch.targets[]` (`src/asset_dispatch.rs:14-37`) | See [`asset_updater_servers`](#asset_updater_servers). |
| `servers` (map region → `ServerConfig`) | ADAPTED | A single-region file is one region profile (`region`, `src/config.rs:29`). A multi-region file uses `regions: {jp: ..., hk: ...}` (`src/deployment.rs:23`), with 1–4 entries whose keys must equal `region` (a deprecated `tw` key is read as `hk`) (`:82-92`), and root-only `listen`/`tls`/`logging`/`access_log` (`:93-101`). | See [`servers.<region>`](#serversregion). |
| `registry` | ADAPTED | Separate `registry-serve REGISTRY_CONFIG` file (`src/main.rs:5-10`, `src/registry_service.rs:20-35`) | See [`registry`](#registry). |

## Regions

Original: `ServerRegion`, `Haruki-Sekai-API@07da6b80:src/config.rs:8-32`.

| Original value | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `jp` | REUSED | `Region::Jp` (`src/region.rs:13-20`) | The only region with verified game RPCs beyond discovery. Master CDN access always uses Basic with a credential reference (`src/config.rs:297-306`, `:361-367`). |
| `en`, `hk`, `kr` | ADAPTED | `Region::{En, Hk, Kr}`, protocol family `global` (`src/region.rs:16-18`; `tw` is a deprecated configuration alias of `hk`, see [REGIONS.md](REGIONS.md#the-hk-identifier)) | Game RPC is limited to the verified `Version` and `GetServerList` (`src/routes.rs:24-31`). Master storage, update, sync, Git, database and notifications are enabled (1.2.1; `Region::master_supported`, `src/region.rs`, used by `src/config.rs:274`, `src/master_git_worker.rs:46`, `src/master_database_worker.rs:21`, `src/master_notify.rs:151`, `src/registry_service.rs:56`). The Master CDN may be anonymous with explicit `master_update.cdn_authorization: none` (`src/config.rs:307-319`). CN remains rejected. |
| `cn` | ADAPTED | Parsed, then rejected with "cn is reserved; no verified endpoint or protocol is available" (`src/config.rs:251-255`) | Explicit rejection, exercised by the packaged smoke test (`scripts/smoke-release.py:151-154`). |
| `is_cp_server()` (JP/EN) | NOT_APPLICABLE | none | Selects Sekai CP versus Nuverse login and master paths (`Haruki-Sekai-API@07da6b80:src/config.rs:29-31`, `Haruki-Sekai-API@07da6b80:src/main.rs:141-177`). Sirius has one gRPC protocol per family. |

## `backend`

Original: `BackendConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:63-80`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `host`, `port` | ADAPTED | `listen: SocketAddr` (`src/config.rs:34`; required at the multi-region root, `src/deployment.rs:18`) | The single-region default changed from `0.0.0.0:9999` to `127.0.0.1:9999` (`src/deployment.rs:118-120`, README). |
| `log_level` | ADAPTED | `logging.level` (off/error/warn/info/debug/trace, `src/application_log.rs:20-30`, `:43-50`) | The original let `RUST_LOG` override it (`Haruki-Sekai-API@07da6b80:src/logging.rs:24-31`). Sirius ignores `RUST_LOG` (`docs/APPLICATION_LOG.md:39`). |
| `sekai_user_jwt_signing_key` | ADAPTED | `client_auth.signing_key_env` (`src/client_auth.rs:24`) | HS256 per-client JWT restored ([CLIENT_AUTH.md](CLIENT_AUTH.md)). The original ran open when the key or the user database was absent (`Haruki-Sekai-API@07da6b80:src/api/middleware.rs:44-55`). Sirius fails closed: the static bearer `api_token_env` is always required unless a verified user token is presented. The header is `X-Sirius-Token` instead of `x-haruki-sekai-token` (`src/client_auth.rs:18`). |
| `internal_token` | ADAPTED | `internal_token_env` (required, `src/config.rs:50`) | Original: an empty token disabled `/internal/*`. Sirius always requires it from an env reference. Peer reads use a separate `peer_token_env` (`src/config.rs:20`), which must differ from the API and internal references (`:191-197`). |

## `redis`

Original: `RedisConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:49-61`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | `response_cache.backend: redis` versus `memory` / `disabled` (`src/response_cache.rs:34-60`) | The original used Redis for the response cache (`Haruki-Sekai-API@07da6b80:src/api/apis.rs:131-148`) and the JWT authorization cache (`Haruki-Sekai-API@07da6b80:src/api/middleware.rs:71`, `:114-116`, `:191-202`). Sirius keeps the client-auth decision cache in process (`client_auth.cache_seconds` / `cache_entries`, `src/client_auth.rs:26-29`), keyed by a credential digest rather than the raw credential. |
| `host`, `port`, `password` | ADAPTED | `response_cache.url_env` holding a `redis://` or `rediss://` URL (`src/response_cache.rs:50`) | The secret moved into an env var. Sirius adds `namespace`, `operation_timeout_ms` and `max_entry_bytes` (`:51-58`). |

## `database` (user database)

Original: `DatabaseConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:102-115`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | Presence of the per-region `client_auth` section (`src/config.rs:25`) | Original use: the SeaORM `sekai_users` / `sekai_user_servers` tables behind the JWT middleware (`Haruki-Sekai-API@07da6b80:src/db.rs:9-45`). Sirius reads `sirius_api_users` / `sirius_api_user_regions` and never creates or changes the schema ([CLIENT_AUTH.md](CLIENT_AUTH.md)). |
| `dsn` | ADAPTED | `client_auth.database.{host, port, database, username, password_env, root_certificate, plaintext_loopback, timeout_seconds}` (`src/client_auth.rs:34-45`) | PostgreSQL only. Verified TLS unless loopback, password from an env reference, ambient libpq client settings SQLx would inherit (`PGSSLROOTCERT`/`PGSSLCERT`/`PGSSLKEY`/`PGOPTIONS`) rejected (transport shared with the Master mirror, `src/client_auth.rs:85-99`, `src/master_database.rs:119-124,155-169`). |
| `max_connections` (default 10) | ADAPTED | `client_auth.database.max_connections` (default 4, 1–64; `src/client_auth.rs:46-47`, `:61-63`) | Validated through the shared connection policy (`src/client_auth.rs:82`, `src/master_database.rs:96`). |
| `ingest_concurrency` | IGNORED_BY_ORIGINAL | none | Only read from `master_database` (`Haruki-Sekai-API@07da6b80:src/bin/run_ingest.rs:32`, `Haruki-Sekai-API@07da6b80:src/updater/scheduler.rs:124`, `:175`). On `database` it has no effect in the original. |
| `driver` (example only) | IGNORED_BY_ORIGINAL | none | Not a struct field. Sirius is PostgreSQL only. |

## `master_database`

Original: `DatabaseConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:102-115`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | Presence of `master_database` (`src/config.rs:9`), JP/HK/EN/KR (CN rejected) and requires `master_directory` (`src/master_database_worker.rs:20-31`) | Sirius stores the verified generic JSON documents plus JSONB. It does not use the original's Sekai Ent-typed tables, which the restoration objective excludes. |
| `dsn` | ADAPTED | `connection.{host, port, database, username, password_env, root_certificate, plaintext_loopback, timeout_seconds, keep_snapshots}` (`src/master_database.rs:32-48`) | The secret moved into an env var; TLS is required unless loopback. |
| `max_connections` | ADAPTED | `connection.max_read_connections` (default 4, 1–64; `src/master_database.rs:49-52`, `:63-65`, `:96`) sizes the read pool (`:506-513`) | Writers intentionally use one connection (`src/master_database.rs:210-211`, `:378-379`) because publication and migration are single serialized transactions. |
| `ingest_concurrency` | ADAPTED | none needed | The original knob bounded parallel per-table ingest memory (`Haruki-Sekai-API@07da6b80:src/ingest_engine.rs:25`). Sirius publishes each snapshot in one serial transaction (`src/master_database.rs:210-216`), so there is no parallelism to bound. |
| `driver` (example only) | IGNORED_BY_ORIGINAL | none | Not a struct field. |

## `git`

Original: `GitConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:136-169`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | Presence of `master_git` (`src/config.rs:11`, `src/master_git_worker.rs:9-18`) | JP/HK/EN/KR (CN rejected) and requires `master_directory` (`src/master_git_worker.rs:44-54`). Multi-region deployments require distinct state directories and remotes (`src/deployment.rs:106-126`). Accepted on Unix and Windows (`cfg!(any(unix, windows))`, `:27`). On Windows, Git runs in a kill-on-close Job Object, and the Windows CI job runs the Git tests. |
| `username` | ADAPTED | `commit.author.name` / `commit.committer.name` (`src/master_git.rs:37-42`, `:66-73`) | The original used `username` both as the committer name (`Haruki-Sekai-API@07da6b80:src/updater/git.rs:456`) and as the URL credential user (`:476`). A Basic user now goes inside `remote.authorization_env` (`src/master_git_worker.rs:48-70`). |
| `email` | ADAPTED | `commit.author.email` / `commit.committer.email` (`src/master_git.rs:41`) | |
| `password` | ADAPTED | `remote.authorization_env`, a full `Authorization: Basic` or `Bearer` header held in env (`src/master_git.rs:524`) | The original injected the credential into the remote URL (`Haruki-Sekai-API@07da6b80:src/updater/git.rs:476`, `:531`). Sirius passes it through Git config-env, never in a URL. |
| `sign_commits` | ADAPTED | Presence of `commit.signing` (`src/master_git.rs:72`) | |
| `signing_format` (`gpg` with `openpgp` alias, `ssh`) | REUSED | `commit.signing.format`: `openpgp` (alias `gpg`) or `ssh` (`src/master_git.rs:51-57`) | |
| `signing_key` | ADAPTED | `commit.signing.key` (`src/master_git.rs:63`) | Must be a 16–64 hex OpenPGP fingerprint or an absolute SSH key path (`:101-109`). The original also accepted an inline SSH public key (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:11`); Sirius rejects inline key material. |
| `signing_program` | REUSED | `commit.signing.program` (`src/master_git.rs:64`, `:112-124`) | Restricted to one absolute executable path. |
| `proxy` (absent inherits, `""` means direct) | DECISION | `remote.proxy_url_env`; omitted means direct (`src/master_git.rs:525`, `:552-560`) | There is no inheritance because there is no global proxy. Ambient `*_PROXY` variables are removed from the Git environment (`src/git_process.rs:201-212`). See [Decisions](#decisions). |
| (implicit) worktree = `master_dir` plus its `origin` | ADAPTED | Separate `state_directory`, which must differ from `master_directory`, and an explicit `remote.url` (`src/master_git_worker.rs:14`, `:30-33`; `src/master_git.rs:523`) | Each commit is built from a fresh tree of the verified snapshot (`src/master_git.rs:191-282`). |

## `servers.<region>`

Original: `ServerConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:300-379`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | The region profile is present. Remote-only serving is `node_routing.local_priority: null` with targets (`src/node_routing.rs:20`, `:60`) | |
| `master_dir` | ADAPTED | `master_directory`, an immutable snapshot store with a `CURRENT` pointer (`src/config.rs:58`, `src/master.rs:334-358`) | |
| `version_path` | NOT_APPLICABLE | none | Persists the Sekai `version.json` (appVersion/appHash/dataVersion) (`Haruki-Sekai-API@07da6b80:src/updater/master.rs:1066`) and app-identity overrides (`Haruki-Sekai-API@07da6b80:src/api/internal.rs:253-257`). Sirius has no app hash or login version file. Version state is observed through the `Version` RPC and snapshot metadata (`publication.json`, manifests). |
| `account_dir` | DECISION | `accounts[].{player_id_env, credential_env, credentials_file}` (`src/accounts.rs:18-25`) plus `POST /internal/v1/accounts/reload` (`src/api.rs:137`) | The original polled the directory every 5 s and reloaded (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:302-322`). See [Decisions](#decisions). |
| `api_url` | REUSED | `endpoint`, an HTTPS origin checked against the region's known services (`src/config.rs:40`, `:284-306`) | |
| `nuverse_master_data_url` | NOT_APPLICABLE | none | Nuverse master download (`Haruki-Sekai-API@07da6b80:src/updater/master.rs:789-791`). Sirius has no Nuverse region. |
| `nuverse_schema_bundle_path` | NOT_APPLICABLE | none | Loaded only for non-CP regions (`Haruki-Sekai-API@07da6b80:src/main.rs:141-153`). The Sirius protocol schema is `protocol_directory` (`src/config.rs:33`). |
| `require_cookies` | NOT_APPLICABLE | none | JP Sekai cookie (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:95`). |
| `headers` (free-form map) | DECISION | none | Merged into every Sekai HTTP request (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:91`) next to computed `X-App-Hash`/`X-Data-Version` (`:189-191`). See [Decisions](#decisions). |
| `aes_key_hex`, `aes_iv_hex` | NOT_APPLICABLE | none | Sekai msgpack/AES API payload cipher. Sirius game traffic is plain Protobuf gRPC over verified TLS (`src/client.rs:772-797`). In the original, these keys were also the Master fallback cipher; that role is covered by the next row. |
| `master_aes_key_hex`, `master_aes_iv_hex` | ADAPTED | `master_update.key_hex_env` / `iv_hex_env` (`src/config.rs:129-130`, `src/master_update.rs:163-165`); the `master-import` CLI reads `SIRIUS_MASTER_KEY_HEX` / `SIRIUS_MASTER_IV_HEX` (`src/main.rs:227-234`) | The secrets moved into env vars. |
| `enable_master_updater` | ADAPTED | Presence of `master_update` (`src/config.rs:59`) | Mutually exclusive with `master_sync` (`src/config.rs:237-247`). |
| `master_updater_cron` | DECISION | `master_update.interval_seconds`, 60–86400 (`src/config.rs:131`, `:272`) | Runs once at startup, then waits this interval after each completed attempt. See [Decisions](#decisions). |
| `enable_app_hash_updater`, `app_hash_updater_cron` | IGNORED_BY_ORIGINAL / NOT_APPLICABLE | none | Deprecated and ignored (`Haruki-Sekai-API@07da6b80:src/config.rs:333-340`, warned at `:579-584`). |
| `upstreams[]` | ADAPTED | `node_routing.targets[]` | See [`upstreams`](#serversregionupstreams). |
| `local_priority` (default 0) | REUSED | `node_routing.local_priority` (default `0`, `null` means remote-only; `src/node_routing.rs:20`, `:31`) | |
| `master_sync` | ADAPTED | See [`master_sync`](#serversregionmaster_sync). | |
| `master_remote_source` | NOT_APPLICABLE | See [`master_remote_source`](#serversregionmaster_remote_source). | |
| `cache_ttls` | ADAPTED | See [`cache_ttls`](#serversregioncache_ttls). | |
| `prune_stale` | ADAPTED | none needed | Each Sirius snapshot is exactly the verified manifest set in a new directory (`src/master.rs:276-332`). Tables dropped upstream are absent from the next snapshot and from the next Git tree without deleting files in place. |
| `prune_min_ratio`, `prune_max_files` | DECISION | none | Mass-deletion guard (`Haruki-Sekai-API@07da6b80:src/config.rs:361-368`, `Haruki-Sekai-API@07da6b80:src/updater/prune.rs:66-87`). See [Decisions](#decisions). |
| `prune_protect` | NOT_APPLICABLE | none | Extends a built-in list of Sekai consumer tables such as `events` and `worldBlooms` (`Haruki-Sekai-API@07da6b80:src/updater/prune.rs:31-59`, `Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:112-123`). Sirius has no table deletion to protect against. |
| `prune_pending_path` | NOT_APPLICABLE | none | State for the original's two-dump prune rule (`Haruki-Sekai-API@07da6b80:src/updater/prune.rs:106-112`). There is no in-place prune. |

### `servers.<region>.upstreams[]`

Original: `UpstreamConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:177-189`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `url` | ADAPTED | `node_routing.targets[].origin`, plus `regional_paths` and `allow_http` (`src/node_routing.rs:41-53`) | Sirius routes verified public reads only. |
| `token` | ADAPTED | `targets[].token_env`, which is the remote's `peer_token_env`, not its internal token (`src/node_routing.rs:46`) | |
| `priority` (default 10) | REUSED | `targets[].priority`, default 10 (`src/node_routing.rs:47-56`) | |
| `name` | REUSED | `targets[].name`, required and unique, and must not be `local` (`src/node_routing.rs:44`, `:74-81`) | |

Sirius adds `timeout_ms`, `max_inflight`, `failure_threshold`, `cooldown_ms` and `transport`
(`src/node_routing.rs:17-27`). These are validated even when `targets` is empty (`:58-70`) and
have no effect until a target exists. They are never silently accepted with invalid values. Peer transport ignores ambient proxies (`src/peer_transport.rs:105`).

### `servers.<region>.master_sync`

Original: `MasterSyncConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:195-244`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `source_url` | ADAPTED | `master_sync.origin`, plus `regional_paths` and `allow_http` (`src/master_sync.rs:13-27`) | Presence enables it. Requires `master_directory` and excludes `master_update` (`src/config.rs:237-247`). |
| `source_token` | ADAPTED | `master_sync.token_env`, the owner's public read bearer | |
| `poll_cron` (empty = webhook only) | DECISION | `master_sync.interval_seconds`, 60–86400 (`src/master_sync.rs:22`, `:36`); hints arrive at `POST /internal/v1/master-data/sync` (`src/api.rs:143-146`) | Polling is always on. Sirius adds `timeout_seconds` and `request_timeout_ms` (`src/master_sync.rs:23-26`). See [Decisions](#decisions). |
| `notify[]` (`MasterSyncPeer`: `url`, `token`) | ADAPTED | `master_notify.targets[].{name, origin, token_env, regional_paths, allow_http}`, `interval_seconds`, `request_timeout_ms` (`src/master_notify.rs:123-148`) | JP/HK/EN/KR, CN rejected (`:150-170`). Adds per-target retry and deduplication. |

### `servers.<region>.master_remote_source`

Original: `MasterRemoteSourceConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:206-221`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `url`, `token` | NOT_APPLICABLE | none | The original borrowed a peer's game accounts for the login-derived version probe and the CP authenticated master-split fetch (`Haruki-Sekai-API@07da6b80:src/config.rs:206-211`, used at `Haruki-Sekai-API@07da6b80:src/updater/scheduler.rs:158-194`). In Sirius the version probe is the anonymous `Version` RPC, which is not in `authenticated()` (`src/client.rs:23-34`). Master download uses CDN credentials (`master_update.username_env` and `cdn_credential_env`), or none for Global with `cdn_authorization: none` (`src/master_update.rs:165-186`). No game account is involved, so there is nothing to borrow. |

### `servers.<region>.cache_ttls`

Original: `CacheTtlConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:246-298`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `ranking_top100` (1 s) | ADAPTED | `response_cache.route_ttl_ms.event_rankings` (`src/response_cache.rs:11-19`, `:44`, `:56`) | Milliseconds instead of fractional seconds. Sirius also has `song_rankings` and `challenge_rankings`. |
| `ranking_border` (30 s) | ADAPTED | `event_rankings` | The Sirius protocol has one event ranking RPC (`src/routes.rs:4`), with no separate border call. |
| `static` (`/system`, `/information`, 300 s) | ADAPTED | `announcements` / `announcement` TTLs | `Version` is never cached. |
| `max_stale` (30 s) | ADAPTED | `stale_while_revalidate_ms` (default 0, off; `src/response_cache.rs:42`, `:54`) | Opt-in instead of on by default. |

## `registry`

Original: `RegistryConfig` / `MusicMetasConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:393-452`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `host`, `port` (`0.0.0.0:9998`) | ADAPTED | `listen` (required, `src/registry_service.rs:23`) | Also `tls`, `logging` and `access_log` (`:32-34`). |
| `token` (mutations only, reads open) | ADAPTED | `token_env` for reads (reads are authenticated) and `owner.internal_token_env` for internal routes (`src/registry_service.rs:24`, `:108-117`; `src/registry_owner.rs:13`) | Stricter than the original. |
| `state_dir` | ADAPTED | `backend: {kind: files, directory}` (`src/registry_service.rs:39`) | |
| `state_dsn` | ADAPTED | `backend: {kind: postgres, connection}` (`src/registry_service.rs:40`) | |
| `subscribers[]` | ADAPTED | `notify`, a `master_notify` config (`src/registry_service.rs:31`) | |
| `account_nodes[]` | NOT_APPLICABLE | none | Pushes the Sekai appVersion/appHash to `POST /internal/app-identity` (`Haruki-Sekai-API@07da6b80:src/config.rs:424-427`, `Haruki-Sekai-API@07da6b80:src/api/internal.rs:216-257`). Sirius `client_version` is static configuration (`src/config.rs:41`); protocol reload does not change it (`docs/PROTO_RELOAD.md:36`). |
| `music_metas.{enabled, cron, inject_omakase, sources, proxy}` | NOT_APPLICABLE | none | Pulls Sekai `music_metas*.json` from sekai-data.3-3.dev and injects the synthetic omakase rows (`Haruki-Sekai-API@07da6b80:src/registry/metas.rs:1-42`, `Haruki-Sekai-API@07da6b80:src/config.rs:439-441`). This is a Sekai-specific data feed with no Sirius counterpart. |
| (implicit) all regions in one registry | ADAPTED | One `scope` per process, JP/HK/EN/KR (`src/registry_service.rs:25`, `:56`) | `regional_paths` uses the scope's region. |

## `asset_updater_servers[]`

Original: `AssetUpdaterInfo`, `Haruki-Sekai-API@07da6b80:src/config.rs:508-513`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `url` | ADAPTED | `asset_dispatch.targets[].origin` (`src/asset_dispatch.rs:26`) | Adds a durable outbox (`state_directory`, `history_capacity`), profile/revision identity and completion requirements (`:14-37`). |
| `authorization` | ADAPTED | `targets[].token_env` (`src/asset_dispatch.rs:27`) | Adds optional `user_agent`. |

## Environment variables

| Original variable | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `CONFIG_PATH` (`Haruki-Sekai-API@07da6b80:src/config.rs:591`) | ADAPTED | `SIRIUS_CONFIG_PATH`, default `sirius-api-config.yaml` (`src/main.rs:46-49`, `:172-173`, `:244-245`) | `registry-serve` and `master-db-import` / `master-db-migrate` take their config path from argv (`src/main.rs:5-10`, `:69-77`). |
| `RUST_LOG` (`Haruki-Sekai-API@07da6b80:src/logging.rs:28`) | ADAPTED | Ignored on purpose; use `logging.level` | `docs/APPLICATION_LOG.md:39`. |
| `BENCH_*` (`Haruki-Sekai-API@07da6b80:src/bin/bench_profile.rs:178-337`), `HARUKI_BENCH_*` (`Haruki-Sekai-API@07da6b80:src/updater/master_stream.rs:628-732`) | NOT_APPLICABLE | none | Benchmarks for Sekai profile and master ingest. |
| `HARUKI_TEST_REGISTRY_DSN` (`Haruki-Sekai-API@07da6b80:src/registry/state.rs:1025`) | ADAPTED | `SIRIUS_TEST_POSTGRES_PORT` / `SIRIUS_TEST_POSTGRES_PASSWORD`; `SIRIUS_TEST_REDIS_SERVER` and `SIRIUS_TEST_GPG_PROGRAM` enable other optional tests (`src/tests.rs`) | Test-only. Never read by the service. |

Other environment reads in Sirius:

- Every `*_env` field is a variable name, and its value is resolved by `secret()`
  (`src/config.rs:134-143`). YAML never holds a secret value.
- `master-import` reads `SIRIUS_MASTER_KEY_HEX` / `SIRIUS_MASTER_IV_HEX` (`src/main.rs:227-234`).
- `master-git-push` reads `SIRIUS_MASTER_GIT_PROXY_URL` / `SIRIUS_MASTER_GIT_AUTHORIZATION`
  (`src/main.rs:191-199`). See [Decisions](#decisions).
- PostgreSQL connections refuse to start while `PGSSLROOTCERT`, `PGSSLCERT`, `PGSSLKEY` or
  `PGOPTIONS` is set; other SQLx-read `PG*` variables are always overridden by explicit
  configuration (`src/master_database.rs:119-124,155-169`).

## Decisions

These generic capabilities are intentionally not restored in their original form.

1. **Prune mass-deletion guard (`prune_min_ratio`, `prune_max_files`) is not restored.** The guard
   protected a flat `master_dir` from being emptied when a partial dump was pruned in place.
   Sirius has no in-place prune:
   - `Manifest::parse` rejects empty, oversized or malformed manifests (`src/master.rs:172-207`).
   - Every listed table must be read and decoded into a fresh staging directory, and any failure
     aborts the import (`src/master.rs:276-332`).
   - The staged snapshot is renamed into place and `CURRENT` is switched atomically
     (`src/master.rs:334-358`).

   A partial download therefore cannot publish. File-store snapshots are never deleted by Sirius.
   Git keeps every earlier commit. The PostgreSQL mirror only drops whole snapshots beyond
   `keep_snapshots` (`src/master_database.rs:325-326`). A table that upstream really drops is
   absent from the next snapshot, as the manifest says, and all earlier snapshots remain
   readable. No ratio threshold would prevent a verified manifest from being published.
2. **Cron syntax became fixed intervals.** `master_update.interval_seconds`,
   `master_sync.interval_seconds` and the Git and database worker intervals replace 6-field cron.
   Each worker runs one attempt at a time and sleeps the interval after it, so runs never
   overlap (for example `src/master_sync.rs:357`). Wall-clock schedules are not reproduced. Owners send immediate hints through
   `master_notify`, and `POST /internal/v1/master-data/sync` triggers a sync on demand.
3. **Account-directory auto-watch became an explicit reload.** Accounts are listed explicitly and
   reloaded with `POST /internal/v1/accounts/reload` (`src/api.rs:137`,
   [ACCOUNTS.md](ACCOUNTS.md)). The reload validates every source, drains active calls and
   swaps the pool atomically. A failed candidate keeps the previous pool. A directory poller
   provides no such barrier.
4. **The free-form `headers` override is not restored.** Sirius sends a fixed gRPC metadata set
   built in `src/client.rs:777-797`: content type, `te`, `grpc-timeout`, `x-platform` and
   `x-client-version` from `platform`/`client_version`, the observed `x-master-version`, and
   account headers only on authenticated routes. Request and response message encoding comes
   from the loaded, verified protocol bundle. The Sekai headers that motivated the map
   (`X-App-Hash`, `X-Data-Version`, cookies) do not exist in this protocol, and arbitrary extra metadata
   would be sent unverified on anonymous and authenticated calls alike.
5. **The global proxy with inheritance became explicit per-transport proxies.** Game gRPC
   (`upstream.*`), the Master CDN (`master_update.network.*`) and Git (`master_git.remote.*`)
   each take their own proxy URL and authorization env references. An omitted proxy means a
   direct connection. Peer, sync, notification and asset-dispatch clients ignore ambient proxies
   (`src/peer_transport.rs:105`, `src/master_sync.rs:157`, `src/master_notify.rs:60`,
   `src/asset_jobs.rs:105`), and Git strips `*_PROXY` variables (`src/git_process.rs:201-212`).
   Each proxy credential stays scoped to one destination, and one dead proxy cannot take down
   unrelated traffic. The original needed an empty-string override for exactly that case
   (`Haruki-Sekai-API@07da6b80:src/config.rs:445-451`).
6. **Global example CDN roots are the verified server-list roots.** Since 1.2.1,
   `docs/examples/{en,hk,kr}.yaml` and the Global entries of
   `sirius-multi-region-config.example.yaml` use the first CDN line of each region's public
   server-list entry, with `cdn_credential_env: {}`. The Global Master CDN needs no credential,
   and no Global CDN credential is verified or shipped. Startup never contacts the CDN: the
   optional `master_update` block stays commented out, so the packaged smoke test still starts a
   Global service offline from the shipped example (`scripts/smoke-release.py:123-146`). Earlier
   releases used `https://cdn.example.invalid/...` placeholders.
7. **The Master Git CLI takes its state directory and remote from argv and env.**
   `master-git-commit STATE_DIR` and `master-git-push STATE_DIR REMOTE_URL`
   (`src/main.rs:164-219`) are explicit one-shot operations. They parse and validate the whole
   profile, then use only `master_directory`, the scope, and `master_git.commit` (identity and
   signing, `:185-189`). They ignore `master_git.state_directory`, `interval_seconds` and
   `remote.*`. The push reads its proxy and authorization only from `SIRIUS_MASTER_GIT_PROXY_URL` /
   `SIRIUS_MASTER_GIT_AUTHORIZATION`, fixes `allow_http: false`, and sets `allow_file` only for a
   `file://` argument (`:191-199`). The background worker honors every `master_git` field. The
   split keeps a manual push from silently targeting the service's configured remote or state.
8. **Client authorization deviates from the original.** It fails closed, uses the Sirius header
   and table names, enforces `exp`, and uses an in-process cache instead of Redis. See
   [CLIENT_AUTH.md](CLIENT_AUTH.md#differences-from-the-original).

## Original fields that were ignored by the original itself

The original structs accept unknown keys, so these keys parsed without error but had no effect.
Sirius rejects all of them as unknown fields. Where the intent was useful, Sirius implements it
under a real field.

| Original key | Where | Sirius |
| --- | --- | --- |
| `apphash_sources[]`, `servers.*.enable_app_hash_updater`, `servers.*.app_hash_updater_cron` | Parsed, deprecated and warned about (`Haruki-Sekai-API@07da6b80:src/config.rs:333-340`, `:497-506`, `:570-587`) | NOT_APPLICABLE (Sekai app hash). |
| `database.ingest_concurrency` | Parsed, never read for the user database | No counterpart. |
| `database.driver`, `master_database.driver` | Example only (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:46`, `:52`) | PostgreSQL only. |
| `backend.ssl`, `ssl_cert`, `ssl_key` | Example only (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:23-25`) | Implemented as `tls.{certificate_file, private_key_file, handshake_timeout_ms}` (`src/server.rs:16-23`). |
| `backend.main_log_file` | Example only (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:27`) | `logging.output: {type: file, path, rotation, max_files}` (`src/access_log.rs:35-47`). |
| `backend.access_log` (template), `access_log_path` | Example only (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:28-29`) | `access_log.{format: json\|text, output}` (`src/access_log.rs:62-70`). Fixed record formats replace the free-form template. |
| `backend.enable_trust_proxy`, `trusted_proxies`, `proxy_header` | Example only (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:34-42`) | `access_log.trusted_proxies` and `proxy_header` (`src/access_log.rs:68-69`, `:101-113`). A non-empty list replaces the enable flag. |

## Example coverage

Every shipped example is parsed by tests:

- `sirius-api-config.example.yaml` (`src/tests.rs:2075-2077`).
- The block-YAML multi-region example, parsed as shipped and again with its commented `tls:` and
  `access_log:` blocks uncommented.
- `docs/examples/{en,hk,kr}.yaml`, `docs/examples/master-registry.yaml` (also with `notify:`
  uncommented) and `docs/examples/master-database.yaml`
  (`every_shipped_example_parses_including_documented_optional_blocks`, `src/tests.rs:11751`).

The commented optional blocks of `sirius-api-config.example.yaml` are not parsed uncommented.
