use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, Clone, thiserror::Error)]
pub enum AppError {
    #[error("no node is currently available")]
    NodeUnavailable,
    #[error("local account unavailable before dispatch")]
    PeerAccountUnavailable,
    #[error("peer protocol identity changed before dispatch")]
    PeerIdentityMismatch,
    #[error("operation is not supported by the verified protocol for this region")]
    UnsupportedRegionOperation,
    #[error("proto bundle compilation or compatibility validation failed")]
    ProtocolDefinition,
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    #[error("invalid request")]
    InvalidRequest,
    #[error("authentication required")]
    Unauthorized,
    #[error("not authorized for this region")]
    Forbidden,
    #[error("client authorization is temporarily unavailable")]
    AuthUnavailable,
    #[error("not found")]
    NotFound,
    #[error("no game account is currently available")]
    AccountUnavailable,
    /// The region's game path (or Global SDK path) is failing; refused before upstream contact.
    #[error("game upstream is temporarily unreachable")]
    UpstreamUnavailable,
    #[error("upstream request timed out")]
    Timeout,
    #[error("upstream transport failed")]
    Transport,
    #[error("upstream proxy rejected connection")]
    Proxy,
    #[error("invalid upstream protocol response")]
    Protocol,
    #[error("game returned gRPC status {0}")]
    Grpc(u16),
    /// A failed call whose response carried `UNDER_MAINTENANCE`; holds its gRPC status.
    #[error("game is under maintenance")]
    Maintenance(u16),
    #[error("resource snapshot is unavailable")]
    SnapshotUnavailable,
    #[error("Master snapshot is unavailable or invalid")]
    MasterUnavailable,
}
impl AppError {
    /// Stable machine-readable identifier; the message text may change between releases.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NodeUnavailable => "node_unavailable",
            Self::PeerAccountUnavailable => "peer_account_unavailable",
            Self::PeerIdentityMismatch => "peer_identity_mismatch",
            Self::UnsupportedRegionOperation => "unsupported_operation",
            Self::ProtocolDefinition => "protocol_definition_invalid",
            Self::Config(_) => "invalid_configuration",
            Self::InvalidRequest => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::AuthUnavailable => "auth_unavailable",
            Self::NotFound => "not_found",
            Self::AccountUnavailable => "account_unavailable",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::Timeout => "upstream_timeout",
            Self::Transport => "upstream_transport",
            Self::Proxy => "upstream_proxy",
            Self::Protocol => "upstream_protocol",
            Self::Grpc(_) => "upstream_grpc",
            Self::Maintenance(_) => "maintenance",
            Self::SnapshotUnavailable => "snapshot_unavailable",
            Self::MasterUnavailable => "master_unavailable",
        }
    }
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::PeerIdentityMismatch => StatusCode::CONFLICT,
            Self::UnsupportedRegionOperation => StatusCode::NOT_IMPLEMENTED,
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::ProtocolDefinition => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::NodeUnavailable
            | Self::PeerAccountUnavailable
            | Self::AccountUnavailable
            | Self::UpstreamUnavailable
            | Self::SnapshotUnavailable
            | Self::MasterUnavailable
            | Self::AuthUnavailable
            | Self::Maintenance(_)
            | Self::Grpc(14) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        };
        // Never echo raw upstream messages: they can contain account/CDN secrets.
        let mut body = json!({"error": self.to_string(), "code": self.code()});
        if let Self::Grpc(grpc_status) | Self::Maintenance(grpc_status) = self {
            body["grpc_status"] = grpc_status.into();
        }
        (status, Json(body)).into_response()
    }
}
/// Gives framework-generated client errors (extractor rejections, unknown routes, wrong
/// methods, body limits) the same `{error, code}` JSON as handler errors. Status and headers
/// such as `Allow` are kept; the plain-text detail is dropped because it can echo input.
pub fn json_client_errors(router: axum::Router) -> axum::Router {
    router.layer(axum::middleware::map_response(json_client_error))
}
async fn json_client_error(response: Response) -> Response {
    use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
    let (error, code) = match response.status() {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            ("invalid request", "invalid_request")
        }
        StatusCode::NOT_FOUND => ("not found", "not_found"),
        StatusCode::METHOD_NOT_ALLOWED => ("method not allowed", "method_not_allowed"),
        StatusCode::PAYLOAD_TOO_LARGE => ("request body too large", "payload_too_large"),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ("unsupported media type", "unsupported_media_type"),
        _ => return response,
    };
    let json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if json {
        return response;
    }
    let (mut parts, _) = response.into_parts();
    parts.headers.remove(CONTENT_TYPE);
    parts.headers.remove(CONTENT_LENGTH);
    // The replacement body is plain JSON, never the encoded original.
    parts.headers.remove(CONTENT_ENCODING);
    let body = Json(json!({"error": error, "code": code})).into_response();
    let (body_parts, body) = body.into_parts();
    parts.headers.extend(body_parts.headers);
    Response::from_parts(parts, body)
}
