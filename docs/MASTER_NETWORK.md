# Master CDN request policy

Master downloads have an independent HTTP client; `upstream` controls game gRPC traffic only.
The optional `master_update.network` section applies to both the manifest and encrypted tables:

```yaml
master_update:
  username_env: SIRIUS_CDN_USERNAME
  key_hex_env: SIRIUS_MASTER_KEY_HEX
  iv_hex_env: SIRIUS_MASTER_IV_HEX
  interval_seconds: 300
  network:
    connect_timeout_ms: 10000
    request_timeout_ms: 60000
    update_timeout_seconds: 600
    attempts: 1
    retry_delay_ms: 250
    max_retry_delay_ms: 5000
    # proxy_url_env: SIRIUS_MASTER_PROXY_URL
    # proxy_authorization_env: SIRIUS_MASTER_PROXY_AUTHORIZATION
```

`cdn_authorization` (default `basic`) is independent of the network policy. `basic` requires
`username_env` and a credential for the CDN root. `none` sends no Authorization header and is
limited to TW/EN/KR (see [region support](REGIONS.md#cdn-authorization)).

The defaults retain one request attempt and the original connection/request/update timeouts.
Connection and request timeouts allow 100–300000 ms. Request timeout includes the response body.
The whole-update deadline allows 1–3600 seconds and includes waiting for another update's lock,
version checks, all downloads and retry backoffs. A queued call that expires before acquiring the
lock does not overwrite the active update's status. Existing cancellation/publication rules remain:
there is no await between final version/credential comparison and switching CURRENT.

`attempts` is 1–8, including the first attempt. Only transport failures, HTTP 429 and HTTP 5xx
retry. Backoff doubles from `retry_delay_ms` (1–10000), capped at `max_retry_delay_ms`
(at least the initial delay and at most 30000). No jitter or Retry-After interpretation is added.
Each retry starts the file from byte zero, retaining existing size bounds. Authentication failures,
redirects, manifest/decryption/hash/JSON validation failures and changed version/CDN credentials
remain terminal. Failed updates preserve the installed snapshot. The policy never retries game
registration/login/account mutations; game RPCs retain their own request policy.

Proxy configuration uses explicit environment references. The URL must be an HTTP or HTTPS
origin without embedded credentials, path, query or fragment. The authorization variable holds
the full Proxy-Authorization value; it is optional and requires the URL reference. Bad/missing
values fail construction before network calls with a fixed, non-sensitive error.

Omission means direct connections. Ambient HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY and
OS proxy discovery are disabled; deployments previously relying on those variables must explicitly
reference the desired URL variable. A configured proxy has no automatic direct fallback. Standard
proxy and origin TLS verification and the no-redirect policy remain enabled. HTTPS origins use
CONNECT; HTTP forward requests expose their origin headers to the proxy. The production CDN
policy still requires approved HTTPS roots and, with `basic`, scoped Basic credentials, separate
from game session and API/internal bearer credentials. An HTTP 407 response is terminal; a rejected CONNECT
may appear as a transport failure and retry within the configured bound.

This configuration also applies to the one-shot `master-update` command and each region's own
background updater. It does not configure asset-updater downloads or S3 publication.
