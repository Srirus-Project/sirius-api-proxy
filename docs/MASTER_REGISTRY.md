# Master snapshot publication

The existing JP `master_directory` now exposes verifiable plaintext manifests through the
public API bearer scope. No game/CDN credential, encryption key, schema model or query to the
game is needed to read installed snapshots. These endpoints remain local when node routing is
enabled. Global Master storage remains outside the verified capability matrix.

For a single-region deployment:

| GET path under `/api/v1/master-data` | Result |
| --- | --- |
| `/manifest` | Current snapshot's scoped plaintext manifest |
| `/by-hash/{content_sha256}/manifest` | Newest committed installation with that scoped content identity |
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
`has_more`, `next_before`, and `legacy_boundary`. Entries include snapshot UUID, source version, scoped content
SHA-256, file count, plaintext byte total and nullable `published_at`. This is installation
history: explicit reimports of identical content remain visible with equal content hashes;
ordinary unchanged CDN/sync polls do not install and thus add no record. Read paths never call
the game. Listed manifests/indexes and history links are validated; corrupt records, cycles,
linked files and unsafe paths fail explicitly. History reports manifest identities rather than
performing a full rehash of every indexed table payload.

Legacy snapshots have no publication record. They are included with `published_at: null`, and
traversal stops with `legacy_boundary: true`; older ordering is unknown. No directory scan,
mtime inference or automatic legacy rewrite fabricates missing history. `has_more` denotes a
known predecessor beyond the requested limit, not an inferred legacy predecessor. Each page
returns at most 100 installations. Pass the returned `next_before` as the `before` query
parameter to retrieve strictly older entries, for example `/history?limit=20&before=master-UUID`.
`next_before` is null at the end; repeating a cursor is safe. A new CURRENT between page requests
does not duplicate or skip the older entries, since each page finds the cursor in the committed
predecessor chain. `head` is pinned independently for each request and may therefore change.
A syntactically invalid cursor returns 400; a missing or orphan snapshot, or a cursor beyond a
legacy boundary, returns 404. A cursor at the oldest entry returns an empty final page. Cursors
are bounded to 128 characters; each request traverses at most 10,000 links, including the skipped
prefix. Hitting that safety bound fails explicitly with 503, not a truncated success. Deep
history indexing, external database persistence and retention/compaction remain separate work.
Existing snapshot directories are not pruned.

## Remaining restoration

Producer reads and consumer synchronization operate over atomic local snapshots.
Central registry persistence, general completion notifications and optional Git
publication remain separate restoration work. Local manifest/file tests do not replace yhm01
full candidate acceptance, source/artifact audits or the 1.2.0 release gates.

## Consumer update notifications

An internal caller may `POST /internal/v1/master-data/sync` (or the configured regional
internal prefix) with the internal bearer and a JSON hint:

```json
{"scope":{"region":"jp","environment":"release","platform":"iOS"},"content_sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}
```

The body is limited to 4 KiB, unknown fields are rejected, and scope must exactly match
the receiving profile. A consumer without `master_sync` returns 503; invalid scope/hash
returns 400. Public read credentials cannot trigger synchronization. HTTP 202 acknowledges
an in-memory wakeup, not completed synchronization or verification of the supplied digest.

Bursts coalesce to at most one pending wakeup, including notifications during an active
update. The existing single sync worker always fetches the configured owner's current
manifest, validates scope/content/files, and atomically publishes; no origin/path or file
content is accepted from the hint. A stale digest therefore cannot roll back a consumer.
Periodic polling remains the fallback, and service restart performs an immediate poll,
so notification loss does not disable eventual synchronization.

### Configured owner notifications

The owner transport now has a bounded one-attempt sender with per-target in-memory
acknowledgements. Only HTTP 202 with the strict JSON status `accepted` advances that
state; errors leave the same content eligible for retry, and newer committed content
can supersede an unaccepted older hint. Restart intentionally permits resending CURRENT.
Requests use explicit origins, normal TLS verification, no ambient proxies, no redirects
and no implicit retries. Response bodies are bounded to 1 KiB and the request deadline
covers body consumption. Acceptance does not prove consumer synchronization.

Configure `master_notify` on a JP profile with `master_directory`:

```yaml
master_notify:
  interval_seconds: 30
  request_timeout_ms: 5000
  targets:
    - name: replica
      origin: https://replica.example
      token_env: SIRIUS_MASTER_REPLICA_NOTIFY_TOKEN
      regional_paths: true
      allow_http: false
```

The token must authorize the consumer's internal endpoint. It must differ from every
local profile's API/internal/peer/game/CDN/owner-read/updater/outgoing-node and proxy
credentials; only environment variable references belong in configuration. Target names
are unique, 1–64 ASCII letters/digits/underscores/hyphens. Configure 1–16 targets, a
10–3600 second reconciliation interval (default 30) and a 100–30000 millisecond per-target
request timeout (default 5000). HTTP requires explicit opt-in; origins cannot include
paths, URL credentials, queries or fragments. Notifications are disabled when omitted.

The worker reconciles committed CURRENT immediately at startup, after successful in-process
CDN/consumer publication and periodically. Polling also discovers independent CLI imports.
Targets are visited sequentially; the interval starts after the pass finishes, so a full
pass can take up to the target count times the request timeout plus local manifest reads.
Each failed target retries on a later pass, while accepted targets skip unchanged content.
One failed target does not prevent later targets from being tried. Local manifest failure
sends no hint; delivery failures never change CURRENT or roll back a publication.
Shutdown cancels active network work. Manifest reads run off the async executor and may
finish after cancellation, but perform no writes or notification transmission themselves.

Acknowledgements are transient. The durable source is CURRENT: restart resends its current
content, and intermediate versions can coalesce to the newest committed state. There is
no promise to deliver every installation event, or to deliver exactly once. A notification
only wakes the consumer; its configured owner and full verification determine installed
data. Periodic consumer polling remains necessary even after an accepted notification.
General publication webhooks, central registry persistence and Git publication remain
separate restoration work.

## Lookup by content identity

`GET /api/v1/master-data/by-hash/{content_sha256}/manifest` resolves a content identity
without requiring the caller to know a node's snapshot UUID. The hash must be exactly
64 lowercase hexadecimal characters (otherwise 400). Authorization is the normal public
Master-read bearer, and regional deployments use the corresponding regional prefix.

The lookup pins CURRENT and walks its committed predecessor chain. It selects the newest
matching installation, returns 404 when no reachable match exists, and stops at a legacy
snapshot without a publication record. It does not scan unrelated directories, so a staged
or orphaned snapshot is never exposed as a published hash. Corruption and the 10,000-link
traversal limit return 503 rather than an incomplete successful result. This is a bounded
local-history lookup; a persistent deep-history index and optional database backend remain
separate work.

Content identity includes region/environment/platform. Identical data in another scope
does not match. Reimporting the same content may change the selected snapshot UUID and
response ETag while preserving `content_sha256`; responses therefore use private/no-cache
and support If-None-Match, rather than promising immutable response bytes for the hash URL.
Pin the returned snapshot and table hashes when fetching files. As with other manifest
reads, indexed metadata is validated here; exact table payloads are verified when read.

## Git publication restoration status

The bounded Git execution primitive is implemented and locally tested on Unix. It invokes
commands directly, disables terminal prompting, drains stdout/stderr concurrently, limits
each stream to a caller-selected 1 KiB–16 MiB, and applies a command timeout up to 600 seconds.
Failures return static error categories without raw argv, stderr or stdout. Successful callers
receive bounded stdout only and must avoid exposing any credential-bearing command results.

Cancellation or failure kills the owned Git process group, including ordinary Git transport
helpers. Timeout/error cleanup explicitly waits up to two additional seconds for the direct
child; cancellation uses Tokio's kill-on-drop/reaping behavior. Process-group containment is
not a sandbox against a helper deliberately escaping its group. Non-Unix execution currently
returns an explicit unsupported error until platform-specific process-tree cleanup is added.

Local commits and explicit remote pushes are available through the commands below. Signing,
configurable author/proxy policies, service configuration/background integration and Windows
process-tree handling remain pending. No Git network commands run automatically.

### Local Master Git commits

On Unix, with Git available on PATH, a single-region configuration can commit its installed
Master snapshot into a dedicated managed bare repository:

```sh
SIRIUS_CONFIG_PATH=owner.yaml sirius-api-proxy master-git-commit ./master-git-state
```

`master_directory` selects the source and the profile supplies region/environment/platform.
The command does not create a game client, resolve game/CDN credentials, contact the game,
start the HTTP service or push to a remote. It prints `commit`, `content_sha256`, `changed` and `remote_verified` (false for local-only commits).
This receipt acknowledges only a local Git reference update.

The destination must be a new/empty directory or an existing matching Sirius Git state
directory. It contains an exclusive owner lock, a scoped ownership marker and `repository.git`.
Do not point it at a source checkout or another application's repository. A different scope,
symlinked destination/repository, unowned nonempty directory or locked owner fails explicitly.
This is a single-writer managed store, not a shared worktree for manual edits.

The command pins CURRENT, validates the manifest and every table's hash, size and JSON, and
stages exact plaintext bytes in temporary storage. It creates Git blobs with filters disabled
and constructs a fresh tree, so removed tables leave the new tree without altering old commits.
`sirius-publication.json` records the scoped source manifest and content identity without the
node-local snapshot UUID. Identical content therefore reuses the existing commit after a
reimport, rather than creating timestamp-only commits. No receipt, keys or CDN credentials
are included. The reserved publication filename cannot also be a table.

The branch is `master-data`, with new commits parented to the previous commit. A compare-and-swap
reference update commits the new tree; failed validation leaves the prior reference unchanged.
A cancelled/failed command may leave unreachable Git objects, and loss of the response at the
reference update is ambiguous: rerun the identical operation to inspect/reuse the committed tree.
The source CURRENT pointer is never changed by Git publication. Git commands share a 120-second
budget after local preparation; synchronous local reads can exceed that preparation time.

Commits currently use the fixed local identity `Sirius Master Publisher <sirius-master@localhost>`
and are unsigned. Ambient `GIT_*` variables are removed before process execution to prevent
repository redirects, injected config and trace destinations; terminal prompting is disabled.
Successful command output is bounded to 1 MiB per stream and generated stdin to 4 MiB. Explicit remote push and environment-referenced HTTP authorization are described below;
configured identities/signing/proxies and Windows support still remain
before the full optional Git publication feature is complete.

### Explicit remote Git push

```sh
SIRIUS_CONFIG_PATH=owner.yaml sirius-api-proxy master-git-push ./master-git-state https://git.example/master-data.git
```

The command uses the same verified snapshot, scoped state lock and `master-data` branch. It
checks the remote branch before creating a new local commit. An absent remote branch can be
created; a remote equal to or behind the local branch can be advanced. A remote ahead of or
diverged from local history fails verification before adding a local commit. A new empty
local store will not overwrite an existing remote branch; retain/recover the original managed
state and reconcile deliberately. The command never force-pushes, merges or resets either
branch to conceal divergence. The force marker used when fetching only refreshes a private
local inspection ref; it is never part of a push refspec.

A rejected/lost push leaves the local commit available. Repeating the command with unchanged
source data reuses that commit and retries the push. After Git reports success, a separate
remote-ref query must confirm exactly the submitted commit before `remote_verified: true`
is returned. `changed` refers to creation of a local commit, not whether network work occurred.
Remote races, errors and timeouts produce an error, never a false publication receipt. The
source Master CURRENT pointer is independent and is never rolled back by Git failures. All
Git work, including checks, commit creation, push and acknowledgement, shares the existing
120-second budget after local preparation.

For HTTP authorization, set `SIRIUS_MASTER_GIT_AUTHORIZATION` externally to a complete single
header value such as `Authorization: Bearer …` or `Authorization: Basic …`. Do not put credentials
in REMOTE_URL. Git receives only the environment variable name through `--config-env`; the
secret value is not embedded in command arguments or stored in repository configuration. Use a
dedicated Git publication credential scoped to the selected repository. Git must support `--config-env`
for this authorization mechanism. No ambient credential helper, Git askpass, system/global Git
configuration or HTTP proxy is used. Certificate verification stays enabled; redirects are
disabled. SSH/custom transport URLs, URL credentials/query/fragments and malformed headers
are rejected. CLI network publication accepts HTTPS only.

An explicitly supplied `file:///absolute/path/to/repository.git` URL is supported for local
mirrors and tests, with authorization unset. The library's HTTP test opt-in is not enabled by
the CLI. Configurable proxy/signing/author settings, Windows process containment and final production Git acceptance remain
separate restoration requirements.

### Background Git publication

JP profiles may enable `master_git` with a separate `state_directory` and optional `remote`.
`master_directory` is required. Omit `remote` for local commits only; omit `master_git` to
leave the feature disabled. See the commented single-profile example configuration.

The worker reconciles CURRENT at startup, after successful in-process Master installations,
and every `interval_seconds` (default 300; range 10–86400). Polling also discovers CLI imports
and retries failed publication. Git and consumer notifications use independent wake signals.
Intermediate installations may coalesce into the latest snapshot. Keep the managed state
across restarts: Git refs are durable, while displayed last-success status is rebuilt at startup.

`GET /internal/v1/master-data/git` (or `/internal/v1/{region}/master-data/git`) requires the
internal bearer. It reports pending/running/ready/failed/stopped/disabled, the last successful
receipt and a static error code; it does not expose paths, remote URLs or credentials. Failure
never rolls back installed Master data. Shutdown cancels active network work and releases the
state lock; the next startup reconciles an ambiguous previous push against remote refs.

Service remote configuration uses `url`, optional `authorization_env`, and explicit `allow_file`
or `allow_http` opt-ins (both default false). Prefer HTTPS. Authorization must be a dedicated
Git credential: deployment preparation rejects reuse of other service credentials, including
underlying Bearer tokens and decoded Basic passwords. No remote is contacted at preparation;
the configured background worker performs publication after service startup.
