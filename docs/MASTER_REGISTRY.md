# Master snapshot publication

The existing JP `master_directory` now exposes verifiable plaintext manifests through the
public API bearer scope. No game/CDN credential, encryption key, schema model or query to the
game is needed to read installed snapshots. These endpoints remain local when node routing is
enabled. Global Master storage remains outside the verified capability matrix.

For a single-region deployment:

| GET path under `/api/v1/master-data` | Result |
| --- | --- |
| `/manifest` | Current snapshot's scoped plaintext manifest |
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

## Remaining restoration

This is the producer/read protocol over atomic local snapshots. Owner-to-consumer synchronization,
central publication history/registry persistence, completion notifications and optional Git
publication remain separate restoration work. Local manifest/file tests do not replace yhm01
full candidate acceptance, source/artifact audits or the 1.2.0 release gates.
