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

Database-backed HTTP document reads, committed file-history migration and a standalone registry
service remain separate pending work. Only PostgreSQL is implemented; no database fallback occurs.

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
