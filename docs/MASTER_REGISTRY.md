# Master snapshot publication

The existing JP `master_directory` now exposes verifiable plaintext manifests through the
public API bearer scope. No game/CDN credential, encryption key, schema model or query to the
game is needed to read installed snapshots. These endpoints remain local when node routing is
enabled. Global Master storage remains outside the verified capability matrix.

For a single-region deployment:

| GET path under `/api/v1/master-data` | Result |
| --- | --- |
| `/manifest` | Current snapshot's scoped plaintext manifest |
| `/history?limit=20` | Recent installations in committed predecessor order |
| `/snapshots/{snapshot}/manifest` | The named snapshot's manifest, independent of CURRENT |
| `/snapshots/{snapshot}/tables/{table}/{sha256}` | Exact JSON bytes matching the pinned SHA-256 |

Multi-region deployments insert `{region}` after `/api/v1`. A table identifier is the existing
name without `.json`, for example `MasterExample`. Only names listed in the source manifest
are served. Unsafe paths and linked snapshot/file entries are rejected. Retained snapshots
remain addressable after a new snapshot becomes current; these endpoints do not prune history.

The manifest contains schema version 1, region/environment/platform scope, snapshot identifier,
Master version, sorted plaintext file names/sizes/SHA-256, a `content_sha256`, and the original
encrypted-file `source_manifest`. The source metadata contains names, sizes and hashes only.
A consumer must pin the manifest before reading files and independently verify every downloaded
file's hash/size and the expected scope. It must never combine CURRENT-relative table reads
into one snapshot while an owner might publish a new version.

## Identity and HTTP caching

`content_sha256` is SHA-256 over compact UTF-8 JSON with recursively lexicographically sorted
object keys and the fields `schema_version`, `scope`, `source_manifest`, `files`. Both file arrays
are sorted by name. It includes the version through `source_manifest`; it excludes the local
snapshot UUID. Reimporting identical data therefore retains content identity while receiving a
new storage identifier. Whitespace and number spelling inside the actual table files are never
rewritten: their hashes and byte sizes refer to the original decoded bytes. Tests include
independently calculated Python hash/canonical-JSON vectors.

Manifest ETags hash the entire serialized response, including snapshot identifier. Manifests use
`Cache-Control: private, no-cache`; `If-None-Match` supports strong/weak matching, lists and `*`.
A matching condition returns an empty 304. Digest-qualified table URLs return a hash ETag and
`private, max-age=31536000, immutable`. Authorization still precedes all handlers. A file is
read and verified before returning a conditional response, so a missing/corrupted file cannot
be concealed by a stale ETag. `x-master-version` is present on successful/conditional responses.

## Local integrity and older snapshots

New local imports and CDN updates stage `tables.json` alongside the source manifest, receipt
and decoded tables, before the existing atomic publication of CURRENT. Its file list must
exactly match the source manifest; files are limited to 64 MiB each, 512 MiB aggregate, and the
existing maximum 4096 tables. Ordinary current-table reads now also verify this index when
present. Invalid indexes never fall back to an unverified read. MasterManifest.bin is rejected
as a table name because its decoded name would overwrite the snapshot's source manifest.

Version 1.1 snapshots without an index remain supported. A manifest read computes their hashes
in memory, validates JSON and leaves disk unchanged; this scans their table data once per
manifest request. Digest-qualified file reads then verify the requested hash without rescanning
all tables. This establishes a baseline from the trusted local legacy files, not renewed proof
of their original encrypted CDN payload. Reimporting or updating with the current producer
creates the durable index. Normal existing API paths remain compatible.

## Consumer synchronization

A JP consumer may configure `master_directory` and `master_sync` instead of `master_update`:

```yaml
master_directory: ./master-data
master_sync:
  origin: https://master-owner.example.invalid
  token_env: SIRIUS_MASTER_OWNER_TOKEN
  regional_paths: false
  allow_http: false
  interval_seconds: 300
  timeout_seconds: 600
  request_timeout_ms: 60000
```

The origin must contain only scheme and authority. The bearer is the owner's public API read
credential; it must not reuse administrative, peer, game, CDN or updater credentials in a
service deployment. `regional_paths: true` selects the owner's multi-region URL layout.
HTTPS verifies certificates normally. Plain HTTP requires explicit opt-in. Redirects, ambient
proxies and automatic HTTP retries are disabled. Neither game login nor CDN decryption keys
are needed by the synchronization operation.

The service synchronizes immediately after startup and waits `interval_seconds` after each
attempt (60..86400). Failed attempts retain the installed snapshot and retry at the next poll.
`timeout_seconds` (default 600, 1..3600) covers lock admission, manifest/file acquisition and
preparation; `request_timeout_ms` (default 60000, 100..300000) bounds each HTTP request. Connection
establishment uses the smaller of that request limit and 10 seconds. Manifests are bounded to
4 MiB; declared tables retain the producer's file/count/aggregate bounds. The final synchronous
filesystem publication is not preemptible; filesystem stalls may exceed the network deadline.

The consumer pins the owner's scoped manifest, validates its content identity, verifies cached
local tables before reuse, and downloads missing or corrupt files through pinned digest URLs.
All files must match their declared byte length and SHA-256 and parse as JSON before publication.
A second manifest check rejects owner content changes during the transfer. New owner snapshot
UUIDs with identical content are accepted. The consumer creates its own local snapshot UUID and
receipt (`source: registry`), retaining the same content identity. It can serve the same read
protocol to downstream consumers. Unchanged polls still verify every installed table, allowing
local corruption to be detected and repaired. Previous snapshots are retained.

The existing filesystem writer lock excludes imports/CDN updates/other consumers. Shutdown
cancels outstanding synchronization; a blocking preparation may finish, but cannot publish
CURRENT independently after cancellation. Partial staging is temporary. No completion record
is claimed until atomic publication succeeds. The existing Master update status reports
`mode: sync`, running/ready/failed and a sanitized result.

For a one-shot run, use a single-region config:

```sh
SIRIUS_CONFIG_PATH=consumer.yaml sirius-api-proxy master-sync
```

This prints the result as JSON, exits nonzero on failure and does not start an HTTP listener.
As with other Master commands, stop a service owning the same snapshot directory first.

## Committed publication history

Every new import, CDN update or consumer installation writes `publication.json` inside its staged
snapshot, with its UUID, UTC publication-attempt time and the previous committed snapshot UUID.
That record is synced before the snapshot directory is renamed and CURRENT changes. The current
pointer therefore commits the new history link together with the new tables. Directories left
behind before a failed pointer switch never become history merely because they exist on disk.
Readers pin CURRENT once and walk the immutable predecessor chain, newest first; clock changes
do not reorder it. All writers continue to require the existing exclusive directory lock.
An invalid existing pointer/predecessor fails publication rather than silently starting a new chain.

`GET /api/v1/master-data/history?limit=20` uses the same public bearer as other Master reads
(and the regional prefix in multi-region deployments). Limits are 1..100, default 20; unknown
query fields fail. Responses are private/no-store and contain scope, pinned `head`, entries,
`has_more`, and `legacy_boundary`. Entries include snapshot UUID, source version, scoped content
SHA-256, file count, plaintext byte total and nullable `published_at`. This is installation
history: explicit reimports of identical content remain visible with equal content hashes;
ordinary unchanged CDN/sync polls do not install and thus add no record. Read paths never call
the game. Listed manifests/indexes and history links are validated; corrupt records, cycles,
linked files and unsafe paths fail explicitly. History reports manifest identities rather than
performing a full rehash of every indexed table payload.

Legacy snapshots have no publication record. They are included with `published_at: null`, and
traversal stops with `legacy_boundary: true`; older ordering is unknown. No directory scan,
mtime inference or automatic legacy rewrite fabricates missing history. `has_more` denotes a
known predecessor beyond the requested limit, not an inferred legacy predecessor. This endpoint
returns at most 100 recent installations; full-history pagination, external database persistence
and retention/compaction are separate work. Existing snapshot directories are not pruned.

## Remaining restoration

Producer reads and consumer synchronization operate over atomic local snapshots.
Central registry persistence, completion notifications and optional Git
publication remain separate restoration work. Local manifest/file tests do not replace yhm01
full candidate acceptance, source/artifact audits or the 1.2.0 release gates.
