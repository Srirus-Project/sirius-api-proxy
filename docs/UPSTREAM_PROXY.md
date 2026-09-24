# Regional outbound proxy transport

Configure each region's `upstream` section independently:

```yaml
upstream:
  connect_timeout_ms: 10000
  proxy_url_env: SIRIUS_JP_PROXY_URL
  proxy_authorization_env: SIRIUS_JP_PROXY_AUTH # optional
```

The URL environment variable must contain an HTTP or HTTPS origin such as
`http://127.0.0.1:8080` or `https://proxy.example:8443`. Do not put userinfo,
paths, queries, fragments or credentials in that URL. URLs and authorization
values are limited to 4,096 bytes. The optional authorization variable contains
the entire Proxy-Authorization value (for example `Basic <base64-user:password>`).
It requires a proxy URL. Missing/invalid references fail client construction.
Values are captured at startup; restart to change the transport.

Omitting `proxy_url_env` uses a direct connection. The gRPC transport never reads
HTTP_PROXY, HTTPS_PROXY or NO_PROXY implicitly. Explicit proxy failure never falls
back to a direct connection. SOCKS and PAC configurations are rejected. Haruki's
outbound HTTP proxy concept is retained; Sirius's binary gRPC framing and HTTP/2
trailers remain unchanged.

The connector sends HTTP/1.1 CONNECT to the proxy, with the target hostname and
port. Target DNS resolution belongs to the proxy; bracketed IPv6 authorities and
explicit ports are preserved. After a successful tunnel, the client negotiates
TLS and HTTP/2 with the game server. HTTPS proxies additionally use verified TLS
to the proxy itself; HTTP proxies receive the CONNECT headers without TLS. Select
a proxy endpoint appropriate for the deployment's trust boundary.

Proxy authorization appears only on CONNECT. Game credentials remain inside the
origin TLS connection and are never added to CONNECT. API/internal bearer tokens
are not proxy credentials. Both TLS layers use the existing WebPKI trust roots and
hostname validation; there is no insecure verification switch. Pools belong to
individual region clients and cannot reuse another region's proxy connection.

`connect_timeout_ms` defaults to 10,000 and accepts 100–300,000 milliseconds. It
bounds DNS/TCP, proxy TLS/CONNECT and origin TLS together, including connection
pool establishment. The logical `timeout_ms` remains the overall call deadline.
A connection timeout is a transport failure (502); an expired logical deadline is
504. Anonymous transport retries still share the original logical deadline.

CONNECT responses are limited to 16 KiB and 128 headers per response. At most four
informational responses precede the final result; 101 is rejected. Successful 2xx
responses start the tunnel immediately after the header terminator. Following
[RFC 9110 CONNECT semantics](https://www.rfc-editor.org/rfc/rfc9110.html#name-connect),
Content-Length does not consume any tunnel bytes. Redirects, authentication failures
and malformed/refused CONNECT responses produce a sanitized proxy error (502),
are not retried, and do not penalize individual account health. Proxy response bodies
and credentials are never echoed in errors.

This setting applies to game gRPC only. Master CDN download transport, other HTTP
clients, inbound TLS and trusted forwarding have separate configuration/restoration
work; this setting does not silently reroute them. Local tests exercise HTTP/2 RPCs
and trailers over CONNECT, header separation, connection reuse, direct/proxy client
isolation, refusal/no-fallback, deadlines, remote DNS/IPv6 and exact tunnel bytes.
Self-signed fixtures verify rejection at both proxy and origin TLS layers.
