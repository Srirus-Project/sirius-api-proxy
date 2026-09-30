//! Opt-in negotiated gzip/zstd encoding of successful public JSON responses.
//!
//! Only the public API and the registry's public Master routes are wrapped; `/health`,
//! internal, peer and asset-dispatch admin routes always answer identity. Request bodies are
//! never decoded. This is a server-side encoder only: outbound reqwest clients keep their
//! features, so SDK, CDN and peer requests still carry no `Accept-Encoding`.
use axum::{
    http::{
        header::{CONTENT_ENCODING, CONTENT_TYPE, ETAG, VARY},
        HeaderMap, HeaderValue, StatusCode,
    },
    response::Response,
    Router,
};
use serde::Deserialize;
use tower_http::compression::{
    predicate::{Predicate, SizeAbove},
    CompressionLayer, CompressionLevel,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Required inside the block; an absent block leaves responses untouched.
    pub enabled: bool,
}

/// Smaller bodies gain little and still cost a codec setup.
const MIN_BYTES: u16 = 1024;

/// Wraps `router` when compression is enabled; otherwise returns it unchanged, so a disabled
/// deployment adds no layer and no header.
pub(crate) fn wrap<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    config: Option<&Config>,
) -> Router<S> {
    if !config.is_some_and(|c| c.enabled) {
        return router;
    }
    // The fastest level bounds per-byte CPU on runtime workers; JSON still shrinks several-fold.
    let compression = CompressionLayer::new()
        .gzip(true)
        .zstd(true)
        .quality(CompressionLevel::Fastest)
        .compress_when(SizeAbove::new(MIN_BYTES).and(eligible));
    // Added last, so it is outermost and sees the encoded response.
    router
        .layer(compression)
        .layer(axum::middleware::map_response(representation))
}

fn json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
}

/// Only authenticated 200 JSON bodies: error bodies, 304s and bundles (`application/x-tar`,
/// exact Content-Length) stay identity, and unauthenticated requests never cost encoder work.
fn eligible(
    status: StatusCode,
    _: axum::http::Version,
    headers: &HeaderMap,
    _: &axum::http::Extensions,
) -> bool {
    status == StatusCode::OK && json(headers)
}

/// An encoded body gets a weak ETag: a strong validator must not be shared across
/// content-codings. The If-None-Match comparisons already strip `W/`, so revalidation still
/// answers 304, whose empty body keeps the strong tag. Every negotiable status (200/304 JSON)
/// carries `Vary: Accept-Encoding`, including identity bodies below the size threshold.
async fn representation(mut response: Response) -> Response {
    let headers = response.headers_mut();
    if headers.contains_key(CONTENT_ENCODING) {
        let weak = headers
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.starts_with("W/"))
            .and_then(|v| HeaderValue::from_str(&format!("W/{v}")).ok());
        if let Some(weak) = weak {
            headers.insert(ETAG, weak);
        }
    }
    let status = response.status();
    let headers = response.headers_mut();
    if (status == StatusCode::OK || status == StatusCode::NOT_MODIFIED)
        && json(headers)
        && !headers.get_all(VARY).iter().any(|v| {
            v.to_str()
                .is_ok_and(|v| v.to_ascii_lowercase().contains("accept-encoding"))
        })
    {
        headers.append(VARY, HeaderValue::from_static("accept-encoding"));
    }
    response
}
