# Account pool

Each region can configure up to 64 game accounts. JP accounts are existing accounts
with a static player ID and credential. HK/EN/KR accounts are SDK guest identities that
log in lazily; see [Global accounts](#global-accounts). The proxy never registers a JP
account. Anonymous RPCs never receive game credentials.

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
Anonymous calls carry no account headers. They no longer share one regional lock: with
`session_lock: true` up to `upstream.anonymous_max_inflight` of them (default 4, at most
`max_inflight`; 1 restores the 1.2.x serialization) run at once. The Version bootstrap of
authenticated calls and the version freshness refresh stay single-flight. With `upstream.coalesce_public_reads: true`, one
account's ranking result (or failure) answers every identical concurrent ranking request and
is reported to account health once; the default runs each ranking request on its own account
selection.

A gRPC permission/authentication failure (7 or 16) disables the selected account until
successful credential reload. gRPC 8/13 increment its failure count; reaching
`failure_threshold` cools it down for `cooldown_seconds`. Threshold is 1..100 and cooldown is
1..3600 seconds. A successful call clears transient failures. Failures before an authenticated
attempt (including anonymous bootstrap and queue deadlines) do not penalize the account, and
neither does a response carrying `UNDER_MAINTENANCE` (whatever its gRPC status; it answers 503
`maintenance`). Responses carrying `MASTER_VERSION_MISMATCH` or `CLIENT_UPDATE_REQUIRED`, on
the request or on the identity check before private data, do not count either, whatever their
gRPC status: they describe the proxy's version headers, not the account (see
[version header freshness](REQUEST_POLICY.md#version-header-freshness)). Since 1.3.0 neither does a JP
`PLAYER_NOT_FOUND` on `profile` with gRPC 2 or 7: it names the looked-up player and answers 404
(see [REGIONS.md](REGIONS.md)). gRPC 7 or 16 with any other code, or none, still disables the
account. Exhaustion returns 503. The failed logical request is never automatically
replayed with another account; a later request can select another healthy account.

Response-cache hits lease no account: since 1.3.0 they are answered before account selection, are
never counted in `active_calls` and never touch health, so a hit cannot re-enable a cooling
account. With every account cooling or disabled, a retained entry may still answer inside a
configured `stale_while_revalidate_ms` window (see
[hits before admission](RESPONSE_CACHE.md#hits-before-admission)).

Path-class faults (transport and protocol failures, deadlines, and gRPC 14 without an
application code) may belong to the shared upstream path rather than to the account, so each
region tracks them separately (see
[REQUEST_POLICY.md](REQUEST_POLICY.md#upstream-path-health)):

- Consecutive path faults form a streak. While every fault of the streak came from one account,
  each is charged to that account like gRPC 8/13 and can cool it down.
- A second account, or any anonymous call (Version, announcements, server list and so on),
  failing in the same streak makes it the path's fault: the streak's charges are withdrawn from
  every account (lifting a threshold cooldown that no longer holds), later faults of the streak
  charge no account, and the path opens at `failure_threshold` faults.
- Any gRPC answer ends the streak. A single-account deployment without anonymous traffic
  therefore behaves as before; a real outage no longer cools every account.

Withdrawal never lifts signal cooldowns (`AEGIS_*`, `CONCURRENT_DEVICE`, the login interval or
daily cap), gRPC 8/13 charges or a disabled account. The rule can occasionally excuse a bad
account whose failure coincided with another's; it then needs `failure_threshold` new failures
to cool again.

## Internal management

All these routes require the region's internal bearer token. In multi-region mode,
insert the region after `/internal/v1`, for example `/internal/v1/jp/accounts`.

| Method and path | Behavior |
| --- | --- |
| `GET /internal/v1/accounts` | Names, generation, active/queued calls and health; Global accounts add session state; `path` (and on Global `sdk_path`) reports path health as `{state, failures, attributed, cooldown_remaining_ms}` with `state` `closed`, `open` or `probing`; no player IDs or secrets |
| `POST /internal/v1/accounts/reload` | Read and validate all configured sources, drain logical calls, then atomically replace the pool |
| `GET /internal/v1/accounts/{name}/identity` | Query the explicitly selected account's identity (JP: Whoami; Global: the PlayerLogin result) |
| `GET /internal/v1/accounts/{name}/player-data` | Verify identity (JP) and query private data using the same account and lock |

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

## Global accounts

HK, EN and KR accounts are Bilibili OneSDK guest identities. Each account references a private
identity file; the game credential is obtained with `PlayerLogin` when it is first needed and is
kept only in memory.

```yaml
region: en
session_lock: true            # required for Global accounts
accounts:
  - name: en-guest
    global_identity_file: /run/secrets/sirius-global-guest-1.json
account_pool: {failure_threshold: 2, cooldown_seconds: 30}
global_login:
  sdk_origin: https://l11-sdk-login-intl.biligame.net   # default; l11, l12 or l13 only
  sdk_app_key_env: SIRIUS_GLOBAL_SDK_APP_KEY            # default; the APK one_appkey
  sdk_timeout_ms: 15000                # 1000..60000
  login_min_interval_seconds: 300      # 0..86400, between two login attempts of one account
  max_logins_per_day: 24               # 1..100, per account in any rolling 24 hours
  aegis_cooldown_seconds: 900          # 60..86400, after a login queue signal
  concurrent_device_limit: 3           # 1..100 CONCURRENT_DEVICE signals per 24 h disable
  state_directory: /var/lib/sirius/sdk-sessions  # optional; SDK sessions survive restarts
  sdk_refusal_retry_seconds: 86400     # 3600..2592000, before a refused identity is retried
```

Rules checked at startup:

- `global_identity_file` is accepted only for `hk`, `en` and `kr`, and is exclusive with the
  JP sources (`player_id_env`/`credential_env`, `credentials_file`). Global profiles reject
  static game credentials, including the legacy `player_id_env`/`player_credential_env`.
- A Global profile with accounts needs a `global_login` section and `session_lock: true`.
  `global_login` is rejected for JP.
- `sdk_origin` must be exactly one of `https://l11-sdk-login-intl.biligame.net`,
  `https://l12-sdk-login-intl.biligame.net` or `https://l13-sdk-login-intl.biligame.net`.
  SDK requests are HTTPS only, never follow redirects, are never retried and read at most
  1 MiB. They use the profile's `upstream` proxy, if one is configured.
- The SDK app key is read from `sdk_app_key_env` at startup. It ships inside the APK but is
  treated as a secret: it never appears in configuration, logs or responses.
- One SDK identity may appear at most once per region. The same file may be referenced by the
  `hk`, `en` and `kr` profiles: one SDK identity has an independent player on each server, and
  the regions of one deployment share its SDK session (see [Shared SDK sessions](#shared-sdk-sessions)).
- In a multi-region deployment every region's `global_login.state_directory` must be the same
  (or absent everywhere).

### Identity file

A JSON object, schema 1, a regular file of at most 16 KiB. On Unix, group and other permission
bits must be absent (mode 0600); protect it with equivalent ACLs on Windows. The proxy never
writes it.

```json
{
  "schema": 1,
  "sdk": {"uid": "<SDK uid>", "access_key": "<SDK access_key>", "id_token": "<SDK id_token or empty>"},
  "device": {
    "udid": "<SDK udid>", "model": "<Build.MODEL>", "pf_ver": "<Android release>",
    "dp": "<width*height>", "net": "4", "operators": "5", "adid": "",
    "lang": "<SDK language>", "time_zone": "<IANA zone>", "isRoot": "0",
    "unity_device_model": "<SystemInfo.deviceModel>",
    "unity_operating_system": "<SystemInfo.operatingSystem>",
    "unity_device_id": "<SystemInfo.deviceUniqueIdentifier>"
  },
  "players": {"en": {"expected_player_id": "<optional pin>"}}
}
```

- `sdk` is what `tourist.login` returned. `uid` may be a string or an integer; `mid` is optional.
- `device` holds the SDK common parameters and the Unity values of **one** real device or
  emulator. Keep them stable for the lifetime of the identity. Do not invent or mix values.
  `adid` may be empty; `operators` defaults to `5`.
- `players` is optional. When a region is pinned, a PlayerLogin that returns another player
  disables the account (`PLAYER_MISMATCH`).
- A template with placeholders is in [examples/global-identity.example.json](examples/global-identity.example.json).

### Login lifecycle

Nothing is sent at startup. The first authenticated request of an account, while holding the
account's session lock, performs:

1. SDK `POST /gapi/client/cache.login` (signed like the Android SDK). The uid must not change;
   the original `access_key` is kept, a new `id_token` is used when returned. It is skipped
   while the identity's [shared SDK session](#shared-sdk-sessions) is valid.
2. `PlayerLoginService/PlayerLogin` on the region's API endpoint with `area_id` 6,
   `global_channel_id` 2001, `brand_id` 5, `platform` 0 (omitted), `client_package`
   `com.bilibili.sirius` and the Unity device values. It sends only `x-platform`,
   `x-client-version`, `x-request-id` and `x-player-bid` (the SDK uid).
3. The returned `credential.id` and `credential.credential` become the in-memory session.

Concurrent requests queue on the session lock, so they trigger one login. Authenticated calls
then send `x-player-id`, `x-player-credential`, `x-player-bid`, `x-master-version`,
`x-resource-version` (the VERSION `resourceVersion`), `x-platform: android`, `x-client-version`
and `x-request-id`. `x-device-id` and `x-override-device-id` are never sent. **Whoami is never
sent on Global** (production rejects it); the account identity is the PlayerLogin result.
A restart or `POST .../accounts/reload` drops the session, and the next request logs in again.

Every login attempt counts toward `login_min_interval_seconds` and `max_logins_per_day`.
A login that is not allowed yet cools the account down until it is, and the request returns
503 without contacting the SDK or the game. Login history survives an account reload but not a
restart. While the region's game path or the SDK path is open, a login is refused with 503
`upstream_unavailable` before the attempt is counted, so an outage spends no login budget.

Signals are read from the `x-sirius-error-code` trailer first, then the gRPC status. The logical
request that received a signal is never replayed.

| Signal | Effect |
| --- | --- |
| `TOKEN_*`, or gRPC 16 without a code | Drop the session and mark the shared SDK session stale; the next request runs `cache.login` and PlayerLogin again |
| `PLAYER_NOT_*`, except `PLAYER_NOT_FOUND` on `profile`/`event_deck` (the looked-up player: 404, session kept) | Drop the session; the next request runs PlayerLogin again |
| A second `TOKEN_*`/`PLAYER_NOT_*`/16 before any successful call | Disable the account |
| `CONCURRENT_DEVICE` | Drop the session and cool down for `cooldown_seconds`; the `concurrent_device_limit`-th signal in 24 h disables |
| `BAN_*` | Disable the account |
| `AEGIS_*` (login queue, server full) | Cool down for `aegis_cooldown_seconds`; the queue is never polled |
| `UNDER_MAINTENANCE` | Recorded as maintenance; no account penalty |
| `MASTER_VERSION_MISMATCH` / `CLIENT_UPDATE_REQUIRED` | Recorded in `last_error_code`; no session drop, no penalty (a failed PlayerLogin still answers 503 and counts toward the login limits); a mismatch refreshes the Master version before the next call |
| gRPC 7 without a code | Disable the account |
| A changed uid | Disable the account |
| SDK code 200007 (`SDK_CAPTCHA`) and other nonzero SDK codes (`SDK_REFUSED`, for example 900200 risk control) | Disable the account in every region that uses the identity; no `cache.login` of the identity for `sdk_refusal_retry_seconds`, across reloads and (with `state_directory`) restarts. For a CAPTCHA, complete verification in the official client first |
| SDK transport failure, deadline or malformed response (`SDK_TRANSPORT`, `SDK_PROTOCOL`) | Recorded as `last_error_code`; counts toward the SDK path, never cools the account (the login interval and daily cap bound retries). `failure_threshold` consecutive failures open the SDK path |
| gRPC 8/13 | Transient failure, as for JP |
| Transport, deadline, malformed responses, gRPC 14 without a code | Path-class fault, attributed as for JP |

A disabled account stays disabled until `POST .../accounts/reload` or a restart. Reloading
re-reads the identity file. An identity under an SDK refusal is disabled again on its next
login until `sdk_refusal_retry_seconds` have passed, unless its identity file now holds another
access key.

`GET /internal/v1/accounts` adds these fields for Global accounts: `session_state` (`none`,
`active`, `relogin_pending`, `cooling` or `disabled`), `last_login_at`, `logins_24h` and
`last_error_code` (an application or SDK code such as `TOKEN_ILLEGAL` or `SDK_CAPTCHA`, never
text) and, after `SDK_REFUSED`, `last_sdk_code` (the SDK's numeric refusal code, also logged as
`sdk_code`; it tells an invalidated identity from a risk-control block), and `sdk_session`
(the identity's shared SDK session: `none`, `valid`, `stale`, `expired` or `refused`). No SDK uid, token, player ID, credential or device value appears in status, errors, logs
or response cache keys. Response cache keys use the account name and SDK uid (hashed), not the
rotating credential, so a re-login keeps cached public responses.

### Shared SDK sessions

An SDK identity is one Bilibili account. The official client revalidates it with `cache.login`
once per launch and then logs in to a single server. The proxy used to revalidate the same
identity separately in each region, and again after every restart. In production that meant
three SDK logins within a second per restart. On 2026-10-01 two identities were blocked by the
SDK's risk control (code 900200, "账号存在异常行为，暂时无法登录") after a day of restarts and
checks. Since 1.3.3:

- The regions of one deployment share one SDK session per identity (keyed by uid and access
  key; a replaced identity file with another access key is a new session). `cache.login` runs at
  most once at a time per identity. A region that needs a session while another region is
  revalidating waits and reuses the result. If that `cache.login` failed (transport, deadline),
  the waiting request answers 503 `upstream_unavailable` instead of repeating it; the next
  request may try again.
- A shared session is reused while it is not stale and its `id_token` has not expired. JWT
  `exp`, 2 hours for Global guests, minus a 5-minute margin. A token without `exp` is reused
  until a `TOKEN_*` signal. A `TOKEN_*` signal marks the session stale only if the failed game
  session used the current `id_token`; a late signal about a token that another region has
  already replaced changes nothing.
- An SDK refusal (`SDK_REFUSED` or `SDK_CAPTCHA`) is shared. Every region disables the account without sending
  anything, and no `cache.login` of the identity is sent for `sdk_refusal_retry_seconds`
  (default 24 h). After that, the next login tries once.
- With `state_directory`, each identity's session and refusal are kept in
  `sdk-session-<hash>.json`, with a `.lock` file next to it. Files are written atomically. On
  Unix they are 0600, and the directory is created 0700 if it is missing; on Windows, protect
  the directory with ACLs. Processes sharing the directory (the service and
  `global-account verify`) hold the lock file around `cache.login` and read each other's
  results. A newer refusal written by one process is never overwritten by another. They hold the uid, the `id_token`, `mid`, an access-key
  fingerprint and the refusal, never the access key itself. A restart within the `id_token`
  lifetime logs in to the game without any SDK request. A restart under a refusal sends
  nothing. A file of another uid or access key, or a malformed one, is ignored with a warning.
- The game session (PlayerLogin credential) is not persisted. Each region still runs PlayerLogin
  after a restart, on the first authenticated request.
- Without `state_directory`, sessions and refusals live in memory only and a restart starts
  over.

Operations: after a deploy, read `GET /internal/v1/<region>/accounts`; it never logs in. Do not
probe `/internal/v1/<region>/account` in every region, because each probe can log the first
account in. To retry a refused identity early, stop the service, delete its state file and
start again.

### Bootstrap and verification

Both commands are one-shot and never retry.

1. Collect the device values from one device or emulator (see the identity file above) into a
   private file `device.json` containing only the `device` object.
2. Create a guest identity. Without `--create-sdk-guest` the command only validates the device
   file and prints the plan; nothing is sent.

   ```bash
   export SIRIUS_GLOBAL_SDK_APP_KEY=...   # the APK one_appkey
   sirius-api-proxy global-account bootstrap --device device.json \
     --identity /secure/dir/guest-1.json [--sdk-origin https://l12-sdk-login-intl.biligame.net]
   sirius-api-proxy global-account bootstrap --device device.json \
     --identity /secure/dir/guest-1.json --create-sdk-guest
   ```

   The directory must be private to its owner (0700). The command writes
   `guest-1.json.attempt.json` before the request and `guest-1.json` (mode 0600) after a
   successful `tourist.login`. If either file exists it refuses without sending anything, so an
   uncertain outcome (timeout, invalid response) is never repeated automatically. A CAPTCHA
   (200007) stops the command; complete it in the official client or give up.
3. Reference the file from a `hk`, `en` or `kr` account and verify the login once:

   ```bash
   SIRIUS_CONFIG_PATH=sirius-api-config.yaml \
     sirius-api-proxy global-account verify --account en-guest [--region en] [--show-player-id]
   ```

   `verify` performs exactly one `cache.login` and one PlayerLogin and prints
   `new_player`, `cp_server_name` and whether the optional pin matches. It prints the player ID
   only with `--show-player-id`; copy it into `players.<region>.expected_player_id` if you want
   the pin. **PlayerLogin creates the player** on a server where the identity has none.
   `--region` selects the profile of a multi-region configuration.

Stop the running service for that region before `verify`: a second login from another process
rotates the credential and can raise `CONCURRENT_DEVICE`.

### Risk and terms of service

This is not legal advice. The Global client's user terms apply to these accounts. Terms of this
kind commonly prohibit unofficial clients, automated access and bulk registration, and violations
can lead to account bans. The service reads other players' public profiles and rankings, which
is personal data under laws such as the Hong Kong PDPO, the Korean PIPA and the GDPR. Before
enabling Global accounts:

- keep access read-only and low-frequency, and do not redistribute personal data;
- use a few dedicated guest identities; do not register in bulk;
- never share an account with a person playing the game or with another proxy instance, which
  logs the other session out (`CONCURRENT_DEVICE`);
- do not bypass CAPTCHAs or login queues;
- review the terms before offering the service to others.

Live verification status: SDK `tourist.login`/`cache.login`, PlayerLogin and GetPlayerData were
verified against production on all three servers. Profile, ranking, event deck and announcement
reads on Global use the same verified protocol but have not been exercised live; see
[REGIONS.md](REGIONS.md).
