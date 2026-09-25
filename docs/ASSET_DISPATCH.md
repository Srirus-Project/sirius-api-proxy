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

A POST transport failure or shutdown can leave acceptance ambiguous. Because remote job retention
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
included. `ready` means the worker processed this request, not that every remote job is healthy;
inspect individual persisted states and failure codes.

Adoption returns the persisted entry with HTTP 200, unknown identities return 404,
and disallowed transitions return 409. Malformed keys/UUIDs return 400; unknown body fields
are rejected and the body limit is 4 KiB. Adoption does not submit or complete a job. The
next scheduled reconciliation checks its remote identity and outcome just as offline recovery
does. Repeating the same UUID is safe; do not switch to a different UUID after an uncertain reply.

Commands use a bounded 16-request queue and wait at most five seconds. The sole worker
processes them between network reconciliation passes, so a long game/updater request can
make the management endpoint return 503. Stopped workers and queue saturation also return
503. Requests abandoned while still queued are discarded. Once persistence has started, a
lost or timed-out response does not prove the change was rolled back: query state or repeat
the identical adoption. Management requests do not accelerate observation/polling or race
in-flight submissions. Automatic replay of uncertain POSTs remains intentionally disabled.

History has a hard capacity and no automatic pruning; a full history refuses new identities while
existing jobs continue reconciliation. Safe operator compaction still needs implementation.
Changing the profile revision intentionally creates a new identity. The updater resolves profiles
at execution time, so this revision is an operator-controlled re-export marker, not an immutable
copy of its configuration. Coordinate profile changes with active work.

Writes sync temporary files before atomic replacement. Process restart is tested; power-loss
persistence across every filesystem is not claimed. Persistence failure stops the dispatch worker
and emits an error while the HTTP proxy remains available. Monitor these errors and inspect the
state ledger. The online endpoint returns 503 once the worker has stopped; it does not restart
a worker or erase its failure state.

Full production acceptance, completion notifications and the 1.2.0 release remain pending.
