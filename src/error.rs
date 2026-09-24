use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("proto bundle compilation or compatibility validation failed")]
    ProtocolDefinition,
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    #[error("invalid request")]
    InvalidRequest,
    #[error("authentication required")]
    Unauthorized,
    #[error("environment not found")]
    NotFound,
    #[error("game account is not configured")]
    AccountUnavailable,
    #[error("upstream request timed out")]
    Timeout,
    #[error("upstream transport failed")]
    Transport,
    #[error("invalid upstream protocol response")]
    Protocol,
    #[error("game returned gRPC status {0}")]
    Grpc(u16),
    #[error("resource snapshot is unavailable")]
    SnapshotUnavailable,
    #[error("Master snapshot is unavailable or invalid")]
    MasterUnavailable,
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::ProtocolDefinition => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::AccountUnavailable
            | Self::SnapshotUnavailable
            | Self::MasterUnavailable
            | Self::Grpc(14) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        };
        // Never echo raw upstream messages: they can contain account/CDN secrets.
        (status, Json(json!({"error": self.to_string()}))).into_response()
    }
}
