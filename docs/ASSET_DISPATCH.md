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

## Pending integration

This module is not yet wired into deployment configuration or a background worker. No new
operator setting is exposed until it actually starts durable dispatch/reconciliation.

`asset_outbox::Outbox` now persists stable dispatch identities with exclusive process ownership.
Identity includes a destination digest, request region/profile/operation, explicit profile revision,
environment/platform/resource version/platform hash and required output scope. New observations
are committed before becoming pending work; sending is committed before network side effects.
The original first-send timestamp survives retries/restart so later reconciliation can enforce
an ambiguity window rather than silently replaying old keys. Acknowledgement pins one job UUID;
completion pins its catalog digest and optional publication UUID. Failed identities remain reserved.

The outbox provides transitions, not scheduling or proof of successful remote output: its caller
must validate receipts and scope before committing completion. History has an explicit capacity
(1–100,000 entries) and no automatic pruning. A full history fails rather than forgetting a known
identity and redispatching it. Operator-controlled compaction/recovery still needs integration.
Writes use synced temporary files and atomic replacement; process restart is tested, but this is
not a claim of power-loss durability across every filesystem. Corrupt state fails closed.

The owner still needs bounded retry/reconciliation, explicit failed/cancelled/pruned-job
handling and receipt scope checks. HTTP 202 means accepted, not exported or published.
Matching must include environment/platform/resource version/platform hash; requested full
export and storage requirements must be checked independently of terminal status.

Updating a configured profile can change export semantics without changing the catalog.
The owner's identity must include an explicit profile revision so an intentional re-export
gets a new key. Job record retention bounds remote idempotency: do not assume an old pruned
key can be replayed indefinitely without new work. A delayed job may process a newer snapshot;
only its actual outcome may be recorded as completed work.

Full automatic dispatch, production acceptance and 1.2.0 release remain pending.
