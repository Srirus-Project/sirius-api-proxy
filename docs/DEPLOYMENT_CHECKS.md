# Deployment checks

Use a release archive or Docker image built from the intended commit. Do not copy only the
API executable: `protocol/` is required at runtime. Start with an existing account only when
account-dependent queries are needed; registration is not part of this service.

1. Copy `sirius-api-config.example.yaml` to `sirius-api-config.yaml`. Supply distinct
   `SIRIUS_API_TOKEN` and `SIRIUS_INTERNAL_TOKEN` secrets. Use an absolute protocol path
   when running outside the archive root. `SIRIUS_CONFIG_PATH` selects another config.
2. For Docker, set listen to `0.0.0.0:9999`, mount configuration read-only and persist the
   writable Master directory if used. Place external HTTP access behind your reverse proxy.
3. Start with Master background updates disabled. Check `/health`, then authenticate to
   `/internal/v1/protocol` and confirm the intended label/fingerprint and native codec.
   Check that public tokens cannot call internal routes and unauthenticated API calls fail.
4. Provide `SIRIUS_PLAYER_ID` and `SIRIUS_PLAYER_CREDENTIAL` together, enable their config
   references and verify `/internal/v1/account` before using account-dependent queries.
5. Enable Master service with `master_directory`. An offline import requires encrypted
   manifest/table files plus `SIRIUS_MASTER_KEY_HEX` and `SIRIUS_MASTER_IV_HEX`.
   Remote updates also require the configured CDN username/password. Run `master-update`
   once, check the table index and sample tables, then enable the periodic updater if desired.
6. Check `/api/v1/system` for actual availability, not merely HTTP 200. An internal resource
   snapshot must exist and have `stale: false` before the asset updater can use it.

Master updates verify the manifest, every table and the final version before publication.
Failures preserve the previous CURRENT; unchanged validated versions are not redownloaded.
CDN requests have 10-second connection and 60-second total timeouts. Each update has a
600-second deadline. Background intervals range from 60 to 86400 seconds and do not overlap.
`/internal/v1/master-data/updater` describes the last check, not continuous upstream health.

Record only versions, timestamps, route names, status codes and sanitized failures. Keep tokens,
credentials, private player responses, game files and downloaded data outside the repository
and image build context. Credential rotation requires updating secrets and restarting.

The current JP baseline covers identity, public/non-friend profiles, announcements, song
rankings, Master updates and codec switching. Established-friendship, event and challenge
business responses still need valid live data; do not invent IDs to claim those checks passed.

All snapshot files and the CURRENT pointer are flushed before publication. Unix additionally
flushes parent directory handles; Windows retains atomic publication without a POSIX directory-fsync step.
