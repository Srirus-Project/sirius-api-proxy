# Per-client API tokens

Optional `client_auth` restores the original multi-user authorization: each client gets its own
credential and a list of regions it may query, stored in PostgreSQL and presented as an HS256 JWT.
It is configured per region profile (single-region config, or inside each `regions.*` entry):

```yaml
client_auth:
  signing_key_env: SIRIUS_CLIENT_JWT_KEY   # >= 32 bytes, shared with the token issuer
  cache_seconds: 60                        # 0–3600; 0 disables the decision cache
  cache_entries: 10000                     # 1–100000
  database:
    host: database.example.invalid
    port: 5432
    database: sirius_clients
    username: sirius_client_reader        # SELECT on the two tables below is sufficient
    password_env: SIRIUS_CLIENT_DB_PASSWORD
    timeout_seconds: 10                   # 1–60, bounds pool acquisition and each lookup
    max_connections: 4                    # 1–64
    # root_certificate: ./db-ca.pem
    # plaintext_loopback: false           # literal loopback IPs only
```

The database transport follows [MASTER_DATABASE.md](MASTER_DATABASE.md): verified TLS by default,
password by environment reference, and ambient `PG*` variables rejected. Sirius never creates or
changes this schema; the issuing system owns it:

```sql
CREATE TABLE sirius_api_users (
  id TEXT PRIMARY KEY,
  credential TEXT NOT NULL,
  remark TEXT NOT NULL DEFAULT ''
);
CREATE TABLE sirius_api_user_regions (
  user_id TEXT NOT NULL REFERENCES sirius_api_users(id) ON DELETE CASCADE,
  region TEXT NOT NULL,            -- jp, tw, en or kr
  PRIMARY KEY (user_id, region)
);
```

## Requests

Clients send exactly one `X-Sirius-Token: <JWT>` header on public `/api/v1` routes. The token is
compact JWS with header `{"alg":"HS256"}` (optional `"typ":"JWT"`) and claims `uid` and
`credential`; other claims are ignored except `exp`, which, when present, must be in the future.
No other algorithm, including `none`, is accepted, and signatures are compared in constant time.

| Result | Status |
| --- | --- |
| Malformed, wrongly signed or expired token; unknown user; wrong credential | 401 |
| Valid user without a grant for this profile's region | 403 |
| User store unreachable or timed out | 503 |

The static `Authorization: Bearer` API token keeps working unchanged. Sending both headers, or the
user header twice, is 401. Internal routes never accept user tokens. A profile without
`client_auth` rejects `X-Sirius-Token` with 401 rather than ignoring it.

## Differences from the original

- **Fail closed.** The original served every route openly when the user database or signing key
  was absent. Sirius always requires the static bearer or a verified user token.
- **Header name.** The original's `X-Haruki-Sekai-Token` is Sekai-branded; Sirius uses
  `X-Sirius-Token`. Table names are likewise `sirius_api_*` instead of `sekai_user*`.
- **Expiry.** The original parsed but did not check `exp`; Sirius enforces it when present.
- **Decision cache.** The original cached positive decisions in Redis under keys containing the raw
  credential. Sirius caches positive decisions in process only, keyed by user and a SHA-256 digest
  of the credential. Each node caches independently, so revocation takes effect after at most
  `cache_seconds` per node (set 0 for immediate effect). Failures are never cached. A full cache
  skips caching instead of evicting live entries.

The signing key and database password must differ from each other and from the profile's API,
internal, peer, account, CDN, updater and other outgoing credentials. Restart to rotate them.
