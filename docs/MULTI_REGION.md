# Multi-region service

Use `SIRIUS_CONFIG_PATH=sirius-multi-region-config.yaml` with a copy of
`sirius-multi-region-config.example.yaml`. The example contains JP, TW, EN and KR;
remove regions you do not operate. Replace Global CDN placeholders with verified
server-list roots and supply the corresponding environment secrets.

The top-level `listen` binds one HTTP listener, or HTTPS when [top-level TLS](LISTENER_TLS.md) is configured. `regions` maps region names to the
same client settings used by a single-region file, excluding `listen` and `tls`. Each map
key must match its client's region. Empty maps, unknown settings, nested listeners,
CN and mismatched known service roots fail at startup before binding the listener.
Paths remain relative to the process working directory, as in single-region mode.

| Scope | Example |
| --- | --- |
| Public API | `GET /api/v1/jp/regions` |
| Version query | `GET /api/v1/en/system` |
| Internal snapshot | `GET /internal/v1/jp/resources/snapshot` |
| Protocol status/reload | `GET /internal/v1/kr/protocol`, `POST /internal/v1/kr/protocol/reload` |
| Process liveness | `GET /health` |

All existing resource suffixes are available under each region's prefix. Unsupported
Global RPCs still return 501 without an upstream call. There is no implicit default
region route in this mode: `/api/v1/system` and unconfigured regions return 404.
Health is unauthenticated process liveness, not proof that every game server is ready.

Each region owns its client, protocol generation, observations, snapshot, game account
and session lock. Protocol reload affects only the selected region. Master workers
are created per supported region and share graceful service shutdown; Global Master
storage remains rejected until its contracts are verified. Configure game credentials
only for their owning region. Never copy JP credentials into Global settings.

Each route uses that region's API or internal token. Use distinct tokens across regions
for independent authorization; deliberately reusing a token grants access to all
regions configured with it. An API token equal to any region's internal token is
rejected, including cross-region collisions. Tokens and clients are validated before
listening. Changing regions, account identity or deployment settings requires restart.

Existing single-region files and `/api/v1/...`, `/internal/v1/...` routes remain supported.
Omitted single-region `listen` defaults to `127.0.0.1:9999`. The `master-update` one-shot
command requires a single-region file; `master-import` retains its offline interface.

For the asset updater, set `regional_routes: true`, the intended `region`, the proxy's
origin in `game_api_root`, and that region's token references. Both version refresh
and snapshot requests then use the region prefix. Leave `regional_routes: false`
(the default) when talking to a legacy single-region deployment. Snapshot region,
platform, protocol and credential checks remain mandatory in either mode.
