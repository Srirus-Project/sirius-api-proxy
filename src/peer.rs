//! Versioned, local-only peer reads. No generic RPC, credentials or forwarding hops.
use crate::{
    client::GameClient,
    error::AppError,
    region::{Platform, Region},
    routes,
};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    middleware,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub contract_version: u32,
    pub region: Region,
    pub environment: String,
    pub platform: Platform,
    pub client_version: String,
    pub protocol_sha256: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub request_id: String,
    pub identity: Identity,
    pub operation: Operation,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Version {},
    Servers {},
    Announcements { tab: i32 },
    Announcement { id: i64 },
    Profile { profile_id: i64 },
    EventRanking { event_id: i64, ranks: Vec<i32> },
    EventDeck { event_id: i64, player_id: String },
    MusicRanking { music_id: i64 },
    ChallengeRanking { challenge_music_id: i64 },
}
impl Operation {
    pub fn rpc(&self) -> Result<(&'static str, Value), AppError> {
        let id = |v: i64| {
            if v > 0 {
                Ok(v.to_string())
            } else {
                Err(AppError::InvalidRequest)
            }
        };
        Ok(match self {
            Self::Version {} => (routes::VERSION, json!({})),
            Self::Servers {} => (routes::SERVER_LIST, json!({})),
            Self::Announcements { tab } if (0..=2).contains(tab) => {
                (routes::ANNOUNCEMENTS, json!({"selectedTab":tab}))
            }
            Self::Announcements { .. } => return Err(AppError::InvalidRequest),
            Self::Announcement { id: value } => (routes::ANNOUNCEMENT, json!({"id":id(*value)?})),
            Self::Profile { profile_id } => {
                (routes::PROFILE, json!({"playerProfileId":id(*profile_id)?}))
            }
            Self::EventRanking { event_id, ranks } => {
                let unique = ranks.iter().collect::<std::collections::BTreeSet<_>>();
                if ranks.is_empty()
                    || ranks.len() > 100
                    || unique.len() != ranks.len()
                    || ranks.iter().any(|v| *v <= 0)
                {
                    return Err(AppError::InvalidRequest);
                }
                (
                    routes::EVENT_RANKING,
                    json!({"eventId":id(*event_id)?,"ranks":ranks}),
                )
            }
            Self::EventDeck {
                event_id,
                player_id,
            } => {
                if player_id.is_empty()
                    || player_id.len() > 128
                    || !player_id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(AppError::InvalidRequest);
                }
                (
                    routes::EVENT_DECK,
                    json!({"eventId":id(*event_id)?,"playerId":player_id}),
                )
            }
            Self::MusicRanking { music_id } => {
                (routes::MUSIC_RANKING, json!({"musicId":id(*music_id)?}))
            }
            Self::ChallengeRanking { challenge_music_id } => (
                routes::CHALLENGE_RANKING,
                json!({"challengeMusicId":id(*challenge_music_id)?}),
            ),
        })
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub request_id: String,
    pub identity: Identity,
    pub outcome: Outcome,
    pub observation: crate::client::Observation,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Success { data: Value },
    Failure { kind: Failure },
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Failure {
    IdentityMismatch {},
    UnsupportedOperation {},
    AccountUnavailable {},
    UnavailableBeforeDispatch {},
    Timeout {},
    Transport {},
    Protocol {},
    Game { grpc_status: u16 },
}
impl From<AppError> for Failure {
    fn from(error: AppError) -> Self {
        match error {
            AppError::UnsupportedRegionOperation => Self::UnsupportedOperation {},
            AppError::PeerAccountUnavailable => Self::UnavailableBeforeDispatch {},
            AppError::AccountUnavailable => Self::AccountUnavailable {},
            AppError::Timeout => Self::Timeout {},
            AppError::Transport | AppError::Proxy => Self::Transport {},
            AppError::Grpc(grpc_status) => Self::Game { grpc_status },
            AppError::PeerIdentityMismatch => Self::IdentityMismatch {},
            _ => Self::Protocol {},
        }
    }
}

pub fn router(client: Arc<GameClient>, prefix: &str, token: String) -> Router {
    Router::new()
        .nest(
            prefix,
            Router::new()
                .route("/query", post(query))
                .layer(DefaultBodyLimit::max(16 * 1024))
                .route_layer(middleware::from_fn_with_state(
                    Arc::<str>::from(token),
                    crate::api::authorize,
                )),
        )
        .with_state(client)
}
async fn query(
    State(client): State<Arc<GameClient>>,
    Json(request): Json<Request>,
) -> Result<(StatusCode, Json<Reply>), AppError> {
    if uuid::Uuid::parse_str(&request.request_id).is_err() || request.request_id.len() != 36 {
        return Err(AppError::InvalidRequest);
    }
    let (route, input) = request.operation.rpc()?;
    let identity = client.peer_identity()?;
    let outcome = if identity != request.identity {
        Outcome::Failure {
            kind: Failure::IdentityMismatch {},
        }
    } else {
        match client
            .call_peer(route, input, &request.identity.protocol_sha256)
            .await
        {
            Ok(mut data) => {
                if matches!(route, routes::MUSIC_RANKING | routes::CHALLENGE_RANKING) {
                    if let Some(object) = data.as_object_mut() {
                        object.remove("myRank");
                        object.remove("myScore");
                    }
                }
                Outcome::Success { data }
            }
            Err(error) => Outcome::Failure { kind: error.into() },
        }
    };
    // Echo requested identity to bind replies; failure does not claim acceptance.
    Ok((
        StatusCode::OK,
        Json(Reply {
            request_id: request.request_id,
            identity: request.identity,
            outcome,
            observation: client.observation().await,
        }),
    ))
}
