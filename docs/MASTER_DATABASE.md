# Optional Master database mirror

`master-db-import` verifies a pinned local Master snapshot, then publishes its manifest and
all JSON tables to PostgreSQL in one transaction. The API continues reading its existing
file snapshots. A database outage cannot roll back local `CURRENT` or delete local files.
This command needs no game account, API token or CDN key.

```sh
export SIRIUS_MASTER_DATABASE_PASSWORD='replace-with-a-dedicated-password'
sirius-api-proxy master-db-import docs/examples/master-database.yaml
```

Set `source`, scope and database connection fields in a private copy of the example. JP is
the only verified Master format; the source must pass existing manifest/table verification.
The configured scope must match the source deployment's region/environment/platform.

Transport defaults to TLS with CA and hostname verification, using the bundled WebPKI trust roots
or an explicit `root_certificate`. `plaintext_loopback: true` is permitted only for literal
loopback IPs (for a local test database or separately secured tunnel); it rejects DNS names
and non-loopback hosts. Passwords are loaded from the named environment variable; connection
strings and raw database errors are never printed. Ambient `PG*` variables are rejected to
prevent importing libpq client certificates, keys, options or identities unintentionally.

## Stored data and consistency

The database role needs permission to create the four `public.sirius_master_*` tables and
indexes on first use. Use a database dedicated to this service. SQLx query logging is disabled;
identifiers are fixed and all data values use bound parameters.

- `sirius_master_snapshots`: immutable verified manifest, keyed by scope and content hash.
- `sirius_master_documents`: exact original JSON bytes and SHA-256 plus a JSONB representation.
  A GIN index supports PostgreSQL JSON containment queries without game-specific schemas.
- `sirius_master_current`: current hash for each scope.
- `sirius_master_history`: ordered publication events, retained independently of document retention.

Scope is the serialized `{region, environment, platform}` object. A scoped publish, history
entry, current-pointer switch and retention are atomic. An advisory transaction lock serializes
writers and initial schema creation. Only one connection is used per import. The configured
`timeout_seconds` bounds network/database work and sets server statement/lock deadlines.
Local source verification precedes connection establishment; it retains the existing Master
size/depth limits and loads the pinned snapshot into memory.

Identical current content is verified against existing stored bytes and JSONB, then acknowledged
without a new history event. Reimported local snapshot UUIDs do not create duplicate content.
Corrupt existing database content fails closed. Retrying after an uncertain commit reconciles
using content identity; it does not blindly append another publish event. Explicitly importing
older local content can intentionally make that content current again.

`keep_snapshots` defaults to 20 (range 1–10000) per scope, ordered by latest publication.
Pruning deletes old database documents through foreign keys, preserves current and other scopes,
and leaves historical event hashes and every local file intact. History is not automatically
pruned. PostgreSQL JSONB's supported numeric/string range still applies; a value PostgreSQL
cannot represent causes transaction rollback even though exact JSON bytes are also retained.

## Background publication

Configure `master_database` on the JP single-region configuration or JP profile of a multi-region
deployment. `master_directory` must be set. The connection fields match the CLI example:

```yaml
master_directory: ./master
master_database:
  interval_seconds: 300 # 10–86400; startup always reconciles CURRENT
  connection:
    host: database.example.invalid
    database: sirius_master
    username: sirius_master
    password_env: SIRIUS_MASTER_DATABASE_PASSWORD
    timeout_seconds: 120
    keep_snapshots: 20
```

Successful in-process Master installations wake the database worker independently of Git and
consumer notifications. External file imports are found on the next interval. Notifications
coalesce; only one import runs per profile. Failures preserve local CURRENT and the last successful
receipt, then retry periodically. Shutdown cancels active database work and waits for the worker;
restart rechecks content identity, including recovery from an uncertain commit.

`GET /internal/v1/master-data/database` (or the regional internal prefix) requires the internal
token and returns `disabled`, `pending`, `running`, `ready`, `failed` or `stopped`, plus the last
successful receipt and static error code where applicable. It never returns connection settings,
passwords or filesystem paths. Last-success status is process-local and rebuilt after restart.

Use a dedicated database password distinct from API, internal, game, CDN, peer, updater,
notification and Git credentials in every deployment profile. Profiles may share database
credentials deliberately; rows remain scoped by region/environment/platform. Global Master
publication remains unsupported. Other service functions do not require the database to be up.

A standalone registry service remains separate pending work.
Only PostgreSQL is implemented; no database fallback occurs.

The optional test `master_database_postgres_atomic_history_retention_integrity_and_retry` requires
an isolated PostgreSQL server with a `sirius_test` database and `postgres` user. Set
`SIRIUS_TEST_POSTGRES_PORT` and `SIRIUS_TEST_POSTGRES_PASSWORD`, then run it with `--ignored`.
It creates synthetic scoped data, injects a document-insert failure, exercises lock timeout and
cancellation, verifies concurrent deduplication, retention/isolation and corruption refusal,
and checks that verified TLS refuses a plaintext-only server. Never point it at production.

The optional `master_database_worker_start_wake_retry_auth_and_shutdown` test exercises real
startup publication, independent update hints, a failed database insert followed by timed recovery,
internal route authorization/redaction, and shutdown during a blocked database transaction.
The two PostgreSQL tests serialize their schema-failure fixtures within the test process.

## Database mirror read API

These routes use the public API bearer and the configured profile's scope. For multi-region
services insert the region after `/api/v1`. They read the database mirror explicitly; the existing
file snapshot endpoints retain their behavior. A configured mirror may lag local installation;
check its internal publication status when freshness matters.

| Route | Result |
| --- | --- |
| `GET /api/v1/master-data/database/manifest` | Current database manifest |
| `GET /api/v1/master-data/database/by-hash/{hash}/manifest` | Retained manifest by content hash |
| `GET /api/v1/master-data/database/by-hash/{hash}/tables/{name}` | Exact table JSON; name omits `.json` |
| `GET /api/v1/master-data/database/history?limit=50&before=123` | Newest-first publication events |

Use `content_sha256` from the manifest to pin table requests. A table response contains the
original bytes, `x-master-version` and a hash ETag. Manifest ETags cover the whole manifest;
manifest responses use `private, no-cache` because a pruned then republished content hash can
have a different local snapshot UUID. Tables use immutable private caching. Hash verification
runs before conditional 304 handling, so invalid stored bytes cannot be hidden by an old ETag.

A missing/pruned hash or unlisted table is 404. Missing configured database, connection failure,
missing listed table, oversized or corrupt content is 503. There is no fallback to local files.
Manifest/table reads share a repeatable-read, read-only transaction, preventing retention or
current-pointer changes from mixing versions within a response. Manifest and payload reads are
bounded to 1 MiB and 64 MiB respectively on the database side before allocating response bytes.

History returns `entries` containing decimal-string `sequence`, `content_sha256` and `retained`,
plus optional `next_before`. Pass that cursor as `before` on the next request. Pages have 1–200
entries; new publications do not shift an existing sequence cursor. Events remain after payload
retention, and `retained` reflects availability when that page was read. History has `no-store`
caching. Sequence identifiers are global database IDs; gaps within a scope are expected.

Each profile lazily opens a shared pool of at most four read connections; publication uses its
separate single connection. The configured database deadline includes pool admission. Restart
the service when rotating database settings/passwords so the read pool uses the new values.
The pool is initialized on the first authenticated valid request, not during configuration checks.

The optional `master_database_read_http_integrity_retention_history_and_scope` test uses real
PostgreSQL through the actual HTTP router. It covers pinned bytes without local CURRENT,
conditional reads, pruning/history pagination across new publication, scope isolation, corrupt
and oversized rows, and database outage without local fallback.

## One-time committed file-history migration

Run `sirius-api-proxy master-db-migrate DATABASE_CONFIG` with the same private configuration
as `master-db-import`. Stop or leave disabled the background database publisher until migration
completes. The selected database scope must be empty: existing snapshots, current pointers or
history cause refusal, including a scope previously populated by `master-db-import`. Other
scopes are untouched. This command does not merge databases or overwrite existing publications.

The migration pins local CURRENT and follows its predecessor chain, oldest first, up to 10000
committed snapshots. It never discovers staging/orphan directories by scanning. Every historical
table is verified before connecting, and each snapshot is verified again as it is written. Only
one decoded snapshot is held at a time; local files and CURRENT remain unchanged. New local
installations after the plan is pinned belong to a later normal publication. Missing or corrupt
history and cycles fail closed. A legacy snapshot without a publication record ends the chain;
`legacy_boundary: true` reports that older chronology is unknown.

Schema initialization, all events and documents, retention, current and a durable migration
receipt commit in one transaction. Original publication times are preserved to PostgreSQL's
microsecond precision; the legacy boundary's unknown time uses the migration transaction time.
Repeated consecutive content still preserves each committed file publication as an event.
`keep_snapshots` retains the most recently published distinct content hashes, while every event
remains. The additional `public.sirius_master_migrations` table stores the scoped source-plan
hash, head, publication count and legacy-boundary flag. It contains no credentials or paths.

Retrying the same source plan returns `changed: false` without duplicating events or rewinding
subsequent database publications. A different plan in a previously migrated scope is refused;
use normal publication for subsequent updates. The receipt acknowledges the completed migration,
not a new integrity audit of data changed afterward. Retrying requires the original source chain
to remain available and valid. Keep the files until migration has been confirmed.

The database deadline includes lock admission and all transactional source rereads/writes; the
initial local verification happens before that deadline. Timeout, cancellation, invalid JSONB
or any write failure rolls back the transaction. An interrupted response after commit is resolved
by the durable receipt. Database writers use the same advisory lock, including background imports.

The optional PostgreSQL migration test injects a failure at the final receipt insert and verifies
that no partial snapshots, documents, history, current or receipt survive. It also covers canceled
lock admission, concurrent identical migration, exact chronology, retention, occupied-scope refusal,
scope isolation and receipt replay after a newer publication. Default tests cover corrupt historical
tables, cycles, orphan exclusion and the explicit legacy boundary without connecting to a database.
