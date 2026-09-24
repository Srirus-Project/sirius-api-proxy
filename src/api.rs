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
    health_router().merge(router_at(
        client,
        api_token,
        internal_token,
        "/api/v1",
        "/internal/v1",
    ))
}

pub fn health_router() -> Router {
    Router::new().route("/health",get(||async {Json(json!({"status":"ok","service":"sirius-api-proxy","version":env!("CARGO_PKG_VERSION")}))}))
}

pub fn router_at(
    client: Arc<GameClient>,
    api_token: String,
    internal_token: String,
    api_prefix: &str,
    internal_prefix: &str,
) -> Router {
    let api = Router::new()
        .route("/system", get(system))
        .route("/servers", get(servers))
        .route("/regions", get(regions))
        .route("/master-data", get(master_status))
        .route("/master-data/tables/{table}", get(master_table))
        .route("/announcements", get(announcements))
        .route("/announcements/{id}", get(announcement))
        .route("/players/by-profile-id/{profile_id}", get(profile))
        .route("/events/{event_id}/rankings", get(event_ranking))
        .route(
            "/events/{event_id}/players/{player_id}/deck",
            get(event_deck),
        )
        .route("/songs/{song_id}/rankings", get(music_ranking))
        .route(
            "/challenge-songs/{challenge_song_id}/rankings",
            get(challenge_ranking),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::<str>::from(api_token),
            authorize,
        ));
    let internal = Router::new()
        .route("/protocol", get(protocol_status))
        .route("/protocol/reload", post(protocol_reload))
        .route("/resources/snapshot", get(snapshot))
        .route("/account", get(account))
        .route("/master-data/updater", get(master_update_status))
        .route("/account/player-data", get(player_data))
        .route_layer(middleware::from_fn_with_state(
            Arc::<str>::from(internal_token),
            authorize,
        ));
    Router::new()
        .nest(api_prefix, api)
        .nest(internal_prefix, internal)
        .with_state(client)
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
async fn regions(State(c): State<Arc<GameClient>>) -> Json<Value> {
    use crate::region::Region;
    let regions=[Region::Jp,Region::Tw,Region::En,Region::Kr,Region::Cn].map(|region| json!({
        "region":region,"area_id":region.area_id(),"protocol_family":region.family(),"reserved":region==Region::Cn,
        "capability":if region==Region::Cn {"reserved"} else if region==Region::Jp {"jp_proxy"} else {"discovery_and_version"}
    }));
    Json(json!({"selected":c.region(),"regions":regions}))
}
async fn servers(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    c.call(crate::routes::SERVER_LIST, json!({}))
        .await
        .map(Json)
}
async fn system(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    match c.call(VERSION, json!({})).await {
        Ok(_) => Ok(Json(
            json!({"status":"available","region":c.region(),"area_id":c.region().area_id(),"platform":c.platform(),"protocol_family":c.region().family(),"supported_rpcs":c.supported_routes(),"observation":c.observation().await}),
        )),
        Err(AppError::Grpc(_)) => Ok(Json(
            json!({"status":"unavailable","region":c.region(),"platform":c.platform(),"observation":c.observation().await}),
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
