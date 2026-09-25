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

## Current integration boundary

This is a working explicit importer and database mirror. Background publication/retry, database
HTTP reads, file-to-database registry migration and a standalone registry service are not yet
connected. Do not configure `master_database` on the API service until that integration exists.
Only PostgreSQL is implemented; there is no silent fallback to another database.

The optional test `master_database_postgres_atomic_history_retention_integrity_and_retry` requires
an isolated PostgreSQL server with a `sirius_test` database and `postgres` user. Set
`SIRIUS_TEST_POSTGRES_PORT` and `SIRIUS_TEST_POSTGRES_PASSWORD`, then run it with `--ignored`.
It creates synthetic scoped data, injects a document-insert failure, exercises lock timeout and
cancellation, verifies concurrent deduplication, retention/isolation and corruption refusal,
and checks that verified TLS refuses a plaintext-only server. Never point it at production.
