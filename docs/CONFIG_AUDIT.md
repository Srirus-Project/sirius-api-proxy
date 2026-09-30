# Configuration audit against the original

This document is the field-by-field configuration audit that the 1.2.0 release gate in
[RESTORATION_1_2.md](RESTORATION_1_2.md) requires for the API: every configuration field of the
original is accounted for, with no placeholder or silently ignored field, and every
game-specific non-applicability is backed by evidence. It was revised for 1.2.1; see
[Revision for 1.2.1](#revision-for-121).

- **Original:** Haruki-Sekai-API `07da6b80e6a59ece89251f4694afe94bea72e131` (MIT), cited as
  `Haruki-Sekai-API@07da6b80:path:line`.
- **Sirius:** this repository. Sirius paths are repository-relative, and line numbers refer to
  the 1.2.1 code at `0a6b99b` (the revision commit changes only documentation and tests). A
  bare `:line` continues the previous path in the same table cell.
- **Method:** every `Deserialize` struct reachable from the original `Config`
  (`Haruki-Sekai-API@07da6b80:src/config.rs:515-538`) and every key in its example file
  (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml`) was traced to its runtime use in the original, then to
  the corresponding Sirius field and the code that reads it. Environment variables read by either
  codebase were checked the same way. Every Sirius configuration struct was checked in reverse
  for fields that are parsed and never used; see [Sirius fields](#sirius-fields-reverse-check).

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
(`src/deployment.rs:13-15`), the registry `Config` and `Backend` (`src/registry_service.rs:20-42`),
the Global identity file (`src/global_account.rs:83-106`, `src/global_sdk.rs:50-69`) and every
nested section cited below. The only `Deserialize` types without it are unit-variant enums, such
as regions, platforms, log levels, Git layouts, signing formats and CDN authorization modes, which
already reject unknown values, and wire or response types that are not configuration (asset
updater replies, JWT header and claims, database receipts, cache entries, the release list).
Misspelled keys and keys copied from the original therefore fail at startup instead of being
ignored. The original has no `deny_unknown_fields` anywhere in `src/`.

## Top level

Original: `Config`, `Haruki-Sekai-API@07da6b80:src/config.rs:515-538`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `proxy` | DECISION | Per-transport explicit proxies: `upstream.proxy_url_env` / `proxy_authorization_env` (game gRPC, `src/config.rs:76-77`, `src/transport.rs:35-52`, and the Global SDK login of the same profile, `src/client.rs:1436-1462`), `master_update.network.proxy_url_env` / `proxy_authorization_env` (Master CDN, `src/master_update.rs:54-55`, `:109-124`), `resource_snapshot.network.*` (Global catalog `.hash`, `src/config.rs:160-162`, `src/client.rs:150-156`), `master_git.remote.proxy_url_env` (Git, `src/master_git.rs:794`) | Original: one global proxy used by the game client (`Haruki-Sekai-API@07da6b80:src/main.rs:81-82`) and the updater (`Haruki-Sekai-API@07da6b80:src/updater/scheduler.rs:27`), and inherited by Git and music_metas. See [Decisions](#decisions). |
| `jp_sekai_cookie_url` | NOT_APPLICABLE | none | Sekai JP cookie bootstrap (`Haruki-Sekai-API@07da6b80:src/main.rs:80`), used only when `region == Jp && require_cookies` (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:95`). Sirius game RPC is gRPC and has no cookie step (`src/client.rs:1069-1111`). |
| `git` | ADAPTED | `master_git` (`src/config.rs:11`) | See [`git`](#git). |
| `redis` | ADAPTED | `response_cache: {backend: redis, ...}` (`src/response_cache.rs:49-59`); the auth-cache use moved to `client_auth` | See [`redis`](#redis). |
| `backend` | ADAPTED | Root `listen`, `tls`, `logging`, `access_log`, `internal_token_env`, `client_auth.signing_key_env` | See [`backend`](#backend). |
| `database` | ADAPTED | `client_auth.database` (`src/client_auth.rs:32-48`) | See [`database`](#database-user-database). |
| `master_database` | ADAPTED | `master_database: {connection, interval_seconds}` (`src/master_database_worker.rs:9-15`) | See [`master_database`](#master_database). |
| `apphash_sources[]` (`type`, `dir`, `url`) | IGNORED_BY_ORIGINAL / NOT_APPLICABLE | none | The original marks it deprecated and ignored (`Haruki-Sekai-API@07da6b80:src/config.rs:497-506`) and warns at startup (`:570-587`). Sekai app hash. Sirius client identity is the static `client_version` (`src/config.rs:41`); Global PlayerLogin sends it as `clientVersion` next to fixed OneSDK constants verified from the Global APK (`src/global_account.rs:19-24`, `:253-270`, `src/global_sdk.rs:25-38`). Nothing is fetched or hashed. |
| `asset_updater_servers[]` | ADAPTED | `asset_dispatch.targets[]` (`src/asset_dispatch.rs:15-38`) | See [`asset_updater_servers`](#asset_updater_servers). |
| `servers` (map region → `ServerConfig`) | ADAPTED | A single-region file is one region profile (`region`, `src/config.rs:28-29`). A multi-region file uses `regions: {jp: ..., hk: ...}` (`src/deployment.rs:23-24`), with 1–4 entries whose keys must equal `region` (a deprecated `tw` key is read as `hk`, `:27-48`) (`:113-123`), and root-only `listen`/`tls`/`logging`/`access_log`/`http_compression` (`:126-135`). | See [`servers.<region>`](#serversregion). |
| (no field; response compression on every route) | ADAPTED | Opt-in root `http_compression: {enabled}` (`src/config.rs:41`, `src/deployment.rs:24`, `src/registry_service.rs:37`, `src/http_compression.rs`) | The original applies `CompressionLayer::new()` unconditionally to all routes, internal and peer included (`Haruki-Sekai-API@9a53714:src/api/routes.rs:141`), and enables reqwest `gzip`/`brotli`/`zstd` for its outbound clients (`Haruki-Sekai-API@9a53714:Cargo.toml:17`). See [Decision 24](#decisions). |
| `registry` | ADAPTED | Separate `registry-serve REGISTRY_CONFIG` file (`src/main.rs:5-10`, `src/registry_service.rs:20-36`) | See [`registry`](#registry). |

## Regions

Original: `ServerRegion`, `Haruki-Sekai-API@07da6b80:src/config.rs:8-32`.

| Original value | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `jp` | REUSED | `Region::Jp` (`src/region.rs:13-20`) | Static existing accounts (`player_id_env`/`credential_env` or `credentials_file`, `src/accounts.rs:24-34`); no login step. Master CDN access always uses Basic with a credential reference (`src/config.rs:324-333`, `:418-426`). |
| `en`, `tw`, `kr` | ADAPTED | `Region::{En, Hk, Kr}`, protocol family `global` (`src/region.rs:16-18`, `:87-93`). The original `tw` is `hk` since 1.2.1; `tw` is accepted only as a deprecated configuration and CLI alias of `hk` and is never written or served (`src/region.rs:21-63`, `src/deployment.rs:27-48`, `src/master_registry.rs:55-71`; [REGIONS.md](REGIONS.md#the-hk-identifier)) | Game RPCs are Version, GetServerList and the JP read operations except Whoami, plus the internal `PlayerLogin` (`src/routes.rs:24-53`); authenticated calls use Global SDK guest accounts ([ACCOUNTS.md](ACCOUNTS.md#global-accounts)). Master storage, update, sync, Git, database and notifications are enabled (`Region::master_supported`, `src/region.rs:131-135`, used by `src/config.rs:301`, `src/master_git_worker.rs:46`, `src/master_database_worker.rs:21`, `src/master_notify.rs:151`, `src/registry_service.rs:57`). The Master CDN may be anonymous with explicit `master_update.cdn_authorization: none` (`src/config.rs:324-346`). Schema-3 resource snapshots are opt-in with `resource_snapshot` (`src/config.rs:348-379`). |
| `cn` | ADAPTED | Parsed, then rejected with "cn is reserved; no verified endpoint or protocol is available" (`src/config.rs:296-300`) | Explicit rejection, exercised by the packaged smoke test (`scripts/smoke-release.py:154-157`). |
| `is_cp_server()` (JP/EN) | NOT_APPLICABLE | none | Selects the Sekai CP versus Nuverse account format, login URL and master paths (`Haruki-Sekai-API@07da6b80:src/config.rs:29-31`, `Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:713-757`, `Haruki-Sekai-API@07da6b80:src/main.rs:141-177`). Sirius has one gRPC protocol per family. Its Global login is not a CP or Nuverse login: it is a OneSDK guest `cache.login` followed by the gRPC `PlayerLogin`, configured by `global_login` and `accounts[].global_identity_file` (see [Sirius fields](#sirius-fields-reverse-check)). |

## `backend`

Original: `BackendConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:63-80`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `host`, `port` | ADAPTED | `listen: SocketAddr` (`src/config.rs:34`; required at the multi-region root, `src/deployment.rs:18`) | The single-region default changed from `0.0.0.0:9999` to `127.0.0.1:9999` (`src/deployment.rs:165-167`, README). |
| `log_level` | ADAPTED | `logging.level` (off/error/warn/info/debug/trace, `src/application_log.rs:20-30`, `:43-50`) | The original let `RUST_LOG` override it (`Haruki-Sekai-API@07da6b80:src/logging.rs:24-31`). Sirius ignores `RUST_LOG` (`docs/APPLICATION_LOG.md:39`). |
| `sekai_user_jwt_signing_key` | ADAPTED | `client_auth.signing_key_env` (`src/client_auth.rs:24`) | HS256 per-client JWT restored ([CLIENT_AUTH.md](CLIENT_AUTH.md)). The original ran open when the key or the user database was absent (`Haruki-Sekai-API@07da6b80:src/api/middleware.rs:44-55`). Sirius fails closed: the static bearer `api_token_env` is always required unless a verified user token is presented. The header is `X-Sirius-Token` instead of `x-haruki-sekai-token` (`src/client_auth.rs:18`). |
| `internal_token` | ADAPTED | `internal_token_env` (required, `src/config.rs:50`) | Original: an empty token disabled `/internal/*`. Sirius always requires it from an env reference. Peer reads use a separate `peer_token_env` (`src/config.rs:20`), which must differ from the API and internal references (`:236-242`). |

## `redis`

Original: `RedisConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:49-61`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | `response_cache.backend: redis` versus `memory` / `disabled` (`src/response_cache.rs:34-59`) | The original used Redis for the response cache (`Haruki-Sekai-API@07da6b80:src/api/apis.rs:131-148`) and the JWT authorization cache (`Haruki-Sekai-API@07da6b80:src/api/middleware.rs:71`, `:114-116`, `:191-202`). Sirius keeps the client-auth decision cache in process (`client_auth.cache_seconds` / `cache_entries`, `src/client_auth.rs:25-29`), keyed by a credential digest rather than the raw credential. |
| `host`, `port`, `password` | ADAPTED | `response_cache.url_env` holding a `redis://` or `rediss://` URL (`src/response_cache.rs:50`) | The secret moved into an env var. Sirius adds `namespace`, `operation_timeout_ms` and `max_entry_bytes` (`:51-58`). |

## `database` (user database)

Original: `DatabaseConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:102-115`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | Presence of the per-region `client_auth` section (`src/config.rs:25`, read at `src/client.rs:165-172`) | Original use: the SeaORM `sekai_users` / `sekai_user_servers` tables behind the JWT middleware (`Haruki-Sekai-API@07da6b80:src/db.rs:9-45`). Sirius reads `sirius_api_users` / `sirius_api_user_regions` and never creates or changes the schema ([CLIENT_AUTH.md](CLIENT_AUTH.md)). |
| `dsn` | ADAPTED | `client_auth.database.{host, port, database, username, password_env, root_certificate, plaintext_loopback, timeout_seconds}` (`src/client_auth.rs:34-45`) | PostgreSQL only. Verified TLS unless loopback, password from an env reference, ambient libpq client settings SQLx would inherit (`PGSSLROOTCERT`/`PGSSLCERT`/`PGSSLKEY`/`PGOPTIONS`) rejected (transport shared with the Master mirror, `src/client_auth.rs:84-100`, `src/master_database.rs:148-154,182-196`). |
| `max_connections` (default 10) | ADAPTED | `client_auth.database.max_connections` (default 4, 1–64; `src/client_auth.rs:46-47`, `:61-63`) | Validated through the shared connection policy (`src/client_auth.rs:82`, `:97`, `src/master_database.rs:107`) and used as the pool size (`src/client_auth.rs:140`). |
| `ingest_concurrency` | IGNORED_BY_ORIGINAL | none | Only read from `master_database` (`Haruki-Sekai-API@07da6b80:src/bin/run_ingest.rs:32`, `Haruki-Sekai-API@07da6b80:src/updater/scheduler.rs:124`, `:175`). On `database` it has no effect in the original. |
| `driver` (example only) | IGNORED_BY_ORIGINAL | none | Not a struct field. Sirius is PostgreSQL only. |

## `master_database`

Original: `DatabaseConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:102-115`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | Presence of `master_database` (`src/config.rs:9`), JP/HK/EN/KR (CN rejected) and requires `master_directory` (`src/master_database_worker.rs:20-35`) | Sirius stores the verified generic JSON documents plus JSONB. It does not use the original's Sekai Ent-typed tables, which the restoration objective excludes. |
| `dsn` | ADAPTED | `connection.{host, port, database, username, password_env, root_certificate, plaintext_loopback, timeout_seconds, read_timeout_seconds, keep_snapshots}` (`src/master_database.rs:32-58`); `read_timeout_seconds` (1.3.0; 1–600, default min(`timeout_seconds`, 30)) is validated at `:103-105` and read by `read_timeout`/`read_options` (`:125-130`, `:140-142`) in `Reader::new` (`:674-683`) | The secret moved into an env var; TLS is required unless loopback. `timeout_seconds` bounds publication, import and migration; reads have their own deadline ([Decision 17](#decisions)). |
| `max_connections` | ADAPTED | `connection.max_read_connections` (default 4, 1–64; `src/master_database.rs:54-57`, `:68-70`, `:107`) sizes the read pool (`:673-702`) | Writers intentionally use one connection (`src/master_database.rs:253-254`, `:445-446`) because publication and migration are single serialized transactions. |
| `ingest_concurrency` | ADAPTED | none needed | The original knob bounded parallel per-table ingest memory (`Haruki-Sekai-API@07da6b80:src/ingest_engine.rs:25`). Sirius publishes each snapshot in one serial transaction (`src/master_database.rs:244-264`, `:301`), so there is no parallelism to bound. |
| `driver` (example only) | IGNORED_BY_ORIGINAL | none | Not a struct field. |

## `git`

Original: `GitConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:136-169`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | Presence of `master_git` (`src/config.rs:11`, `src/master_git_worker.rs:9-27`) | JP/HK/EN/KR (CN rejected) and requires `master_directory` (`src/master_git_worker.rs:45-73`). Multi-region deployments require distinct state directories and remotes (`src/deployment.rs:135-150`). Accepted on Unix and Windows (`cfg!(any(unix, windows))`, `src/master_git_worker.rs:55`). On Windows, Git runs in a kill-on-close Job Object, and the Windows CI job runs the Git tests. |
| `username` | ADAPTED | `commit.author.name` / `commit.committer.name` (`src/master_git.rs:211-225`, `:240-247`) | The original used `username` both as the committer name (`Haruki-Sekai-API@07da6b80:src/updater/git.rs:456`) and as the URL credential user (`:476`). A Basic user now goes inside `remote.authorization_env` (`src/master_git_worker.rs:76-99`). |
| `email` | ADAPTED | `commit.author.email` / `commit.committer.email` (`src/master_git.rs:215`) | |
| `password` | ADAPTED | `remote.authorization_env`, a full `Authorization: Basic` or `Bearer` header held in env (`src/master_git.rs:827`, `:880-898`) | The original injected the credential into the remote URL (`Haruki-Sekai-API@07da6b80:src/updater/git.rs:476`, `:531`). Sirius passes it through Git config-env, never in a URL (`src/master_git.rs:931-933`). |
| `sign_commits` | ADAPTED | Presence of `commit.signing` (`src/master_git.rs:246`) | |
| `signing_format` (`gpg` with `openpgp` alias, `ssh`) | REUSED | `commit.signing.format`: `openpgp` (alias `gpg`) or `ssh` (`src/master_git.rs:225-232`) | |
| `signing_key` | ADAPTED | `commit.signing.key` (`src/master_git.rs:237`) | Must be a 16–64 hex OpenPGP fingerprint or an absolute SSH key path (`:274-284`). The original also accepted an inline SSH public key (`Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:11`); Sirius rejects inline key material. |
| `signing_program` | REUSED | `commit.signing.program` (`src/master_git.rs:238`, `:286-298`) | Restricted to one absolute executable path. |
| `proxy` (absent inherits, `""` means direct) | DECISION | `remote.proxy_url_env`; omitted means direct (`src/master_git.rs:828`, `:912`, `:922-924`) | There is no inheritance because there is no global proxy. Ambient `*_PROXY` variables are removed from the Git environment (`src/git_process.rs:201-213`). See [Decisions](#decisions). |
| (implicit) worktree = `master_dir` plus its `origin` | ADAPTED | Separate `state_directory`, which must differ from `master_directory`, and an explicit `remote.url` (`src/master_git_worker.rs:14`, `:57-61`; `src/master_git.rs:826`) | Each commit is built from a fresh tree of the verified snapshot (`src/master_git.rs:384-532`). Since 1.2.1 `layout` (`native` or `indented_root`) and `branch` (default `master-data`) choose the tree and the published branch (`src/master_git_worker.rs:18-23`, `src/master_git.rs:38-125`). |
| (implicit) 120 s per networked Git command plus `http.lowSpeedLimit=1000` / `http.lowSpeedTime=30` (`Haruki-Sekai-API@9a53714:src/updater/git.rs:14-25`, `:325-326`) | ADAPTED | `master_git.timeout_seconds` (1.3.0; default 120, 10–600, `src/master_git_worker.rs:24-26`, `src/master_git.rs:49-106`), one deadline for all Git commands of a publication or adoption attempt (`src/master_git.rs:665`, `:1117`), plus the same low-speed abort on every remote command (`src/master_git.rs:916-918`) | The original budget is per command and hard-coded; Sirius keeps a single total deadline so a publication cannot run several budgets back to back, and makes it configurable for large first pushes. The CLI can override it with `SIRIUS_MASTER_GIT_TIMEOUT_SECONDS` ([Decision 7](#decisions)). |

## `servers.<region>`

Original: `ServerConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:300-379`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `enabled` | ADAPTED | The region profile is present. Remote-only serving is `node_routing.local_priority: null` with targets (`src/node_routing.rs:20`, `:60`) | |
| `master_dir` | ADAPTED | `master_directory`, an immutable snapshot store with a `CURRENT` pointer (`src/config.rs:61`, `src/master.rs:443-473`) | |
| `version_path` | ADAPTED / NOT_APPLICABLE | No configurable file. `dataVersion` and `assetVersion` are recorded with each snapshot (`resource_version`, `src/master.rs:158`, `:425-433`) and published as `version.json` `{dataVersion, assetVersion}` in the `master_git.layout: indented_root` tree (`src/master_git.rs:195-203`, `:498-505`) | The original merged appVersion/appHash/dataVersion/assetVersion/assetHash/cdnVersion into an operator-chosen file (`Haruki-Sekai-API@07da6b80:src/updater/master.rs:1066`, `:1195-1245`) and took app-identity overrides into it (`Haruki-Sekai-API@07da6b80:src/api/internal.rs:253-257`). The data/asset version pair is ADAPTED as above, taken from one VERSION observation (`src/client.rs:1172-1213`). appVersion, appHash, assetHash and cdnVersion are Sekai app-hash and CDN state and remain NOT_APPLICABLE; Sirius has no login version file (Global PlayerLogin sends the static `client_version`). |
| `account_dir` | DECISION | `accounts[].{player_id_env, credential_env, credentials_file}` for JP and `accounts[].global_identity_file` plus `global_login` for HK/EN/KR (`src/accounts.rs:24-34`, `:143-222`, `:369-410`), reloaded with `POST /internal/v1/accounts/reload` (`src/api.rs:137`) | The original polled the directory every 5 s (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:300-333`), parsed every `*.json` file as a CP (`userId`/`deviceId`/`credential`) or Nuverse (`userId`/`deviceId`/`accessToken`) account (`Haruki-Sekai-API@07da6b80:src/client/account.rs:41-56`, `:100-120`, `Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:335-360`) and logged every account in eagerly (`:226-290`). Those Sekai account files are not accepted: a JP account file holds exactly `player_id` and `credential`, and a Global identity file holds an SDK guest identity and device context (schema 1, [ACCOUNTS.md](ACCOUNTS.md#global-accounts)). The directory watch itself is a decision; see [Decisions](#decisions). |
| (implicit) account login at load and relogin (CP `PUT /api/user/{id}/auth`, Nuverse `POST /api/user/auth`) | NOT_APPLICABLE | none | Sekai login with msgpack/AES payloads (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:713-757`). Sirius JP accounts have no login step: the static credential is sent as gRPC metadata. Sirius has its own Global login, new configuration rather than a restoration of this path: a lazy OneSDK guest `cache.login` plus the gRPC `PlayerLogin`, serialized by the account's session lock and bounded by `global_login` (`src/client.rs:751-885`, `src/global_account.rs:29-81`, `:196-235`, `src/accounts.rs:320-350`). Guest identities are created only by the one-shot `global-account bootstrap` command (`src/main.rs:397-422`). |
| `api_url` | REUSED | `endpoint`, an HTTPS origin checked against the region's known services (`src/config.rs:40`, `:380-402`) | The Global SDK login uses a separate `global_login.sdk_origin`, restricted to the three official OneSDK origins (`src/global_sdk.rs:11-44`). |
| `nuverse_master_data_url` | NOT_APPLICABLE | none | Nuverse master download (`Haruki-Sekai-API@07da6b80:src/updater/master.rs:789-791`). Sirius has no Nuverse region; Global Master data uses the same CDN manifest pipeline as JP from `default_cdn_root`. |
| `nuverse_schema_bundle_path` | NOT_APPLICABLE | none | Loaded only for non-CP regions (`Haruki-Sekai-API@07da6b80:src/main.rs:141-153`). The Sirius protocol schema is `protocol_directory` (`src/config.rs:32-33`, family default at `:226-234`). |
| `require_cookies` | NOT_APPLICABLE | none | JP Sekai cookie (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:95`). |
| `headers` (free-form map) | DECISION | none | Merged into every Sekai HTTP request (`Haruki-Sekai-API@07da6b80:src/client/sekai_client.rs:91`, `:439-457`) next to computed `X-App-Hash`/`X-Data-Version` (`:189-191`). See [Decisions](#decisions). |
| `aes_key_hex`, `aes_iv_hex` | NOT_APPLICABLE | none | Sekai msgpack/AES API payload cipher. Sirius game traffic is plain Protobuf gRPC over verified TLS (`src/client.rs:1064-1111`). In the original, these keys were also the Master fallback cipher; that role is covered by the next row. |
| `master_aes_key_hex`, `master_aes_iv_hex` | ADAPTED | `master_update.key_hex_env` / `iv_hex_env` (`src/config.rs:143-144`, `src/master_update.rs:183-186`); the `master-import` CLI reads `SIRIUS_MASTER_KEY_HEX` / `SIRIUS_MASTER_IV_HEX` (`src/main.rs:271-278`) | The secrets moved into env vars. |
| `enable_master_updater` | ADAPTED | Presence of `master_update` (`src/config.rs:62`) | Mutually exclusive with `master_sync` (`src/config.rs:282-292`). |
| `master_updater_cron` | DECISION | `master_update.interval_seconds`, 60–86400 (`src/config.rs:145`, `:317`) | Runs once at startup, then waits this interval after each completed attempt. See [Decisions](#decisions). |
| `enable_app_hash_updater`, `app_hash_updater_cron` | IGNORED_BY_ORIGINAL / NOT_APPLICABLE | none | Deprecated and ignored (`Haruki-Sekai-API@07da6b80:src/config.rs:333-340`, warned at `:579-584`). |
| `upstreams[]` | ADAPTED | `node_routing.targets[]` | See [`upstreams`](#serversregionupstreams). |
| `local_priority` (default 0) | REUSED | `node_routing.local_priority` (default `0`, `null` means remote-only; `src/node_routing.rs:20`, `:31`) | |
| `master_sync` | ADAPTED | See [`master_sync`](#serversregionmaster_sync). | |
| `master_remote_source` | NOT_APPLICABLE | See [`master_remote_source`](#serversregionmaster_remote_source). | |
| `cache_ttls` | ADAPTED | See [`cache_ttls`](#serversregioncache_ttls). | |
| `prune_stale` | ADAPTED | none needed | Each Sirius snapshot is exactly the verified manifest set in a new directory (`src/master.rs:373-437`). Tables dropped upstream are absent from the next snapshot and from the next Git tree without deleting files in place. |
| `prune_min_ratio`, `prune_max_files` | DECISION | none | Mass-deletion guard (`Haruki-Sekai-API@07da6b80:src/config.rs:361-368`, `Haruki-Sekai-API@07da6b80:src/updater/prune.rs:66-87`). See [Decisions](#decisions). |
| `prune_protect` | NOT_APPLICABLE | none | Extends a built-in list of Sekai consumer tables such as `events` and `worldBlooms` (`Haruki-Sekai-API@07da6b80:src/updater/prune.rs:31-59`, `Haruki-Sekai-API@07da6b80:haruki-sekai-configs.example.yaml:112-123`). Sirius has no table deletion to protect against. |
| `prune_pending_path` | NOT_APPLICABLE | none | State for the original's two-dump prune rule (`Haruki-Sekai-API@07da6b80:src/updater/prune.rs:106-112`). There is no in-place prune. |

### `servers.<region>.upstreams[]`

Original: `UpstreamConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:177-189`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `url` | ADAPTED | `node_routing.targets[].origin`, plus `regional_paths` and `allow_http` (`src/node_routing.rs:41-52`) | Sirius routes verified public reads only; for HK/EN/KR these are the operations in `src/routes.rs:29-40` (`src/peer_transport.rs:126-128`). |
| `token` | ADAPTED | `targets[].token_env`, which is the remote's `peer_token_env`, not its internal token (`src/node_routing.rs:46`) | |
| `priority` (default 10) | REUSED | `targets[].priority`, default 10 (`src/node_routing.rs:47-48`, `:53-55`) | |
| `name` | REUSED | `targets[].name`, required and unique, and must not be `local` (`src/node_routing.rs:44`, `:73-81`) | |

Sirius adds `timeout_ms`, `max_inflight`, `failure_threshold`, `cooldown_ms` and `transport`
(`src/node_routing.rs:17-27`). These are validated even when `targets` is empty (`:58-70`) and
have no effect until a target exists. They are never silently accepted with invalid values. Peer transport ignores ambient proxies (`src/peer_transport.rs:105`).

Failover differs on purpose: the original fails over on any non-2xx peer answer for every
request, while Sirius fails authenticated reads over only when the attempt provably did not
execute, including a fixed set of pre-dispatch HTTP statuses since 1.3.0
([Decision 29](#decisions)).

### `servers.<region>.master_sync`

Original: `MasterSyncConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:195-244`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `source_url` | ADAPTED | `master_sync.origin`, plus `regional_paths` and `allow_http` (`src/master_sync.rs:13-27`) | Presence enables it. Requires `master_directory` and excludes `master_update` (`src/config.rs:282-292`). |
| `source_token` | ADAPTED | `master_sync.token_env`, the owner's public read bearer | |
| `poll_cron` (empty = webhook only) | DECISION | `master_sync.interval_seconds`, 60–86400 (`src/master_sync.rs:22`, `:36`); hints arrive at `POST /internal/v1/master-data/sync` (`src/api.rs:143-146`) | Polling is always on. Sirius adds `timeout_seconds` and `request_timeout_ms` (`src/master_sync.rs:23-26`). See [Decisions](#decisions). |
| `notify[]` (`MasterSyncPeer`: `url`, `token`) | ADAPTED | `master_notify.targets[].{name, origin, token_env, regional_paths, allow_http}`, `interval_seconds`, `request_timeout_ms` (`src/master_notify.rs:123-148`) | JP/HK/EN/KR, CN rejected (`:150-170`). Adds per-target retry and deduplication. |

### `servers.<region>.master_remote_source`

Original: `MasterRemoteSourceConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:206-221`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `url`, `token` | NOT_APPLICABLE | none | The original borrowed a peer's game accounts for the login-derived version probe and the CP authenticated master-split fetch (`Haruki-Sekai-API@07da6b80:src/config.rs:206-211`, used at `Haruki-Sekai-API@07da6b80:src/updater/scheduler.rs:158-194`). In Sirius the version probe is the anonymous `Version` RPC on every family, which is not in `authenticated()` (`src/client.rs:25-36`); Global PlayerLogin is never used to observe versions. Master download uses CDN credentials (`master_update.username_env` and `cdn_credential_env`), or none for Global with `cdn_authorization: none` (`src/master_update.rs:163-186`). No game account is involved, so there is nothing to borrow. Peer queries never carry accounts (`src/peer.rs:29-48`). |

### `servers.<region>.cache_ttls`

Original: `CacheTtlConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:246-298`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `ranking_top100` (1 s) | ADAPTED | `response_cache.route_ttl_ms.event_rankings` (`src/response_cache.rs:11-19`, `:44`, `:56`) | Milliseconds instead of fractional seconds. Sirius also has `song_rankings` and `challenge_rankings`. |
| `ranking_border` (30 s) | ADAPTED | `event_rankings` | The Sirius protocol has one event ranking RPC (`src/routes.rs:4`), with no separate border call. |
| `static` (`/system`, `/information`, 300 s) | ADAPTED | `announcements` / `announcement` TTLs | `Version` is never cached, but identical concurrent Version calls share one RPC since 1.3.0 ([Decision 11](#decisions)). |
| `max_stale` (30 s) | ADAPTED | `stale_while_revalidate_ms` (default 0, off; `src/response_cache.rs:42`, `:54`) | Opt-in instead of on by default. |

## `registry`

Original: `RegistryConfig` / `MusicMetasConfig`, `Haruki-Sekai-API@07da6b80:src/config.rs:393-452`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `host`, `port` (`0.0.0.0:9998`) | ADAPTED | `listen` (required, `src/registry_service.rs:23`) | Also `tls`, `logging`, `access_log` and `http_compression` (`:33-37`, `src/main.rs:11`, `src/registry_service.rs:220`, `:254-258`). |
| `token` (mutations only, reads open) | ADAPTED | `token_env` for reads (reads are authenticated) and `owner.internal_token_env` for internal routes (`src/registry_service.rs:24`, `:105-116`; `src/registry_owner.rs:13`) | Stricter than the original. |
| `state_dir` | ADAPTED | `backend: {kind: files, directory}` (`src/registry_service.rs:39`) | |
| `state_dsn` | ADAPTED | `backend: {kind: postgres, connection}` (`src/registry_service.rs:40`) | |
| `subscribers[]` | ADAPTED | `notify`, a `master_notify` config (`src/registry_service.rs:32`) | |
| `account_nodes[]` | NOT_APPLICABLE | none | Pushes the Sekai appVersion/appHash to `POST /internal/app-identity` (`Haruki-Sekai-API@07da6b80:src/config.rs:424-427`, `Haruki-Sekai-API@07da6b80:src/api/internal.rs:216-257`). Sirius `client_version` is static configuration (`src/config.rs:41`), also on Global where PlayerLogin sends it (`src/global_account.rs:253-270`); protocol reload does not change it (`docs/PROTO_RELOAD.md:36`). No node needs an app identity pushed to it. |
| `music_metas.{enabled, cron, inject_omakase, sources, proxy}` | NOT_APPLICABLE | none | Pulls Sekai `music_metas*.json` from sekai-data.3-3.dev and injects the synthetic omakase rows (`Haruki-Sekai-API@07da6b80:src/registry/metas.rs:1-42`, `Haruki-Sekai-API@07da6b80:src/config.rs:439-441`). This is a Sekai-specific data feed with no Sirius counterpart. |
| (implicit) all regions in one registry | ADAPTED | One `scope` per process, JP/HK/EN/KR, with the deprecated `tw` scope alias read as `hk` (`src/registry_service.rs:25-26`, `:57`; `src/master_registry.rs:55-71`) | `regional_paths` uses the scope's region. |

## `asset_updater_servers[]`

Original: `AssetUpdaterInfo`, `Haruki-Sekai-API@07da6b80:src/config.rs:508-513`.

| Original field | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `url` | ADAPTED | `asset_dispatch.targets[].origin` (`src/asset_dispatch.rs:27`) | Adds a durable outbox (`state_directory`, `history_capacity`), profile/revision identity and completion requirements (`:15-38`). HK/EN/KR dispatch from schema-3 resource snapshots when `resource_snapshot` is configured (`src/asset_dispatch.rs:236-252`, `src/client.rs:412`). Worker health is at `GET /internal/v1/asset-dispatch/status`, never echoing the origin ([Decision 18](#decisions)). |
| `authorization` | ADAPTED | `targets[].token_env` (`src/asset_dispatch.rs:28`) | Adds optional `user_agent`. |

## Environment variables

| Original variable | Status | Sirius mapping | Evidence/notes |
| --- | --- | --- | --- |
| `CONFIG_PATH` (`Haruki-Sekai-API@07da6b80:src/config.rs:591`) | ADAPTED | `SIRIUS_CONFIG_PATH`, default `sirius-api-config.yaml` (`src/main.rs:47-50`, `:180-181`, `:290-291`, `:444-445`) | `registry-serve` and `master-db-import` / `master-db-migrate` take their config path from argv (`src/main.rs:5-10`, `:73-81`). |
| `RUST_LOG` (`Haruki-Sekai-API@07da6b80:src/logging.rs:28`) | ADAPTED | Ignored on purpose; use `logging.level` | `docs/APPLICATION_LOG.md:39`. |
| `BENCH_*` (`Haruki-Sekai-API@07da6b80:src/bin/bench_profile.rs:178-337`), `HARUKI_BENCH_*` (`Haruki-Sekai-API@07da6b80:src/updater/master_stream.rs:628-732`) | NOT_APPLICABLE | none; the ignored `perf_stages` test (`src/tests.rs`) measures Sirius codec, Master, registry and cache stages without environment variables or configuration | Benchmarks for Sekai profile and master ingest. See [Decisions](#decisions). |
| `HARUKI_TEST_REGISTRY_DSN` (`Haruki-Sekai-API@07da6b80:src/registry/state.rs:1025`) | ADAPTED | `SIRIUS_TEST_POSTGRES_PORT` / `SIRIUS_TEST_POSTGRES_PASSWORD`; `SIRIUS_TEST_REDIS_SERVER` and `SIRIUS_TEST_GPG_PROGRAM` enable other optional tests (`src/tests.rs`) | Test-only. Never read by the service. |

Other environment reads in Sirius:

- Every `*_env` field is a variable name, and its value is resolved by `secret()`
  (`src/config.rs:179-188`). YAML never holds a secret value.
- `master-import` reads `SIRIUS_MASTER_KEY_HEX` / `SIRIUS_MASTER_IV_HEX` (`src/main.rs:271-278`).
- `master-git-push` and `master-git-adopt` read `SIRIUS_MASTER_GIT_PROXY_URL` /
  `SIRIUS_MASTER_GIT_AUTHORIZATION` (`src/main.rs:480-492`). Since 1.3.0 all three Master Git
  commands also read `SIRIUS_MASTER_GIT_TIMEOUT_SECONDS` (`src/main.rs:200-205`). See
  [Decisions](#decisions).
- `global-account bootstrap` reads the SDK app key from `--sdk-app-key-env`, default
  `SIRIUS_GLOBAL_SDK_APP_KEY` (`src/main.rs:419-423`); the service reads it from
  `global_login.sdk_app_key_env` (same default, `src/global_account.rs:48-60`, `src/client.rs:1448`).
- PostgreSQL connections refuse to start while `PGSSLROOTCERT`, `PGSSLCERT`, `PGSSLKEY` or
  `PGOPTIONS` is set; other SQLx-read `PG*` variables are always overridden by explicit
  configuration (`src/master_database.rs:148-154,182-196`).

## Decisions

These generic capabilities are intentionally not restored in their original form.

1. **Prune mass-deletion guard (`prune_min_ratio`, `prune_max_files`) is not restored.** The guard
   protected a flat `master_dir` from being emptied when a partial dump was pruned in place.
   Sirius has no in-place prune:
   - `Manifest::parse` rejects empty, oversized or malformed manifests (`src/master.rs:228-261`).
   - Every listed table must be read and decoded into a fresh staging directory, and any failure
     aborts the import (`src/master.rs:373-437`).
   - The staged snapshot is renamed into place and `CURRENT` is switched atomically
     (`src/master.rs:443-473`).

   A partial download therefore cannot publish. File-store snapshots are never deleted by Sirius
   unless `master_retention` or a registry `owner.retention` is configured, and even then only
   along the committed chain behind an explicit boundary, never by mtime or directory scan
   ([Decision 19](#decisions)). Git keeps every earlier commit. The PostgreSQL mirror only drops whole snapshots beyond
   `keep_snapshots` (`src/master_database.rs:393-394`). A table that upstream really drops is
   absent from the next snapshot, as the manifest says, and all earlier snapshots remain
   readable. No ratio threshold would prevent a verified manifest from being published.
2. **Cron syntax became fixed intervals.** `master_update.interval_seconds`,
   `master_sync.interval_seconds` and the Git and database worker intervals replace 6-field cron.
   Each worker runs one attempt at a time and sleeps the interval after it, so runs never
   overlap (for example `src/master_sync.rs:365`). Wall-clock schedules are not reproduced. Owners send immediate hints through
   `master_notify`, and `POST /internal/v1/master-data/sync` triggers a sync on demand.
3. **Account-directory auto-watch became an explicit reload.** Accounts are listed explicitly and
   reloaded with `POST /internal/v1/accounts/reload` (`src/api.rs:137`,
   [ACCOUNTS.md](ACCOUNTS.md)). The reload validates every source, including every Global
   identity file, drains active calls and swaps the pool atomically. A failed candidate keeps
   the previous pool. Global login rate history survives the reload (`src/accounts.rs:472`), so
   a reload cannot be used to exceed `global_login.max_logins_per_day`. A directory poller
   provides no such barrier.
4. **The free-form `headers` override is not restored.** Sirius sends a fixed gRPC metadata set
   built in `src/client.rs:1069-1111`: content type, `user-agent`, `te`, `grpc-accept-encoding`,
   `grpc-timeout`, `x-platform` and `x-client-version` from `platform`/`client_version`,
   `x-request-id`, the observed `x-master-version` (not on Global `PlayerLogin`), and account
   headers only on authenticated routes (on Global also `x-resource-version` and the SDK uid as
   `x-player-bid`). Request and response message encoding comes from the loaded, verified
   protocol bundle. The Sekai headers that motivated the map (`X-App-Hash`, `X-Data-Version`,
   cookies) do not exist in this protocol, and arbitrary extra metadata would be sent unverified
   on anonymous and authenticated calls alike.
5. **The global proxy with inheritance became explicit per-transport proxies.** Game gRPC and
   the Global SDK login of the same profile (`upstream.*`), the Master CDN
   (`master_update.network.*`), the Global catalog `.hash` (`resource_snapshot.network.*`) and
   Git (`master_git.remote.*`) each take their own proxy URL and authorization env references.
   An omitted proxy means a direct connection. Peer, sync, notification, asset-dispatch and SDK
   clients ignore ambient proxies (`src/peer_transport.rs:105`, `src/master_sync.rs:157`,
   `src/master_notify.rs:60`, `src/asset_jobs.rs:105`, `src/global_sdk.rs:342`), and Git strips
   `*_PROXY` variables (`src/git_process.rs:201-213`). The one-shot `global-account bootstrap`
   reads no configuration and connects directly. Each proxy credential stays scoped to one
   destination, and one dead proxy cannot take down unrelated traffic. The original needed an
   empty-string override for exactly that case (`Haruki-Sekai-API@07da6b80:src/config.rs:445-451`).
6. **Global example CDN roots are the verified server-list roots.** Since 1.2.1,
   `docs/examples/{en,hk,kr}.yaml` and the Global entries of
   `sirius-multi-region-config.example.yaml` use the first CDN line of each region's public
   server-list entry, with `cdn_credential_env: {}`. The Global Master CDN needs no credential,
   and no Global CDN credential is verified or shipped. Startup never contacts the CDN: the
   optional `master_update`, `resource_snapshot` and account lines stay commented out, so the
   packaged smoke test still starts a Global service offline from the shipped example
   (`scripts/smoke-release.py:121-157`). Earlier releases used `https://cdn.example.invalid/...`
   placeholders.
7. **The Master Git CLI takes its state directory and remote from argv and env.**
   `master-git-commit STATE_DIR`, `master-git-push STATE_DIR REMOTE_URL` and, since 1.3.0,
   `master-git-adopt STATE_DIR REMOTE_URL` (`src/main.rs:169-244`) are explicit one-shot
   operations. They parse and validate the whole profile, then use only `master_directory`, the
   scope, `master_git.commit` (identity and signing), since 1.2.1 `master_git.layout` and
   `master_git.branch`, and since 1.3.0 `master_git.timeout_seconds` (`:185-205`, `:218-221`).
   `SIRIUS_MASTER_GIT_TIMEOUT_SECONDS` (10–600, digits only) overrides the time budget for one
   run (`:200-205`); it selects no target, so it cannot redirect the operation. `master-git-adopt`
   uses only the scope, layout, branch and time budget (`:206-217`): it neither requires
   `master_directory` nor applies the commit policy. All three ignore
   `master_git.state_directory`, `interval_seconds` and `remote.*`.
   The push and the adoption read their proxy and authorization only from
   `SIRIUS_MASTER_GIT_PROXY_URL` / `SIRIUS_MASTER_GIT_AUTHORIZATION`, fix `allow_http: false`,
   and set `allow_file` only for a `file://` argument (`cli_remote`, `:480-492`). The background
   worker honors every `master_git` field (`src/master_git_worker.rs:163-182`) and never adopts
   remote history. The split keeps a manual push or adoption from silently targeting the
   service's configured remote or state. The original has no adoption: it clones and leaves
   diverged history to a manual merge (`Haruki-Sekai-API@9a53714:src/updater/git.rs:194-295`,
   `:892-953`); Sirius adopts only a recognizable Sirius publication by fast-forward
   ([MASTER_REGISTRY.md](MASTER_REGISTRY.md#adopting-remote-history)).
8. **Client authorization deviates from the original.** It fails closed, uses the Sirius header
   and table names, enforces `exp`, and uses an in-process cache instead of Redis. See
   [CLIENT_AUTH.md](CLIENT_AUTH.md#differences-from-the-original).
9. **Error bodies keep the Sirius shape.** The original answers `{result, status, message}` and
   passes the game's 400/404/409 bodies through (`Haruki-Sekai-API@9a53714:src/error.rs`,
   `src/client/sekai_client.rs`). Sirius keeps `{error}` for v1 compatibility, adds a stable
   `code` (and `grpc_status` for game failures) since 1.2.4, and never echoes upstream bodies or
   grpc-message values. Maintenance maps to 503 like the original (1.2.4). The original's 426
   for an outdated app is not reproduced: the game's `CLIENT_UPDATE_REQUIRED` stays
   `upstream_grpc` and is visible as `observation.application_code` in `/system`, because it
   means the operator must raise `client_version`, not that the caller should retry. Since
   1.3.0 it never counts against the account ([Decision 14](#decisions)).
10. **Busy updaters are retried by status, not by body text.** The original retries a 409 every
   60 s up to 10 times and treats a body containing "is disabled" as permanent
   (`Haruki-Sekai-API@9a53714:src/updater/master.rs`). The Sirius updater answers busy with
   429/503 and uses 409 for an Idempotency-Key conflict, so Sirius resubmits 429/503 (same key,
   one reconciliation interval apart, 10 times) and treats 409 and other 4xx as terminal without
   reading the body. See [ASSET_DISPATCH.md](ASSET_DISPATCH.md#durable-state-and-recovery-boundaries).
11. **Request coalescing is in-process, keyed by the Sirius protocol identity, and opt-in for
   rankings.** The original shares one execution among identical in-flight requests with a
   `OnceCell` map and a pointer-checked drop guard, independent of Redis and shared errors
   included (`Haruki-Sekai-API@9a53714:src/lib.rs:21-92`, `src/api/apis.rs:131-172`,
   `:204-237`). Since 1.3.0 Sirius does the same at two points, independent of the response
   cache: `GameClient::call_selected` and, under node routing, `Router::call`
   (`src/single_flight.rs`). Differences: the key covers region, environment, endpoint, platform,
   client version, protocol fingerprint and generation and the fingerprint a peer caller asserted,
   not a URL; the entry is removed before the outcome is published, so a finished result is never
   handed to a later caller; each joined caller waits only within its own deadline. Version,
   server list and announcements always share. Rankings share only with
   `upstream.coalesce_public_reads`, because in Sirius they spend a game account and one
   account's failure would answer every joined request (the original has no anonymous calls to
   distinguish). The Version bootstrap of authenticated calls never joins: it already holds an
   admission permit and the protocol barrier, and waiting there for a flight that still needs
   either would deadlock until the deadline. The 1.2.x per-region anonymous call lock is replaced
   by `upstream.anonymous_max_inflight` (1 restores it).
12. **Path faults open a per-region path breaker instead of cooling accounts.** The original has
   no account health; target faults (network, invalid HTTP status, upstream data and account
   errors alike) count toward a per-target circuit breaker whose expiry is the half-open probe
   (`Haruki-Sekai-API@9a53714:src/upstream.rs:185-260`). Sirius keeps account health and the node
   router's breaker, and since 1.3.0 adds a breaker for the shared upstream path of each region
   (`src/path_health.rs`), plus one for the Global SDK. A run of transport, protocol, deadline or
   bare gRPC 14 faults stays the account's while one account saw it, and becomes the path's once
   a second account or any anonymous call fails in it: the run's account charges are withdrawn
   (`src/accounts.rs:333-344`, `src/client.rs:508-523`) and the path opens at
   `account_pool.failure_threshold`, refusing new calls with 503 `upstream_unavailable` before any
   upstream contact or login (`src/client.rs:561-567`, `:831`, `:919`, `:1053`). Differences:
   attribution by distinct sources (the original cannot tell an account fault from a path
   fault), one probe at a time every min(`cooldown_seconds`, 5 s), admission once per logical
   call, gRPC 8/13 remain account faults, and SDK transient failures never cool an account. No
   configuration is added.
13. **Game connections use HTTP/2 PING keepalive, not TCP keepalive.** The original enables only
   TCP keepalive on its game client (`Haruki-Sekai-API@9a53714:src/client/sekai_client.rs:49-54`),
   which the kernel acts on after minutes. Sirius's game client multiplexes calls over pooled
   HTTP/2 connections whose idle timer is renewed by every call, so a blackholed connection kept
   every call on it waiting for its deadline (504, not retried, charged like any timeout). Since
   1.3.0 the pool sends HTTP/2 PINGs with `while_idle` off (`src/client.rs:220-230`): only while
   a call is open on a connection silent for `upstream.http2_keepalive_interval_ms`, and a missed
   acknowledgement within `http2_keepalive_timeout_ms` closes the connection, failing its calls
   as `upstream_transport` (502) so the next call reconnects. Differences: defaults are derived
   from `timeout_ms` (min(10 s, 1/2) and min(5 s, 1/4)) so interval + timeout stays below the
   deadline and every 1.2.x configuration still validates; they stay off below a 1 s
   acknowledgement (`timeout_ms` < 4000) unless a key is set; explicit values must sum to less
   than `timeout_ms`; `http2_keepalive_interval_ms: 0` turns them off. The official game client
   configures `Http2KeepAliveInterval`/`Http2KeepAliveTimeout` on its HTTP/2 handler (no
   `WhileIdle`), so PINGs during an open call match its behavior. Peer, SDK, CDN and Git clients
   are unchanged.
14. **The Master version header is refreshed ahead of time, not on a 426.** The original learns
   that its app/data version is outdated from Sekai's HTTP 426, then refreshes and replays the
   request (`Haruki-Sekai-API@9a53714:src/client/sekai_client.rs:848-959`); that mechanism is
   Sekai-specific and is not restored. Before 1.3.0 Sirius read `x-master-version` once and never
   refreshed it while the process ran, and a game answer of gRPC 7/16 about a stale version
   would have disabled the account. Since 1.3.0 a call refreshes the header by a Version call
   first when it is older than `upstream.version_max_age_seconds` (600 s, 60..86400), or after a
   response carried `MASTER_VERSION_MISMATCH` for the header in use (`src/client.rs:1089-1167`,
   `:1733-1761`); `MASTER_VERSION_MISMATCH` and `CLIENT_UPDATE_REQUIRED` never penalize a JP or
   Global account (`src/accounts.rs:417`, `:634`). Differences: authenticated calls are never
   replayed (the call that received the code still fails); the version is read only from
   Version, never from an error trailer, so the Master and asset versions stay one pair; a failed
   refresh keeps the previous header for 30 s instead of failing the call; detection keys on the
   application code, found statically in the JP 1.0.3 and Global 1.0.1 clients, because the gRPC
   status that comes with it and whether the game enforces freshness at all are unverified.
15. **Response-cache hits are answered before admission, with per-account keys kept.** The
   original reads its cache first and returns a hit before coalescing or any upstream work
   (`Haruki-Sekai-API@9a53714:src/api/apis.rs:203-221`). Before 1.3.0 Sirius looked up the cache
   only after the regional admission permit, the protocol barrier, account selection and the
   Version bootstrap, so during an outage a hit queued behind stuck permits and, with every
   account quarantined, even an in-window stale entry answered 503. Since 1.3.0
   `GameClient::cached_before_admission` (`src/client.rs`) runs first, keyed exactly as the
   admitted call would be, with the account that `Pool::peek_public` reports without leasing it
   (`src/accounts.rs`). Differences: keys stay per account; named-account calls, refreshes, known
   maintenance, a needed Version bootstrap or a reported stale header, and a mismatched peer
   schema skip the early hit; the quarantine fallback reads other accounts' retained entries only
   with an explicit stale window, through bounded GETRANGE pipelines of at most four keys (not
   MGET), and never refreshes or reports health. No configuration is added.
16. **Master history events carry generic metadata, not Sekai's publish record.** The original
   registry records each publication with its versions and totals
   (`Haruki-Sekai-API@9a53714:src/registry/state.rs:53-82`, `:329-361`). Before 1.3.0 the
   Sirius PostgreSQL history kept only scope, content hash and time, and did not return the time;
   once a snapshot was pruned, nothing said what it had been. Since 1.3.0 every event also stores
   the Master `version`, asset `resource_version`, file count and plaintext byte total, and the
   history API returns them with `published_at` (`src/master_database.rs:279-291`, `:379-387`,
   `:606-667`, `:793-814`); file history adds `resource_version` (`src/master_registry.rs:394-404`).
   Differences: `app_version` and `cdn_version` are Sekai concepts and are not restored; a Git
   commit is not recorded, because Git is an asynchronous downstream mirror and one content hash
   can map to several commits; events written before 1.3.0 are not backfilled and report null;
   the migration receipt hash keeps its 1.2 format (`src/master_database.rs:460-508`). No
   configuration is added.
17. **PostgreSQL reads get their own deadline, without a breaker or fallback.** The original
   registry blob store gives reads a 2-second budget, trips a 5-second breaker and falls back
   to disk (`Haruki-Sekai-API@9a53714:src/registry/blobs.rs:71-84`, `:439-530`, `:714-728`).
   Before 1.3.0 one Sirius `timeout_seconds` (default 120) bounded publication and every read,
   including the read pool's acquire wait and each read connection's server
   `statement_timeout`/`lock_timeout`. Since 1.3.0 `connection.read_timeout_seconds` (1–600,
   default min(`timeout_seconds`, 30)) bounds each HTTP/registry read end to end and sets those
   server timeouts on read connections only, and waiting for a read connection gives up after
   5 s, or the read deadline if shorter (`src/master_database.rs:72`, `:125-180`, `:674-702`).
   Differences: the default is not 2 s, because a verified read decodes, hashes and checks tables
   of up to 64 MiB inside the deadline; there is no breaker and no fallback, because Sirius
   backends never fall back to each other and no production deployment reads Master data from
   PostgreSQL yet, so a breaker would only turn a 503 after 5 s into an immediate 503. An expired
   read answers the existing 503 `master_unavailable`. Publication, import and migration keep
   `timeout_seconds`.
18. **Dispatch worker health is an internal status route, not `/health`.** The original reports
   a failed mirror or updater push as `degraded` in its always-200 `/health`, with a closed-set
   reason instead of Git error text (`Haruki-Sekai-API@9a53714:src/registry/http.rs:115-131`,
   `src/updater/sync.rs:69-99`). Sirius keeps `/health` a liveness check. The asset dispatch
   worker publishes its state through a watch channel to
   `GET /internal/v1/asset-dispatch/status` under the internal token (`src/asset_dispatch.rs`,
   `src/asset_dispatch_admin.rs`): always 200, independent of the command queue, readable
   after the worker stopped, and made only of timestamps, counts, the recorded resource version
   and closed-set codes. Unknown persisted failure codes count as `other`. No field is added.
19. **File snapshot retention follows the committed chain, is bounded below and is off by
   default.** The original keeps the newest `MANIFEST_SNAPSHOTS_KEPT = 20` manifest snapshots per
   region, chosen by scanning the snapshot directory and sorting by mtime, and deletes the rest
   (`Haruki-Sekai-API@9a53714:src/registry/state.rs:93`, `:492-512`). Sirius does not reproduce
   the scan or the mtime order: a directory also holds staging, download and sync temporaries,
   orphans of failed pointer switches and legacy snapshots, and clock changes would reorder it.
   The optional `master_retention` and registry `owner.retention` (`keep_snapshots`, 2..10000)
   count installations along CURRENT's committed predecessor chain (`src/master_registry.rs`
   `prune`). The minimum of 2 keeps the snapshot CURRENT just replaced readable for pinned
   readers. The oldest retained snapshot is durably recorded in `retention.json` before anything
   is removed, so history, lookup by content identity, bundles and migration stop there and
   report `retention_boundary` instead of failing on missing predecessors. A pass runs after the
   update or sync result is settled (`retain`, called from `src/master_update.rs` and
   `src/master_sync.rs` `update_once`), under the writer lock and outside the update deadline,
   and removes at most 64 snapshots, oldest first. Nothing is pruned unless configured.
20. **The Docker build caches dependencies, but CI does not reuse release binaries.** The
   original splits its builder into cargo-chef planner and cook stages on the plain
   `rust:*-alpine` image, installing `cargo-chef --version 0.1.78 --locked` rather than using a
   cargo-chef base image, and builds from a whitelist `.dockerignore`
   (`Haruki-Sekai-API@9a53714:Dockerfile:1-18`, `.dockerignore`). Sirius adopts the dependency
   layer the same way: the planner also copies `build.rs`, so the cook compiles the protobuf
   build-dependencies, and `ARG VERSION` is declared after the cook, so tag and `dev` builds
   share it. cargo-chef masks the package version and replaces targets with `fn main() {}`
   stubs; the real build recompiles the crate and reruns `build.rs`, and the container smoke
   test's `codec == native` proves the generated codecs. The runtime stage is unchanged. The
   context whitelist keeps local configuration, `*.env` and private files out of the builder.
   CI reuse of release binaries is not adopted: main pushes build `VERSION=dev` images and
   publish nothing, so there is no artifact to reuse.
21. **`/health` uptime counts from `main()` and stays liveness only.** The original starts its
   `uptime_secs` clock when the API router is built and reports it only on the API server
   (`Haruki-Sekai-API@9a53714:src/api/routes.rs:20-44`). Sirius records a monotonic `Instant`
   as the first statement of `main()` (`src/main.rs`, `src/api.rs` `mark_started`), so the
   value includes configuration loading and bind time, and shares one `health_body` across the
   single-profile, multi-region and registry servers (`src/registry_service.rs`). The body is
   exactly `status`, `service`, `version` and `uptime_secs`: no wall-clock start time and no
   readiness, account, Master or upstream state. No field is added.
22. **JP `PLAYER_NOT_FOUND` answers 404 only on `profile`, on gRPC 2 or 7, from static
   evidence.** The original passes the upstream status and body through to the caller
   (`Haruki-Sekai-API@9a53714:src/api/apis.rs:57-69`). Sirius never echoes upstream bodies or
   `grpc-message`; it maps a proven code to its own 404 `not_found` without an account penalty
   (`src/client.rs` `target_not_found`). No JP account exists for a live test, so the JP scope is
   what the iOS 1.0.3 client proves: FindByProfileID expects the code, which names the target,
   and the client reads codes only on gRPC 2 or 7. JP `event_deck`, gRPC 3/5 and
   `PLAYER_NOT_EXISTS` are not mapped (see [REGIONS.md](REGIONS.md)). No configuration surface.
23. **Current Master reads revalidate with a verified, private content ETag.** The original's
   pointer files answer public `no-cache` with a strong ETag and `Last-Modified`, and return
   304 before reading the file (`Haruki-Sekai-API@9a53714:src/registry/http.rs:19-25,581-601`).
   Sirius hashes the bytes it serves from one pinned CURRENT after the region and `tables.json`
   checks (`src/master.rs` `read_current_inner`), then shares the pinned routes' matcher
   (`src/api.rs` `master_document`, `registry_document`), so a 304 never hides corruption. It
   sends `private, no-cache` like the manifest and no `Last-Modified`, whose file times differ
   per node and per reimport. No configuration surface.
24. **Response compression is opt-in, negotiated, and limited to public reads.** The original
   has no setting: `CompressionLayer::new()` wraps every route, including internal and peer
   routes (`Haruki-Sekai-API@9a53714:src/api/routes.rs:141`), and its outbound reqwest clients
   enable `gzip`, `brotli` and `zstd` (`Haruki-Sekai-API@9a53714:Cargo.toml:17`). Sirius adds a
   root `http_compression: {enabled}` block (single file, multi-region root only, registry
   root), absent by default so an unchanged configuration answers exactly as 1.2.x
   (`src/http_compression.rs`, [HTTP_COMPRESSION.md](HTTP_COMPRESSION.md)):
   - Only the public API router (`src/api.rs` `router_at`) and the registry's public Master
     routes (`src/registry_service.rs`) are wrapped. `/health`, internal (accounts, identity,
     player data, owner), peer and asset dispatch admin routes stay identity, which keeps the
     peer wire compatible with 1.2.x and keeps private output out of compression length
     oracles.
   - gzip and zstd at the fastest level; status 200 `application/json` of at least 1024 bytes
     only, so errors, 304s and unauthenticated requests never cost encoder work and bundles
     keep their exact Content-Length. Encoded responses send a weak ETag and every negotiable
     JSON response `Vary: Accept-Encoding`.
   - No request decompression: a compressed request body is parsed as-is and rejected.
   - Server-side encoders only (tower-http `compression-*` on the copy reqwest already uses).
     reqwest features are unchanged, so outbound SDK, CDN, peer, dispatch and sync requests
     keep sending no `Accept-Encoding`; tests assert the header's absence. Transport
     compression for sync and peer traffic stays refused (no WAN sync; Cloudflare already
     compresses; it would change outbound fingerprints; peers accept identity only).
   - `json_client_errors` also drops `Content-Encoding` when it replaces a non-JSON body
     (`src/error.rs`), so a rewritten error can never be labeled with the original coding.
25. **The Git link is a commit trailer, not a manifest or notification field.** The original
   reads its worktree HEAD after a publication and carries it as `gitCommit` in the registry
   notification (`Haruki-Sekai-API@9a53714:src/api/internal.rs:492-505`, `:581`;
   `src/registry/service.rs:51-58`). Sirius publishes Git asynchronously from its own managed
   repository, and identical trees reuse a commit while one content identity can span several
   commits, so a `git_commit` in manifests, history or hints stays refused. Instead each new
   commit carries `Sirius-Content-SHA256: <content_sha256>` as a trailer paragraph after the
   unchanged subject (`src/master_git.rs` `CONTENT_TRAILER`, `commit_internal`). The hash is
   checked as 64 lowercase hex before any Git command (`prepare`), so it cannot inject message
   lines; signatures cover it. Sirius never reads it back: adoption still matches the subject
   and verifies the tree. Existing commits are not rewritten and no commit is created only to
   add a trailer. No configuration surface.
26. **Unchanged polls trust the snapshot index instead of reparsing JSON; serving reads still
   validate.** The original streams its large Sekai tables through a transcoder
   (`Haruki-Sekai-API@9a53714:src/updater/master_stream.rs:1-124`); Sirius tables are small, so
   no streaming transcode is adopted. Every check that used to build and discard a JSON tree now
   walks the value once without keeping it (`src/master.rs` `validate_json`), with the same
   accepted and rejected inputs as parsing into a tree (recursion limit, UTF-8 and escapes,
   number range); `IgnoredAny` is refused because it skips those checks. Unchanged CDN and sync
   polls (`src/master_update.rs`, `src/master_sync.rs`) accept an indexed table whose length and
   SHA-256 match `tables.json`, which is written after its tables were validated
   (`src/master.rs` `current_table_intact`, `src/master_registry.rs` `table_intact`); legacy
   unindexed tables are parsed again. This is the trust the `/master-data` current read already
   extends; pinned, bundle, Git and database reads still reject invalid JSON. No configuration
   surface.
27. **Table reads are admitted through a fixed gate with its own wait.** The original holds at
   most `READ_CONCURRENCY = 16` blob responses at once, the permit travelling with the body, and
   takes the read slot inside the 2 s database budget, behind a breaker with a disk fallback
   (`Haruki-Sekai-API@9a53714:src/registry/blobs.rs:22-27`, `:64-85`, `:487-509`). Sirius admits
   every Master table read, file or database, proxy or standalone registry, through 16
   process-wide permits (`src/master_admission.rs`). A read waits in FIFO order for up to 5 s
   and then answers the existing 503 `master_unavailable`; a 200 keeps its permit until the body
   is sent or dropped, and a 304 or error releases it at once. Differences: the wait is its own
   budget before the database read deadline (Decision 17), not shared with it, because file
   reads have no database budget; there is no breaker or disk fallback (Sirius backends never
   fall back to each other); manifests, history and bundles are not admitted, bundles keeping
   their own two permits. The count and wait are fixed like the bundle gate, and no `Retry-After`
   is sent, because `master_sync` does not read it. No configuration surface.
28. **Benchmarks are one manual ignored test, not a binary or environment-configured harness.**
   The original ships a `bench_profile` binary that calls live Sekai profiles under `BENCH_*`
   variables and summarizes stage percentiles and a CPU pipeline
   (`Haruki-Sekai-API@9a53714:src/bin/bench_profile.rs:73-148`, `:437-475`), plus an ignored
   master ingest memory probe sized by `HARUKI_BENCH_*`
   (`Haruki-Sekai-API@9a53714:src/updater/master_stream.rs:614-732`). Sirius has the
   `#[ignore]` test `perf_stages` (`src/tests.rs`): native versus dynamic codec for every JP and
   Global route, the Master import stages (SHA-256, Rijndael, gunzip, JSON parse and
   validation, snapshot install), Master and registry reads, and memory response-cache
   serialization, all on deterministic local fixtures. It prints min, median and p90 and never
   asserts timings. There is no live mode, no network, PostgreSQL or Redis, no new dependency,
   and no production stage tracing. No configuration or environment surface.

29. **Peer HTTP statuses fail over authenticated reads only when they prove non-execution.**
   The original carries every node-level answer on HTTP 200 and treats any other status from
   `/internal/sekai-api` as a `NetworkError` target fault, failing over for every request and
   keeping up to 200 bytes of the body in the error
   (`Haruki-Sekai-API@9a53714:src/upstream.rs:48-50`, `:282-294`, `:186-214`). Sirius fails
   anonymous reads over on any target fault, but authenticated reads (profile, event ranking and
   deck, music and challenge ranking) only on a connection-level not-sent error, a typed
   pre-dispatch outcome, or one of `PRE_DISPATCH_STATUSES` = 400, 401, 404, 405, 413, 415, 422
   (`src/peer_transport.rs:55-70`, `src/node_routing.rs:415`, `:469-472`), because a replayed
   authenticated read costs a second account request. Those seven are the only statuses a
   1.2.0+ executor answers on the peer route, all before dispatch: bearer rejection, routing,
   the body limit and the JSON extractor or parameter checks; every dispatched query answers
   200 (`src/peer.rs:160-230`, [PEER_QUERIES.md](PEER_QUERIES.md#pre-dispatch-http-statuses)).
   3xx, 403, 408, 409, 429 and 5xx stay ambiguous because an intermediary can produce them after
   forwarding. The status still counts as a target fault, the body is never read, and node
   routing events carry the bare `status`. No wire change and no configuration surface.

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

## Sirius fields (reverse check)

Every field of every Sirius configuration struct was traced to a read outside its own
validation. Validation-only reads do not count. The sections that existed in 1.2.0 were
re-checked against the current code; the 1.2.1 additions are listed field by field.

| Section | Fields | Read at |
| --- | --- | --- |
| Root profile (`src/config.rs:5-74`) | `region`, `platform`, `protocol_directory`, `environment`, `endpoint`, `client_version`, `session_lock`, `api_token_env`, `internal_token_env`, `peer_token_env`, `player_id_env`, `player_credential_env`, `master_directory`, `default_cdn_root`, `cdn_credential_env` and the section fields below | `src/client.rs` (scope, headers, CDN state at `:133-147`), `src/deployment.rs:173-201`, `src/accounts.rs:369-410` |
| | `master_retention.keep_snapshots` (1.3.0, `src/config.rs:66`) | `MasterUpdater::new` (`src/master_update.rs:267`) and `Syncer::new` (`src/master_sync.rs:122`), applied by `master_registry::retain` after each settled pass (`src/master_update.rs:333`, `src/master_sync.rs:248`); rejected without `master_directory` or a writer (`src/config.rs:367-378`) ([Decision 19](#decisions)) |
| `upstream` (`src/config.rs:72-97`) | all, including `anonymous_max_inflight`, `coalesce_public_reads`, `http2_keepalive_interval_ms`, `http2_keepalive_timeout_ms` and `version_max_age_seconds` (1.3.0) | `src/transport.rs:35-66`, `src/client.rs:203`, `:216`, keepalive at `:220-230` (derived by `src/config.rs:155-171`), `:266-268`, `:813`, `:1456`, `:1478`, `:1603`, version age at `:1707`, SDK proxy at `:1985-1997`; `coalesces` also gates `src/node_routing.rs` `Router::call` |
| `master_update` (`src/config.rs:130-146`) and `network` (`src/master_update.rs:44-56`) | all, including `cdn_authorization` (1.2.1) | `src/master_update.rs:160-200`, `:97-137` |
| `resource_snapshot` (1.2.1, `src/config.rs:148-163`) | `cdn_authorization`, `username_env`, `catalog_hash_ttl_seconds`; `network.{connect_timeout_ms, request_timeout_ms, update_timeout_seconds, attempts, retry_delay_ms, max_retry_delay_ms, proxy_url_env, proxy_authorization_env}` | `src/client.rs:150-156`, `:1254-1325`, `src/resources.rs:106-110`. `network.update_timeout_seconds` bounds all `.hash` attempts and retry delays together, as for Master updates; see [Findings](#findings). |
| `master_git` (`src/master_git_worker.rs:9-27`), `commit`, `remote` | all, including `layout` and `branch` (1.2.1) and `timeout_seconds` (1.3.0) | `src/master_git_worker.rs:37-44`, `:163-182`, `:222`; `src/master_git.rs:302-326`, `:904-935`; `timeout_seconds` through `options()` (`src/master_git_worker.rs:42`) into the publication deadline (`src/master_git.rs:100-102`, `:665`); CLI subset (commit, push and, since 1.3.0, adopt with scope, `layout`, `branch` and `timeout_seconds` only) in [Decision 7](#decisions) |
| `master_database`, `master_sync`, `master_notify`, `node_routing` (+ `transport`), `asset_dispatch`, `response_cache`, `tls`, `access_log`, `logging` | all, including `connection.max_read_connections` (1.2.1) and `connection.read_timeout_seconds` (1.3.0) | Cited in the tables above; unchanged in use since 1.2.0 except `read_timeout_seconds`, read only by `Reader::new` (`src/master_database.rs:674-683`); the writers (`master_database_worker`, the registry owner and the `master-db-*` CLI) accept and ignore it |
| `client_auth` (`src/client_auth.rs:20-48`) | all | `src/client_auth.rs:84-100`, `:132-145`, `src/client.rs:165-172` |
| `accounts[]` (`src/accounts.rs:24-34`), `account_pool` (`:35-48`) | all, including `global_identity_file` (1.2.1) | `src/accounts.rs:81-101`, `:333-344`, `:346-429`, `:619-650`; path health since 1.3.0: `src/path_health.rs:74-83`, `src/client.rs:569-596` ([Decision 12](#decisions)) |
| `global_login` (1.2.1, `src/global_account.rs:29-47`) | `sdk_origin`, `sdk_app_key_env`, `sdk_timeout_ms` | `src/client.rs:1445-1471` |
| | `login_min_interval_seconds`, `max_logins_per_day` | `src/global_account.rs:218-235` |
| | `aegis_cooldown_seconds`, `concurrent_device_limit` | `src/accounts.rs:330-345` |
| Global identity file (1.2.1, `src/global_account.rs:83-106`, `src/global_sdk.rs:50-69`) | `schema`, `sdk.{uid, access_key, id_token, mid}`, `players.<region>.expected_player_id`, `device.{udid, model, pf_ver, dp, net, operators, adid, lang, time_zone, isRoot}`, `device.unity_{device_model, operating_system, device_id}` | `src/global_account.rs:127-162`, `:253-270`, `src/global_sdk.rs:101-114`, `:239-252`, `src/client.rs:854` |
| `http_compression` (1.3.0, `src/http_compression.rs`) | `enabled` | `http_compression::wrap`, called by `api::router_at` (`src/api.rs:161`) with the root value chosen in `src/deployment.rs:326-329`, and by `src/registry_service.rs:220` ([Decision 24](#decisions)) |
| `MultiConfig` (`src/deployment.rs:13-27`) | all; `regions` accepts the deprecated `tw` key (1.2.1) | `src/deployment.rs:156-201`, `:383-389`, `src/application_log.rs:78-93` |
| Registry `Config`, `Backend` (`src/registry_service.rs:20-42`), `owner` (`src/registry_owner.rs:7-20`) | all | `src/registry_service.rs:56-247`, `src/registry_owner.rs:33-87`, `src/main.rs:11`; `owner.retention` (1.3.0) is passed to `Syncer::standalone` (`src/registry_owner.rs:62-71`) and requires `source` (`:52-59`) |
| `master-db-*` `Import` (`src/master_database.rs:73-80`) | `source`, `scope`, `database` | `src/main.rs:92-113` |

### Findings

- **`resource_snapshot.network.update_timeout_seconds` was accepted but ignored; fixed in 1.2.1.**
  The review found that the catalog `.hash` fetch had no overall deadline. It now wraps every
  attempt and retry delay in `update_timeout_seconds`, exactly as the Master updater does
  (`src/resources.rs`, test `catalog_hash_fetch_honors_the_overall_update_deadline`).
- **Release smoke script:** `scripts/smoke-release.py` expected HTTP 501 for a Global profile
  lookup; Global now supports it, and without a configured account it returns 503. Updated.
- One-shot commands honor a documented subset of the profile: `master-git-commit`,
  `master-git-push` and `master-git-adopt` ([Decision 7](#decisions)); `global-account verify` uses the profile's
  accounts, `global_login` and `upstream` but, like every one-shot command except
  `master-update` and `master-sync`, default logging (`src/main.rs:47-54`,
  [APPLICATION_LOG.md](APPLICATION_LOG.md)); `global-account bootstrap` reads no configuration
  file and takes its SDK origin and app-key variable from arguments ([ACCOUNTS.md](ACCOUNTS.md)).
- No other parsed-but-unused or placeholder field was found.

## Example coverage

Every shipped example is parsed by tests, with its documented optional blocks uncommented:

- `sirius-api-config.example.yaml` as shipped (`src/tests.rs:2080-2082`), and each commented
  optional block (`master_update`, `master_sync`, `master_retention`, `accounts`, `tls`, `access_log`,
  `http_compression`, `logging`,
  `asset_dispatch`, `node_routing`, `master_notify`, `master_git` with its nested `signing`,
  `master_database`, `client_auth`) uncommented one at a time.
- `sirius-multi-region-config.example.yaml` as shipped and with its commented `tls:`,
  `access_log:` and `http_compression:` blocks uncommented, and with the deprecated `tw` region key
  (`multi_region_alias_key_maps_to_hk_and_both_keys_are_rejected`).
- `docs/examples/{en,hk,kr}.yaml` as shipped and with every commented Master, resource snapshot
  and Global account line uncommented; `hk.yaml` also with the deprecated `tw` region.
- `docs/examples/global-identity.example.json` through the identity-file parser.
- `docs/examples/master-registry.yaml` as shipped and with `tls:`, `access_log:`,
  `http_compression:`, `notify:`, each
  of the two `owner:` variants (the synchronizing one with its `retention`) and the PostgreSQL
  `backend:` uncommented.
- `docs/examples/master-database.yaml` as shipped and with `root_certificate` uncommented.
- The files above are covered by `every_shipped_example_parses_including_documented_optional_blocks`
  (`src/tests.rs:11916`). `docs/examples/master-publisher.yaml` is validated with all four regions
  by `multi_region_master_publishers_validate_with_distinct_state` (`src/tests.rs:13714`).

## Revision for 1.2.1

The audit was first written against 1.2.0 (`1054b40`). This revision re-verifies every row
against the 1.2.1 code (`0a6b99b`) and corrects every moved Sirius line reference. Changes in
classification and evidence:

- **Regions:** the original `tw` row now maps to `hk` with the deprecated input alias; Global
  game RPCs are no longer limited to Version and GetServerList; the Master pipeline and
  resource snapshots are enabled for HK/EN/KR.
- **Accounts and login:** `account_dir` stays a DECISION, now with the JP and Global account
  sources and the formats of the original CP/Nuverse account files. A new NOT_APPLICABLE row
  covers the original's CP/Nuverse login; Sirius's own Global SDK guest login is recorded as new
  configuration (`global_login`, `global_identity_file`), not a restoration. `is_cp_server()`,
  `apphash_sources`, `api_url` and `registry.account_nodes` note the Global SDK constants,
  `sdk_origin` and the `clientVersion` sent by PlayerLogin.
- **`version_path`:** NOT_APPLICABLE became ADAPTED / NOT_APPLICABLE. The dataVersion and
  assetVersion pair is recorded with each snapshot and published as `version.json` by the
  `indented_root` Git layout; the app-hash fields remain Sekai-specific.
- **`git`:** `layout` and `branch` added; the Master Git CLI now also honors them (Decision 7).
- **Proxy and headers decisions:** the Global SDK login uses the profile's `upstream` proxy,
  `resource_snapshot.network` has its own proxy, and the fixed gRPC metadata set includes the
  Global headers.
- **Reverse check:** added the [Sirius fields](#sirius-fields-reverse-check) table. One ignored
  field was found (`resource_snapshot.network.update_timeout_seconds`) and fixed; none remain.
- **Examples:** the example test now parses every commented optional block of every shipped
  example.

## Revision for 1.3.0

- **Request coalescing:** new [Decision 11](#decisions). `upstream.anonymous_max_inflight` and
  `upstream.coalesce_public_reads` are added to the reverse check; the `cache_ttls` `static`
  row notes that Version is coalesced though never cached.
- **Path health:** new [Decision 12](#decisions). `account_pool` now also sets the thresholds of
  the per-region path breaker; its reverse-check citations are updated.
- **Connection liveness:** new [Decision 13](#decisions). `upstream.http2_keepalive_interval_ms` and
  `upstream.http2_keepalive_timeout_ms` are added to the reverse check, whose `upstream` line
  references are updated.
- **Version header freshness:** new [Decision 14](#decisions); Decision 9 notes that
  `CLIENT_UPDATE_REQUIRED` no longer penalizes accounts. `upstream.version_max_age_seconds` is
  added to the reverse check, whose `upstream` line references are updated.
- **Cache hits before admission:** new [Decision 15](#decisions). No field is added or changed.
- **Master Git adoption:** [Decision 7](#decisions) adds `master-git-adopt STATE_DIR REMOTE_URL`,
  which reads the scope, `master_git.layout` and `master_git.branch` only. The environment
  variable list, the `master_git` reverse-check row and the one-shot finding cover it; the
  `src/main.rs` and `src/master_git.rs` line references are refreshed. No field is added.
- **Git time budget:** `master_git.timeout_seconds` (default 120, 10–600) replaces the fixed
  120-second publication deadline, with the new `git` row for the original's hard-coded
  per-command timeout and low-speed abort; [Decision 7](#decisions) and the environment variable
  list add `SIRIUS_MASTER_GIT_TIMEOUT_SECONDS`. The `master_git` reverse-check row and the moved
  `src/main.rs`, `src/master_git.rs` and `src/master_git_worker.rs` references are refreshed.
- **History metadata:** new [Decision 16](#decisions). No field is added; the moved
  `src/master_database.rs` references in the `master_database` rows and Decision 1 are refreshed.
- **Database read deadline:** new [Decision 17](#decisions). `connection.read_timeout_seconds`
  is added to the `master_database` `dsn` row and the reverse check; the moved
  `src/master_database.rs` references (including the environment variable list, Decisions 1 and
  16 and the `client_auth` rows) are refreshed.
- **File snapshot retention:** new [Decision 19](#decisions); Decision 1 notes the opt-in
  exception. `master_retention` joins the root-profile reverse-check rows and `owner.retention`
  the registry row; the example coverage lists the new commented blocks.
- **Dispatch worker status:** new [Decision 18](#decisions). The `asset_updater_servers[]` rows
  point to the status route and their moved `src/asset_dispatch.rs` references are refreshed.
  No field is added.
- **Health uptime:** new [Decision 21](#decisions). `/health` adds `uptime_secs` and remains
  liveness only. No field is added.
- **Response compression:** new [Decision 24](#decisions) and a top-level row for the
  original's unconditional `CompressionLayer`. Root `http_compression` joins the root-only
  lists (`servers`, `registry`), the reverse check and the example coverage.
- **Current Master ETag:** new [Decision 23](#decisions). A behavior-only change on the
  `/master-data` reads; no field is added.
- **Git content trailer:** new [Decision 25](#decisions). New Master Git commits add a
  `Sirius-Content-SHA256` trailer; no field is added.
- **JSON validation:** new [Decision 26](#decisions). Unchanged polls trust the snapshot index
  (size + SHA-256) instead of reparsing JSON; serving reads still validate. No field is added.
- **Table read admission:** new [Decision 27](#decisions). Master table reads pass 16 fixed
  process-wide permits with a 5 s wait; no field is added.
- **Stage measurement:** new [Decision 28](#decisions). The `BENCH_*` / `HARUKI_BENCH_*` row
  names the ignored `perf_stages` test; no field or variable is added.
- **Peer status failover:** new [Decision 29](#decisions) and a note under
  `servers.<region>.upstreams[]`. No field is added.
