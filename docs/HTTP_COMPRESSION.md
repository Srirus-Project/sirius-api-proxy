# Response compression

Sirius can encode successful public JSON responses with gzip or zstd when the client asks for it
with `Accept-Encoding`. It is off unless configured; an absent block or `enabled: false` leaves
responses byte-identical to 1.2.x.

```yaml
http_compression:
  enabled: true
```

Configure the block at the service root, alongside `listen`, `tls` and `access_log`:

- single-region file: the file root;
- multi-region file: the deployment root only; a region entry that sets it is rejected;
- standalone registry (`registry-serve`): the registry file root.

`enabled` is required inside the block and unknown keys are rejected. There is no level or
algorithm setting.

## Scope

Wrapped routes:

- the public API, `/api/v1/...` and `/api/v1/<region>/...`, including the Master registry and
  database read routes;
- the standalone registry's public `.../master-data/...` routes.

Never wrapped (always identity, no `Vary`):

- `/health`;
- `/internal/v1/...`, including accounts, identity, player data and the registry owner routes;
- peer routes (`/internal/v1[/<region>]/peer/query`), so 1.2.x callers and executors keep the
  identity wire format;
- asset dispatch admin routes.

## Negotiation

- Codings: gzip and zstd only, at the fastest level. With equal quality values zstd is
  preferred. `br`, `deflate`, `*`, `identity` and `q=0` all give identity.
- Eligible: status 200, `Content-Type: application/json`, at least 1024 bytes. Error bodies
  (401, 4xx, 5xx) and 304 are never encoded, so unauthenticated requests never cost encoder
  work. Master bundles (`application/x-tar`) are never encoded and keep their exact
  `Content-Length`.
- An encoded response has `Content-Encoding`, no `Content-Length` and no `Accept-Ranges`. Its
  `ETag` is sent weak (`W/"<hash>"`), because a strong validator must not be shared between
  content-codings. The hash inside is unchanged, and `If-None-Match` accepts the weak form, so
  revalidation still answers 304. The empty 304 keeps the strong tag.
- Every JSON 200 or 304 on a wrapped route carries `Vary: Accept-Encoding`, whether encoded or
  not.
- `x-master-version`, `Cache-Control` and the decoded bytes are unchanged.

Request bodies are never decoded: a request with `Content-Encoding` is read as-is, so a
compressed body fails JSON parsing with 400 and cannot be inflated.

## Clients

Many HTTP libraries (browsers, python-requests, `curl --compressed`) send
`Accept-Encoding: gzip` by default. Once enabled, those clients receive `Content-Encoding`, no
`Content-Length`, and a `W/`-prefixed ETag. A client that compares the ETag literally with
`"<sha256>"` or relies on `Content-Length` for progress must strip `W/` or stop sending
`Accept-Encoding`.

Sirius's own consumers (`master_sync`, the registry owner, peer transport and update
notifications) send no `Accept-Encoding` and always receive identity bytes.

## Outbound requests are unchanged

Only server-side encoders are compiled in. The HTTP client features are unchanged, so outbound
Global SDK, CDN, peer, asset dispatch and Master sync requests still carry no
`Accept-Encoding` header; tests assert this.

## When to enable

Behind Cloudflare or another proxy that already compresses, leave it off. Enable it for direct
or non-CDN deployments where transfer size matters. Encoding runs on the runtime workers; the
fastest level and the authenticated-200-only rule bound the cost, but large table reads under
load still use CPU.
