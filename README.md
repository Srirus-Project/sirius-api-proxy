# Sirius API Proxy

A Rust API proxy for **BanG Dream! Our Notes**. Derived from
[Haruki-Sekai-API](https://github.com/Team-Haruki/Haruki-Sekai-API), with game-specific
protocols and a standalone implementation. Haruki's MIT attribution is retained
in [LICENSE](LICENSE); see [sources](docs/SOURCES.md). This is an unofficial project.

## Features

- Version, announcements, public player profiles, song/event/challenge rankings and event decks.
- Separate public API and internal administration tokens; private account responses stay internal.
- Binary Protobuf over HTTP/2 unary gRPC with verified TLS, trailers, deadlines and response limits.
- Native Protobuf/JSON codecs generated at build time; compatible proto hot reload switches to
  dynamic codecs without restarting. Rebuilding restores the native path for the new definitions.
- Master manifest verification, Rijndael-256 decryption, gzip/JSON validation, atomic snapshots,
  local table queries and optional periodic updates. No database is required.
- Version-pinned resource snapshots for Sirius Asset Updater, with CDN allowlists and secret references.

The full proxy baseline is JP iOS 1.0.3; Global Android 1.0.1 has a separate discovery/version bundle. The application release version **1.1.0** is
independent of the game's client version, protocol label and resource version.
Only explicitly supported RPCs for the selected region are exposed; arbitrary RPC forwarding is unavailable.

## Regions

Configure `region: jp`, `tw`, `en` or `kr`; `cn` is reserved and currently rejected before
network activity. Use one instance per region. JP retains its existing functionality; Global
currently supports verified server discovery/version queries and region-aware asset transport,
not completed SDK login or end-to-end Global asset validation. See [region support and upgrade
instructions](docs/REGIONS.md) before deploying paired v1.1.0 services.

## Quick start

Extract the release archive and run from its root, or build from source with Rust 1.96 or later:

```sh
cargo build --release --locked
cp sirius-api-config.example.yaml sirius-api-config.yaml
export SIRIUS_API_TOKEN='replace-with-a-long-random-token'
export SIRIUS_INTERNAL_TOKEN='replace-with-a-different-long-random-token'
./target/release/sirius-api-proxy
# Release archive: ./sirius-api-proxy (sirius-api-proxy.exe on Windows)
```

`SIRIUS_CONFIG_PATH` overrides the configuration path. Default listen address: `127.0.0.1:9999`.
Keep the bundled `protocol/` directory beside the executable and run from that directory,
or configure an absolute `protocol_directory`. No external protoc, Redis or database is needed.

Player queries require an existing account: configure both `player_id_env` and
`player_credential_env`, then provide the referenced secrets. The service does not register,
transfer or delete accounts. Public profile IDs are different from credential player IDs.

`session_lock` defaults to `true`, including when omitted from existing configuration.
Set `session_lock: false` to allow concurrent upstream RPCs for the same configured account;
restart the proxy to apply the change. Upstream concurrency support is not confirmed, and
server instability can also cause request failures. Keep the default unless testing or
operating with that uncertainty. The 20-second request deadline includes time waiting for
serialization, bootstrap or protocol activation. Initial authenticated Version discovery
remains single-flight, and protocol reload waits for all active logical calls in either mode.
With concurrency enabled, upstream observations reflect response completion order.

CDN secrets are optional for API-only use. Resource snapshots become ready only when the
observed CDN and credential match configuration. Secret values are never included in responses.
Rotate secrets in the deployment environment and restart after a server-side credential change.

## HTTP API

All routes except `/health` require `Authorization: Bearer ...`.

| Route | Token | Result |
| --- | --- | --- |
| `GET /health` | None | Process health and service version, not upstream availability |
| `GET /api/v1/system` | API | Region, supported RPCs, version and availability observation |
| `GET /api/v1/regions` | API | Region capabilities, including reserved CN |
| `GET /api/v1/servers` | API | Global server list; JP returns 501 |
| `GET /api/v1/announcements?tab=0` | API | Announcement list; tab is 0, 1 or 2 |
| `GET /api/v1/announcements/{id}` | API | Announcement details |
| `GET /api/v1/players/by-profile-id/{profile_id}` | API | Public player profile |
| `GET /api/v1/events/{event_id}/rankings?ranks=1,10,100` | API | Up to 100 distinct positive ranks |
| `GET /api/v1/events/{event_id}/players/{player_id}/deck` | API | Event deck |
| `GET /api/v1/songs/{song_id}/rankings` | API | Song ranking without the service account's myRank |
| `GET /api/v1/challenge-songs/{challenge_song_id}/rankings` | API | Challenge ranking without myRank/myScore |
| `GET /api/v1/master-data` | API | Local Master version and table index |
| `GET /api/v1/master-data/tables/{name}` | API | Original table JSON with x-master-version |
| `GET /internal/v1/protocol` | Internal | Protocol fingerprint, codec and generation |
| `POST /internal/v1/protocol/reload` | Internal | Validate and activate the configured proto bundle |
| `GET /internal/v1/master-data/updater` | Internal | Last update status; does not trigger an update |
| `GET /internal/v1/account` | Internal | Verify the configured account identity |
| `GET /internal/v1/account/player-data` | Internal | Read the service account's private data |
| `GET /internal/v1/resources/snapshot` | Internal | Last observed resource snapshot |

HTTP `v1` is independent of game versions. Environment and upstream are deployment settings,
not request parameters. Protobuf JSON int64/uint64 values are strings; original Master JSON
may contain numeric integers that require a lossless parser.

`/system` returns HTTP 200 with `status: unavailable` for valid upstream business errors;
network/protocol failures return 502 and timeouts return 504. Other upstream errors map to
502/503. Invalid caller tokens return 401; an unconfigured game account returns 503.
Raw credential fields and grpc-message values are not returned to callers.

## Master data and protocol updates

```sh
# Offline import; supply SIRIUS_MASTER_KEY_HEX and SIRIUS_MASTER_IV_HEX.
./sirius-api-proxy master-import /path/to/encrypted-master ./master-data
# One remote update using the configured master_update settings.
./sirius-api-proxy master-update
# Hot reload a complete, compatible proto bundle.
curl --fail-with-body -X POST -H "Authorization: Bearer $SIRIUS_INTERNAL_TOKEN" \
  http://127.0.0.1:9999/internal/v1/protocol/reload
```

Master key and IV each contain 32 bytes encoded as 64 hexadecimal characters.
Configure `master_directory` to serve snapshots. Remote updates verify manifest hashes,
decrypt and parse every table, recheck the version and atomically switch CURRENT.
Failed updates preserve the old snapshot; a writer lock prevents concurrent publication.
Optional background updates run at the configured interval without overlapping.

Protocol reload rejects incompatible changes with HTTP 422 and keeps the previous schema.
In-flight logical requests use one schema throughout. Files are not watched automatically.
See [protocol updates](docs/PROTO_RELOAD.md) and [deployment checks](docs/DEPLOYMENT_CHECKS.md).

## Deployment and scope

The Docker image contains the executable and protocol bundle. Mount configuration, supply
secrets and set `listen: 0.0.0.0:9999` inside the container. Persist `/app/master-data` if enabled.
Restrict internal routes at the reverse proxy as well as through their separate token.

The current JP baseline has been exercised for identity, account data, public profiles,
announcements, song rankings, 235 Master tables and native/dynamic protocol switching.
Event/challenge business responses and an established friendship were not covered by live testing.
No account pool, multi-node coordination, response cache or automatic RPC retries are provided.
Upstream availability is outside this service's control.

## Development and release

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
```

Tests use local fixtures and do not require the game servers. Release archives include the
runtime protocol bundle, examples, documentation and licenses. See [release preparation](docs/RELEASING.md).
Repository visibility and workflow activation are separate from preparing a release.
