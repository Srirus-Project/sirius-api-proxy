# Asset dispatch restoration

The original Haruki API owns recurring version checks and submits work to the asset service.
Sirius will keep this responsibility split: a background owner reconciles catalog identity,
while normal snapshot reads/refreshes do not themselves dispatch jobs. This prevents an
updater's snapshot refresh from recursively scheduling another export.

## Implemented transport

`asset_jobs::Client` submits configured region/profile/operation requests with an idempotency
key to `/api/v1/jobs` and polls `/api/v1/jobs/{id}`. It validates the updater's typed result:
request identity, job UUID, submission key digest, terminal outcome state, region, catalog
SHA-256 and publication identifier. Legacy completed jobs without an outcome can be read,
but do not prove successful catalog reconciliation. Extra response fields permit additive
updater contract evolution. Unknown statuses fail explicitly.

The caller supplies one configured origin and its dedicated updater token. HTTPS uses normal
certificate verification; plain HTTP requires explicit opt-in for private-network deployments.
There are no ambient proxies, redirects, automatic retries or fallback destinations. Requests
have bounded connection/whole-request deadlines and a 64 KiB streamed response limit. Status
errors expose the numeric HTTP status only, never the response body, destination or token.
The updater token is unrelated to game credentials, public API tokens and snapshot tokens.

## Background worker configuration

`asset_dispatch` is optional and belongs to each region configuration (including single-region
legacy deployments). It runs once after listener startup, then waits the configured interval
between cycles. Normal public/internal snapshot reads and refreshes never directly enqueue work.
Each cycle requests a fresh game Version observation, records configured destination/profile
identities, and reconciles up to 16 nonterminal entries using a persisted rotating cursor.
Long-running or unavailable jobs cannot permanently monopolize the first batch. Selection is
committed before network work; restart continues after the previous batch, and interrupted work
returns on a later rotation. A failed game refresh still permits polling
previously submitted work. Shutdown cancels requests; pre-send state makes interrupted POSTs visible.

```yaml
asset_dispatch:
  state_directory: ./state/jp-assets
  interval_seconds: 60
  request_timeout_ms: 10000
  history_capacity: 10000
  targets:
    - origin: https://asset-updater.example.com
      token_env: SIRIUS_ASSET_UPDATER_TOKEN
      # Optional when the updater requires a client User-Agent prefix.
      user_agent: SiriusClient/api-proxy
      allow_http: false
      profile: jp-full
      profile_revision: "1"
      require_full_catalog: true
      require_full_export: true
      require_publication: true
```

Set `require_publication: false` when retaining verified exports locally without a storage
provider. Full-export requirements accept retained local files or a verified storage publication;
validation-only export does not satisfy them. A completed job must match region, environment,
platform, resource version and platform hash. Missing/legacy outcomes or mismatched scope are
terminal reconciliation failures. The actual catalog digest/publication UUID are persisted only
on a match. HTTP acceptance is not completion.

The listener configuration rejects updater tokens equal to any configured region's API, internal
or CDN credentials. Environment references are resolved at startup. Each region needs its own state
directory; existing state belonging to another region/environment/platform is rejected.
Origins must be roots without credentials/query/fragment. Explicit plaintext can be used on a
trusted private network; the bearer token is sent unencrypted in that mode. Targets are unique by
normalized origin and profile. Bounds: 1–16 targets, 10–86,400-second intervals, 100–300,000-ms
request deadlines, 1–100,000 retained identities.

## Durable state and recovery boundaries

The outbox uses exclusive process ownership and atomic file replacement. Identity includes a
destination digest, region/profile/operation, explicit profile revision, environment/platform,
resource version/platform hash and required scope. New observations are committed before sending;
possible submission is persisted before POST. Known job IDs survive restart and continue polling.
Temporary GET failures leave the job submitted for the next cycle. Terminal failures/cancellation,
404 for a previously acknowledged job, malformed replies and removed targets are recorded as failed
and logged with sanitized codes. They are not repeatedly exported.

A POST answered with a definite status was not accepted, so it is classified instead of being
left ambiguous (1.2.4; earlier releases recorded every failed POST as `submission_ambiguous`):

| Updater answer to the POST | Result |
| --- | --- |
| 429 (queue full) or 503 (draining) | Back to pending; submitted again at the next cycle with the same Idempotency-Key. After 10 such answers since the process started: `submission_refused` |
| 400, 404, 405, 413, 415, 422 | `submission_rejected` (for example an unknown profile) |
| 401, 403 | `submission_unauthorized` (wrong or unauthorized token) |
| 409 | `idempotency_conflict` (the key is already bound to a different request) |
| Invalid key or request detected before sending | `asset_dispatch_request_invalid` |

These codes are terminal and cannot be adopted: nothing was accepted. Fix the target or updater
configuration, then raise `profile_revision` to dispatch again. Warnings carry `region`,
`profile`, `target` (the first 12 hex digits of the destination digest, never the origin),
`stage` (`submit`/`poll`) and the HTTP `status`; response bodies are never read or logged.

Any other POST failure (transport error, timeout, other 5xx) or a shutdown can leave acceptance
ambiguous. Because remote job retention
has no minimum time guarantee, the worker does **not** blindly replay such submissions after a
restart or lost response. The next reconciliation records `submission_ambiguous`. Operators must
inspect the updater's retained job list before intentionally requesting new execution. Use a new
profile revision only after resolving the previous execution and deciding that a new job is needed.
Do not delete the state directory to retry: doing so forgets completed catalog identities too.
Offline status and adoption commands are available after stopping the API process:

```sh
sirius-api-proxy asset-dispatch-status ./state/jp-assets
sirius-api-proxy asset-dispatch-adopt ./state/jp-assets DISPATCH_KEY EXISTING_JOB_UUID
```

Inspect the updater's authenticated job list and match the job's `idempotency_sha256` to the
SHA-256 of the dispatch key before choosing its UUID. Adoption only transitions an ambiguous
submission (or an unacknowledged malformed response) to submitted; it does not send a request or
mark completion. After restart, the worker validates the job key digest, request and output
identity/scope before completion. A wrong adopted ID fails reconciliation. Already completed,
failed-with-known-job, or pending work cannot be reassigned; repeating the same adoption is safe.
Both commands use exclusive state ownership and reject nonexistent state directories. Status
prints JSON without credential values. They require no game/CDN credentials or network access.

Online administrative routes are available when `asset_dispatch` is configured:

- `GET /internal/v1/asset-dispatch/status` (see [Worker status](#worker-status))
- `GET /internal/v1/asset-dispatch/entries?limit=50&after=DISPATCH_KEY`
- `POST /internal/v1/asset-dispatch/entries/DISPATCH_KEY/adopt` with
  `{"job_id":"EXISTING_JOB_UUID"}`

Multi-region deployments insert the region after `/internal/v1`, for example
`/internal/v1/jp/asset-dispatch/entries`. Use that profile's internal bearer; public tokens
cannot inspect or change dispatch state. Disabled profiles have no dispatch routes.

Lists return `status`, `total`, `entries` (each has `key` and `entry`) and nullable
`next_after`. Limit defaults to 50 and accepts 1–200. Entries sort by dispatch key; pass
`next_after` as `after` for the next page. Pages are live views, not a frozen export:
new identities can appear before a previous cursor. No origins or credential values are
included. The list's `status` is always the constant `ready`: it means the worker processed this
request, not that the worker or every remote job is healthy. Use `/status` for worker health and
inspect individual persisted states and failure codes.

Adoption returns the persisted entry with HTTP 200, unknown identities return 404,
and disallowed transitions return 409. Malformed keys/UUIDs return 400; unknown body fields
are rejected and the body limit is 4 KiB. Adoption does not submit or complete a job. The
next scheduled reconciliation checks its remote identity and outcome just as offline recovery
does. Repeating the same UUID is safe; do not switch to a different UUID after an uncertain reply.

Commands use a bounded 16-request queue and wait at most five seconds. The sole worker
processes them between network reconciliation passes, so a long game/updater request can
make the management endpoint return 503. Stopped workers and queue saturation also return
503. `GET /status` tells these apart: `reconciling` (or `observing`) means the worker is busy
and the command can be retried; `stopped` with a `stop_reason` means it will not be processed
until restart. Requests abandoned while still queued are discarded. Once persistence has started, a
lost or timed-out response does not prove the change was rolled back: query state or repeat
the identical adoption. Management requests do not accelerate observation/polling or race
in-flight submissions. Automatic replay of uncertain POSTs remains intentionally disabled.

The working ledger has a hard capacity and no automatic pruning. Confirmed completions can be
explicitly archived to free capacity while retaining permanent deduplication, as described below.
Capacity exhaustion refuses new observations (reported by `/status` as the observation result
`capacity_exhausted`) but leaves reconciliation and online management available. If the worker has stopped after a reconciliation persistence error, use offline
maintenance before restarting it; an online command cannot revive a stopped worker.
Changing the profile revision intentionally creates a new identity. The updater resolves profiles
at execution time, so this revision is an operator-controlled re-export marker, not an immutable
copy of its configuration. Coordinate profile changes with active work.

Writes sync temporary files before atomic replacement. Process restart is tested; power-loss
persistence across every filesystem is not claimed. Persistence failure stops the dispatch worker
and emits an error while the HTTP proxy remains available. Monitor these errors and inspect the
state ledger. The entry endpoints return 503 once the worker has stopped, while `GET /status`
keeps answering 200 with `status: "stopped"` and `stop_reason: "asset_outbox_storage"`. Neither
restarts a worker or erases its failure state.

Released in 1.2.0 with local integration tests. The yhm01 production acceptance runs submit jobs
directly to the asset updater, so this automatic dispatch path has not itself been exercised in a
production deployment.

Targets may set `user_agent` to 1–256 printable ASCII characters (not whitespace-only).
It is sent on both job submissions and polling, including after restart, and supports the
updater's optional `user_agent_prefix` filter. Bearer credentials remain independently required.
Omitting the field preserves the existing transport behavior. It identifies the client, not
a secret; do not place credentials in it. Changing it does not change durable job identity
or create a second submission of the same catalog.

## Worker status

`GET /internal/v1/asset-dispatch/status` (regional deployments: `/internal/v1/{region}/...`)
uses the same internal bearer as the other routes; a missing or wrong token returns 401. It
always returns 200 once authorized. It does not use the 16-command queue, so it answers while a
reconciliation holds the worker and after the worker has stopped or its task has exited. No
configuration enables it; profiles without `asset_dispatch` have no route.

| Field | Meaning |
| --- | --- |
| `status` | `pending` (built, no cycle yet), `observing`, `reconciling`, `idle` or `stopped`. |
| `stop_reason` | Only when stopped: `shutdown`, `asset_outbox_storage` (persistence failure) or `exited` (the task ended without reporting, for example a panic or a worker that never ran). |
| `updated_at` | Last time the worker published this status. |
| `cycle_started_at` | Start of the current observation/reconciliation cycle; `null` otherwise. |
| `last_cycle_at`, `next_cycle_at` | End of the last successful cycle and the next scheduled one. |
| `last_observation` | `{at, result, resource_version}`. `result` is `recorded`, `unavailable` (no fresh game snapshot), `rejected`, `capacity_exhausted` or `storage_failed`; `resource_version` is set only for `recorded`. |
| `last_reconcile` | `{at, result, batch, transport_errors}`. `result` is `completed` or `storage_failed`; `batch` is the number of entries selected (at most 16); `transport_errors` counts updater requests that failed without a definite answer in that pass. |
| `entries` | Working-ledger counts: `total`, `capacity`, `pending`, `sending`, `submitted`, `completed`, `failed` and `busy_retrying` (pending entries the updater refused as busy and that will be resubmitted). |
| `failed_by_code` | Failed working entries by code. Only codes this worker writes are named; any other persisted code counts as `other`. Zero counts are omitted. |

Counts cover the working ledger only, like the list's `total`; archived completions are
excluded. They are refreshed at each phase change, before each submission POST and after each
management command, so during a long reconciliation they can lag by up to one batch;
`updated_at` and `cycle_started_at` show how old they are. `last_*` values and `busy_retrying`
live in memory and are `null`/zero after a restart; the ledger and entry routes remain the
durable record. Every string is a fixed code: the status never includes origins, tokens,
destination digests, updater responses or error text.

## Completed-history compaction

Use `POST /entries/{key}/archive` under the existing authenticated asset-dispatch management
prefix with `{"job_id":"EXPECTED_JOB_UUID"}`. Only `completed` entries with that exact job UUID
can be archived. Pending, sending, submitted and failed entries remain in the working ledger,
including ambiguous submissions that still need reconciliation. Successful/repeated identical
requests return200 with `archived:true` and the complete entry. Invalid syntax returns400;
unknown entries, wrong UUIDs and ineligible states return409; persistence failures return503.
`GET /entries/{key}` reads an active or archived entry and reports `archived`; unknown keys
return404. These routes use the same internal token, sole-owner queue, limits and timeout
semantics as adoption. Listing `/entries` and its `total` cover working entries only.

The archive is `STATE_DIRECTORY/completed/{dispatch_key}.json`. Its complete terminal receipt
is written and synchronized before removal from the atomic working ledger. Unix also syncs the
archive/root directories before removal. An interruption can leave both copies; retrying the
same request reconciles them. Conflicting/corrupt archives fail closed. Re-observation checks
the archived identity before capacity/admission, so the same catalog does not submit another job,
including after restart. Completed archives do not occupy working capacity or load into the
in-memory ledger. They continue to consume disk space; there is no automatic archive deletion.

For maintenance while the worker is stopped (exclusive state ownership is required):

```sh
sirius-api-proxy asset-dispatch-archive STATE_DIRECTORY DISPATCH_KEY JOB_UUID
sirius-api-proxy asset-dispatch-entry STATE_DIRECTORY DISPATCH_KEY
```

The first prints the archived entry and is idempotent. The second prints either the active or
archived entry (null when absent). Existing `asset-dispatch-status` still prints the working
ledger. Back up `completed/` together with `outbox.json`; deleting archived records destroys the
corresponding deduplication evidence and can permit re-export on a later observation. This is
receipt compaction, not a retry/reset mechanism or proof of remote asset retention.
