# Account pool

Each region can configure up to 64 existing game accounts. This does not register
accounts or implement unverified Global authentication. Global authenticated RPCs
remain unsupported. Anonymous RPCs never receive game credentials.

```yaml
accounts:
  - name: primary
    player_id_env: SIRIUS_JP_PRIMARY_ID
    credential_env: SIRIUS_JP_PRIMARY_CREDENTIAL
  - name: secondary
    credentials_file: /run/secrets/sirius-jp-secondary.json
account_pool:
  failure_threshold: 2
  cooldown_seconds: 30
session_lock: true
```

Use either paired environment references or one credentials file per account.
Files contain a JSON object with exactly `player_id` and `credential` string fields.
They must be regular files, at most 16 KiB, with valid nonblank HTTP-header values.
On Unix, group/other permissions must be absent (for example mode 0600). Protect
files with equivalent ACLs on Windows. Paths are relative to the working directory
unless absolute. Do not commit real files, identities or credentials.

Legacy `player_id_env` and `player_credential_env` configure a single account named
`default`. They cannot be combined with `accounts`. Names must be unique safe
identifiers; duplicate player identities are rejected so one account cannot acquire
independent locks through aliases.

Public authenticated reads choose the available account with the fewest active or
queued calls; ties rotate in configuration order. A reservation is released on
completion, timeout or caller cancellation. `session_lock: true` serializes each
account's logical call, including its identity check, while different accounts can
work concurrently. False keeps the existing opt-out for concurrent use of a single
session. Actual game-server support for single-session concurrency remains unproven.
Anonymous bootstrap is shared within the region and carries no account headers.

A gRPC permission/authentication failure (7 or 16) disables the selected account until
successful credential reload. Transport/protocol failures, deadlines and gRPC
8/13/14 increment its failure count; reaching `failure_threshold` cools it down for
`cooldown_seconds`. Threshold is 1..100 and cooldown is 1..3600 seconds. A successful
call clears transient failures. Failures before an authenticated attempt (including
anonymous bootstrap and queue deadlines) do not penalize the account. Exhaustion
returns 503. The failed logical request is never automatically replayed with another
account; a later request can select another healthy account.

## Internal management

All these routes require the region's internal bearer token. In multi-region mode,
insert the region after `/internal/v1`, for example `/internal/v1/jp/accounts`.

| Method and path | Behavior |
| --- | --- |
| `GET /internal/v1/accounts` | Names, generation, active/queued calls and health; no player IDs or secrets |
| `POST /internal/v1/accounts/reload` | Read and validate all configured sources, drain logical calls, then atomically replace the pool |
| `GET /internal/v1/accounts/{name}/identity` | Query the explicitly selected account's identity |
| `GET /internal/v1/accounts/{name}/player-data` | Verify identity and query private data using the same account and lock |

Legacy `/internal/v1/account` and `/internal/v1/account/player-data` always use the
first configured account (the legacy `default` when applicable). They do not fall
back to another identity if that account is unhealthy. Prefer named routes when
operating a pool. Public ranking responses continue to strip service-account fields.

For live rotation, atomically replace a credential file, then call reload. A failed
candidate leaves the entire previous pool and generation intact. Successful reload
resets health and increments the generation after existing calls drain, so credentials
and session locks cannot be replaced underneath a request. Account membership and
source paths are startup configuration; changing them requires restart. Environment
values are reread from the running process; changing a parent shell's environment
cannot update a running service, so use files for live rotation. Health is transient
and resets on service restart. Management operations never log credential contents.
