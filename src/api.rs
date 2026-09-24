use crate::{
    client::{
        GameClient, ANNOUNCEMENT, ANNOUNCEMENTS, CHALLENGE_RANKING, EVENT_DECK, EVENT_RANKING,
        MUSIC_RANKING, PLAYER_DATA, PROFILE, VERSION, WHOAMI,
    },
    error::AppError,
};
use axum::{
    extract::{Path, Query, Request, State},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

async fn authorize(
    State(token): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        != Some(&format!("Bearer {token}"))
    {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(request).await)
}
pub fn router(client: Arc<GameClient>, api_token: String, internal_token: String) -> Router {
    let public=Router::new().route("/health",get(||async {Json(json!({"status":"ok","service":"sirius-api-proxy","version":env!("CARGO_PKG_VERSION")}))}));
    let api = Router::new()
        .route("/api/v1/system", get(system))
        .route("/api/v1/master-data", get(master_status))
        .route("/api/v1/master-data/tables/{table}", get(master_table))
        .route("/api/v1/announcements", get(announcements))
        .route("/api/v1/announcements/{id}", get(announcement))
        .route("/api/v1/players/by-profile-id/{profile_id}", get(profile))
        .route("/api/v1/events/{event_id}/rankings", get(event_ranking))
        .route(
            "/api/v1/events/{event_id}/players/{player_id}/deck",
            get(event_deck),
        )
        .route("/api/v1/songs/{song_id}/rankings", get(music_ranking))
        .route(
            "/api/v1/challenge-songs/{challenge_song_id}/rankings",
            get(challenge_ranking),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::<str>::from(api_token),
            authorize,
        ));
    let internal = Router::new()
        .route("/internal/v1/protocol", get(protocol_status))
        .route("/internal/v1/protocol/reload", post(protocol_reload))
        .route("/internal/v1/resources/snapshot", get(snapshot))
        .route("/internal/v1/account", get(account))
        .route(
            "/internal/v1/master-data/updater",
            get(master_update_status),
        )
        .route("/internal/v1/account/player-data", get(player_data))
        .route_layer(middleware::from_fn_with_state(
            Arc::<str>::from(internal_token),
            authorize,
        ));
    public.merge(api).merge(internal).with_state(client)
}

async fn protocol_status(
    State(c): State<Arc<GameClient>>,
) -> Result<Json<crate::protocol::ProtocolStatus>, AppError> {
    c.protocol_status().map(Json)
}
async fn protocol_reload(
    State(c): State<Arc<GameClient>>,
) -> Result<Json<crate::protocol::ProtocolStatus>, AppError> {
    c.reload_protocol().await.map(Json)
}

async fn master_document(c: Arc<GameClient>, table: Option<String>) -> Result<Response, AppError> {
    let directory = c
        .master_directory()
        .ok_or(AppError::MasterUnavailable)?
        .to_path_buf();
    let document = tokio::task::spawn_blocking(move || {
        crate::master::read_current(&directory, table.as_deref())
    })
    .await
    .map_err(|_| AppError::MasterUnavailable)?
    .map_err(|err| match err {
        crate::master::MasterError::NotFound => AppError::NotFound,
        _ => AppError::MasterUnavailable,
    })?;
    Response::builder()
        .header("content-type", "application/json")
        .header("x-master-version", document.version)
        .body(axum::body::Body::from(document.bytes))
        .map_err(|_| AppError::MasterUnavailable)
}
async fn master_status(State(c): State<Arc<GameClient>>) -> Result<Response, AppError> {
    master_document(c, None).await
}
async fn master_table(
    State(c): State<Arc<GameClient>>,
    Path(table): Path<String>,
) -> Result<Response, AppError> {
    master_document(c, Some(table)).await
}
async fn system(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    match c.call(VERSION, json!({})).await {
        Ok(_) => Ok(Json(
            json!({"status":"available","observation":c.observation().await}),
        )),
        Err(AppError::Grpc(_)) => Ok(Json(
            json!({"status":"unavailable","observation":c.observation().await}),
        )),
        Err(e) => Err(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    tab: i32,
}
async fn announcements(
    State(c): State<Arc<GameClient>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    if !(0..=2).contains(&q.tab) {
        return Err(AppError::InvalidRequest);
    }
    c.call(ANNOUNCEMENTS, json!({"selectedTab":q.tab}))
        .await
        .map(Json)
}
async fn announcement(
    State(c): State<Arc<GameClient>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    if id <= 0 {
        return Err(AppError::InvalidRequest);
    }
    c.call(ANNOUNCEMENT, json!({"id":id.to_string()}))
        .await
        .map(Json)
}
async fn profile(
    State(c): State<Arc<GameClient>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    if id <= 0 {
        return Err(AppError::InvalidRequest);
    }
    c.call(PROFILE, json!({"playerProfileId":id.to_string()}))
        .await
        .map(Json)
}
async fn snapshot(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    c.snapshot().await.map(Json)
}

async fn account(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    c.call(WHOAMI, json!({})).await.map(Json)
}

async fn master_update_status(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    Ok(Json(c.master_update_status().await))
}

async fn player_data(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    c.call(PLAYER_DATA, json!({})).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RankingQuery {
    ranks: String,
}

fn parse_ranks(raw: &str) -> Result<Vec<i32>, AppError> {
    // Local request budget, not a claim about the upstream server's limit.
    if raw.len() > 1100 {
        return Err(AppError::InvalidRequest);
    }
    let values = raw
        .split(',')
        .map(|s| s.parse::<i32>().map_err(|_| AppError::InvalidRequest))
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty() || values.len() > 100 || values.iter().any(|v| *v <= 0) {
        return Err(AppError::InvalidRequest);
    }
    let mut unique = values.clone();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != values.len() {
        return Err(AppError::InvalidRequest);
    }
    Ok(values)
}
async fn event_ranking(
    State(c): State<Arc<GameClient>>,
    Path(id): Path<i64>,
    Query(q): Query<RankingQuery>,
) -> Result<Json<Value>, AppError> {
    if id <= 0 {
        return Err(AppError::InvalidRequest);
    }
    let ranks = parse_ranks(&q.ranks)?;
    c.call(
        EVENT_RANKING,
        json!({"eventId":id.to_string(),"ranks":ranks}),
    )
    .await
    .map(Json)
}
async fn event_deck(
    State(c): State<Arc<GameClient>>,
    Path((id, player)): Path<(i64, String)>,
) -> Result<Json<Value>, AppError> {
    if id <= 0
        || player.is_empty()
        || player.len() > 128
        || !player
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(AppError::InvalidRequest);
    }
    c.call(
        EVENT_DECK,
        json!({"eventId":id.to_string(),"playerId":player}),
    )
    .await
    .map(Json)
}
async fn public_ranking(
    c: Arc<GameClient>,
    id: i64,
    route: &str,
    key: &str,
) -> Result<Json<Value>, AppError> {
    if id <= 0 {
        return Err(AppError::InvalidRequest);
    }
    let mut response = c.call(route, json!({key:id.to_string()})).await?;
    // These fields describe the shared service account, not the queried player.
    if let Some(object) = response.as_object_mut() {
        object.remove("myRank");
        object.remove("myScore");
    }
    Ok(Json(response))
}
async fn music_ranking(
    State(c): State<Arc<GameClient>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    public_ranking(c, id, MUSIC_RANKING, "musicId").await
}
async fn challenge_ranking(
    State(c): State<Arc<GameClient>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    public_ranking(c, id, CHALLENGE_RANKING, "challengeMusicId").await
}
