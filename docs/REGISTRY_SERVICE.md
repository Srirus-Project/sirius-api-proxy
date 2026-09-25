# Standalone Master registry reads

Run `sirius-api-proxy registry-serve REGISTRY_CONFIG` using a private copy of
[the example](examples/master-registry.yaml). Set `SIRIUS_REGISTRY_TOKEN` to a dedicated
random bearer token. This command initializes no game client, account pool, CDN transport,
background game updater or runtime Protobuf bundle. An optional owner worker fetches only verified Master data. It can run from a directory without
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
| `/bundle` or `/by-hash/{content_sha256}/bundle` | Verified tar of one complete snapshot |
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

The service can serve files or database state written externally or by the optional publication worker
below, and can notify consumers after the served state changes (see
[outbound consumer notifications](#outbound-consumer-notifications)). No Sekai-specific music metadata
or app-identity overrides are introduced. Final candidate and packaged cross-platform acceptance are
still required for 1.2.0.


## Verified snapshot bundles

The two bundle routes return `application/x-tar` with `metadata/manifest.json` and only the
listed `tables/<name>.json` entries. They do not scan directories or include runtime config,
credentials, encrypted inputs, orphan files or unrelated publication records. Table JSON bytes
are preserved exactly. Headers use regular files, mode 0644, UID/GID 0 and mtime 0. Manifests
retain the backend's snapshot identity; archive entries are an export format, not a writable
Master directory or an automatically extracted import request.

The service pins one manifest, verifies every table's size/hash/JSON and builds the whole tar
in an anonymous temporary file before returning 200. A change to CURRENT cannot mix versions.
A database retention race or unreadable/corrupt file fails the request rather than returning a
successful partial archive. Build work is bounded to 120 seconds after manifest selection,
528 MiB of temporary archive bytes, and one decoded table at a time (existing 64 MiB/table and
512 MiB total source limits). Two process-wide permits cover construction and response streaming;
additional simultaneous bundle requests return 503. Temporary files and permits are dropped on
failure, disconnect or completion, including cancellation while a bounded blocking write finishes.

The response includes exact Content-Length, an ETag covering the actual tar bytes, Master version,
scoped content hash and a safe content-derived download filename. Bundle responses use private
`no-cache`: reimporting identical file content can change the embedded local snapshot UUID and
therefore the archive bytes. Conditional 304 is considered only after full source verification
and archive construction, so it cannot conceal later corruption. Range/resume is not implemented.
Clients must still check successful HTTP completion before using or extracting a downloaded file.


## Optional owner pull and publication

Configure `owner` to synchronize from a verified Sirius Master endpoint on startup, at the
configured interval, and when an authenticated refresh or update hint arrives:

```yaml
owner:
  internal_token_env: SIRIUS_REGISTRY_INTERNAL_TOKEN
  source:
    origin: https://owner.example.invalid
    token_env: SIRIUS_MASTER_OWNER_TOKEN
    regional_paths: false
    interval_seconds: 300 # 60–86400
    timeout_seconds: 600
    request_timeout_ms: 60000
  # PostgreSQL backends require this local verified snapshot directory:
  # staging_directory: ./registry-master
```

File backends synchronize into their configured serving directory and reject `staging_directory`
to avoid ambiguous ownership. PostgreSQL backends require that directory: first the worker
installs a fully verified local snapshot, then it transactionally publishes to the database.
With this worker enabled, the database role needs the writer's schema/publication permissions;
a SELECT-only role is suitable only for a registry without an owner worker.

Public read, internal administration and source bearer values must all differ. Database passwords
must also differ from all three. Source transport uses verified HTTPS, no ambient proxy, no redirects
or hidden retries; explicit HTTP is limited to the existing loopback-only testing policy. No game
account, game server access or runtime proto bundle is needed. Restart to rotate configuration/secrets.

| Internal route under `/internal/v1` (or `/internal/v1/jp`) | Behavior |
| --- | --- |
| `GET /master-data/updater` | Pending/running/ready/failed/stopped and process-local last success |
| `POST /master-data/refresh` | Queue a coalesced source reconciliation; return 202 |
| `POST /master-data/publish` | Queue local verification/database publication without contacting the source; return 202 |
| `POST /master-data/sync` | Validate a bounded scoped `{scope, content_sha256}` hint, queue reconciliation; return 202 |

These routes exist only with an owner configured and require its internal token. The update hint
matches the existing `master_notify` contract; point a producer's notification target at this registry.
A hint never supplies a URL, content or authority: the configured source's current manifest remains
authoritative. Acceptance means a wakeup was queued, not that publication finished. Hints during an
active transfer coalesce into a subsequent reconciliation. Wakes are process-local; startup reconciliation
and polling recover after restarts or lost notifications.

Each worker serializes its transfers. Failed source download/integrity checks preserve local CURRENT
and published database state. Failed database publication may leave a newer verified local snapshot,
while the served database and last-success status remain at their previous version. Retry reuses local
verified tables and reconciles the database by content identity. There is no file fallback for database
reads. Errors expose fixed codes without credentials, URLs or paths. Shutdown cancels in-flight owner
requests/database work; the normal snapshot and transaction cancellation protections remain in force.

Tests exercise a real HTTP owner, startup, authorization, scoped hints, source failure/recovery,
stalled-request shutdown, actual PostgreSQL insertion failure/retry, observed advisory-lock blocking
at shutdown, restart deduplication, and a separately executed real 60-second periodic retry without hints.


### Local publication without a source

Omit `owner.source` to run a local publication worker instead of a puller:

```yaml
owner:
  internal_token_env: SIRIUS_REGISTRY_INTERNAL_TOKEN
  local_interval_seconds: 300 # optional; 60–86400, default 300
  # staging_directory: ./registry-master # required for PostgreSQL, omitted for files
```

Startup and periodic reconciliation fully verify a pinned local CURRENT. File backends already
serve installed snapshots directly, so verification does not create a new snapshot or history entry;
the local verification receipt's `changed` field is always false. PostgreSQL backends transactionally
publish local CURRENT with the existing content deduplication and retention rules. Database failures
leave published state unchanged. No game account, upstream bearer or owner URL is needed in this mode.
The refresh and source-hint endpoints return 404 without a source. Status and local publish remain
protected by the configured internal token. `local_interval_seconds` is rejected when a source is
configured; source mode uses `source.interval_seconds` without silently ignoring either setting.

`POST /master-data/publish` also works with a source configured, even when that source is unavailable.
It queues local work on the same serial worker; it does not pull, alter local CURRENT or accept files
from the caller. A 202 acknowledges process-local queued work; inspect status for completion. Concurrent
local-publish and source-refresh requests are coalesced separately: local publication runs first, followed
by the requested source reconciliation. Periodic source reconciliation may subsequently install newer
source content. Failed local verification retains last success and never publishes corrupted tables.


## Outbound consumer notifications

The original registry notifies subscribers after publishing. Configure `notify` to send the existing
bounded `{scope, content_sha256}` update hint to other registries or proxies that pull from this one:

```yaml
notify:
  interval_seconds: 30        # 10–3600 retry/reconciliation interval
  request_timeout_ms: 5000    # 100–30000
  targets:                    # 1–16, unique names
    - name: replica-a
      origin: https://replica-a.example.invalid
      token_env: SIRIUS_REPLICA_A_INTERNAL_TOKEN # the consumer's internal token
      regional_paths: false
```

The announced hash is always the state this process serves: CURRENT for files and the committed
current document for PostgreSQL. A snapshot staged locally whose database publication failed is
therefore never announced. The notifier runs at startup, after each successful owner reconciliation
or local publication, and at the retry interval. Every target is attempted independently; a target
that accepted a hash is not sent it again, while failed targets retry. Without an `owner` the
notifier still announces externally written state on its interval. Notification never modifies
publication state.

Hints only wake a consumer; it still fetches and verifies the manifest from its configured source.
Acceptance (202 `{"status":"accepted"}`) is not synchronization success, and delivery is not
exactly-once: acknowledgement state is process-local, so a restart may resend the current hint.
Consumers must keep periodic polling. Transport uses verified HTTPS, no ambient proxy, no redirects
or hidden retries, and bounded replies; `allow_http` is an explicit testing opt-in.

Notification tokens must differ from the registry read token, internal token, source token and
database password. With an owner, `GET /internal/v1[/jp]/master-data/notifications` (internal token)
reports `pending`, `ready`, `retrying`, `unavailable` or `stopped`, the served content hash and each
target's name and accepted hash, without origins, credentials or paths. Shutdown cancels an
in-flight delivery.

Tests cover a real consumer woken by a local publication with 86400 s consumer polling and 3600 s
notifier retry, partial failure retry without resending, missing served state, stalled delivery
shutdown, credential separation and policy bounds; an optional PostgreSQL test proves a staged
snapshot is not announced while database publication fails and is announced after recovery.
