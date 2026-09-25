# Standalone Master registry reads

Run `sirius-api-proxy registry-serve REGISTRY_CONFIG` using a private copy of
[the example](examples/master-registry.yaml). Set `SIRIUS_REGISTRY_TOKEN` to a dedicated
random bearer token. This command initializes no game client, account pool, CDN transport,
background game updater or runtime Protobuf bundle. It can run from a directory without
`protocol/`. It shares the normal binary's TLS listener, application/access logging and
SIGINT/SIGTERM shutdown. Configuration is bounded to 64 KiB and rejects unknown fields.

A process serves one explicit JP/environment/platform scope using either immutable local
file snapshots or the PostgreSQL mirror. Global Master formats, including reserved CN,
are rejected before network activity. `regional_paths: true` inserts `/jp` after `/api/v1`;
there are no implicit aliases. Use separate instances/listeners for different scopes.

The file backend observes CURRENT on each request. The PostgreSQL backend reads committed
state directly, so it needs no local source files. It does not initialize or mutate the
schema: first publish/import/migrate using the documented database writer, and grant the
registry role SELECT access to the existing tables. Database passwords must differ from
the registry bearer. Verified TLS, lazy bounded pooling, read deadlines and corruption
checks match [the database mirror](MASTER_DATABASE.md). A database outage returns 503;
there is no file fallback. Restart to rotate tokens or connection settings.

## Routes and consumers

All Master reads require exactly one `Authorization: Bearer ...` header. Missing, incorrect
and duplicate headers return 401. `/health` is public and reports process liveness only;
it does not assert that Master data or the database is ready. Game and internal proxy
routes are absent.

| Route under `/api/v1/master-data` | Meaning |
| --- | --- |
| `/manifest` | Current verified manifest |
| `/by-hash/{content_sha256}/manifest` | Retained manifest by scoped content identity |
| `/snapshots/{snapshot}/manifest` | Pinned manifest |
| `/snapshots/{snapshot}/tables/{table}/{sha256}` | Pinned original JSON bytes; table omits `.json` |
| `/history?limit=20&before=...` | Backend-tagged publication history, 1–100 entries |

The standard manifest and pinned-table contract works with existing `master_sync` consumers.
Configure their `origin`, matching `regional_paths`, scope and token reference. Both backends
are tested through real HTTP consumer installation and unchanged-content reconciliation.
The registry has no dependency on a running game server.

Database manifests expose `master-<content_sha256>` as a stable virtual snapshot ID. This
adapter changes no table bytes or content identity and avoids coupling clients to a writer's
local snapshot UUID. Always use the ID returned by the registry; a local writer UUID is not
a database registry address. Retention can remove old content, producing 404. Manifest ETags
cover the adapted bytes, and manifests require revalidation. Verified table responses have
immutable private caching. Integrity verification precedes conditional 304 handling.

History returns `{backend, history}`. File history contains committed snapshot IDs and uses
`next_before` as its snapshot cursor. PostgreSQL history contains decimal-string sequences,
content hashes and retention flags; use its `next_before` sequence cursor. Do not reuse a
cursor after changing backends. History uses private `no-store` caching. The endpoint does
not pretend that file installation chronology and database publication chronology are identical.

## Remaining registry work

This command currently serves data written by the existing file importer, owner updater,
background database publisher or explicit database commands. Standalone owner pull/poll,
authenticated refresh/publication hints and whole-snapshot bundle delivery are separate
remaining restoration work. No Sekai-specific music metadata or app-identity overrides are
introduced. Final candidate and packaged cross-platform acceptance are still required for 1.2.0.
