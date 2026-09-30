use crate::{
    client::{GameClient, PLAYER_DATA, WHOAMI},
    error::AppError,
    peer::Operation,
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
use std::{sync::Arc, time::Instant};

pub(crate) async fn authorize(
    State(token): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if request.headers().get_all("authorization").iter().count() != 1
        || request
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            != Some(&format!("Bearer {token}"))
    {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(request).await)
}
/// Public API routes accept the static bearer or, when `client_auth` is configured, exactly
/// one per-client token. Presenting both, duplicates, or a user token on a profile without
/// client authorization is rejected. Internal routes never accept user tokens.
async fn authorize_api(
    State((token, client)): State<(Arc<str>, Arc<GameClient>)>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let headers = request.headers();
    let users = headers.get_all(crate::client_auth::HEADER).iter().count();
    if users == 0 {
        return authorize(State(token), request, next).await;
    }
    let auth = client.client_auth().ok_or(AppError::Unauthorized)?;
    if users != 1 || headers.contains_key("authorization") {
        return Err(AppError::Unauthorized);
    }
    let value = headers
        .get(crate::client_auth::HEADER)
        .and_then(|h| h.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    auth.authorize(value).await?;
    Ok(next.run(request).await)
}
pub fn router(client: Arc<GameClient>, api_token: String, internal_token: String) -> Router {
    health_router().merge(router_at(
        client,
        api_token,
        internal_token,
        "/api/v1",
        "/internal/v1",
        None,
    ))
}

static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
fn started() -> Instant {
    *STARTED.get_or_init(Instant::now)
}
/// Records the process start reported as `/health` `uptime_secs`; `main()` calls this first.
/// Later calls keep the first instant. Without `main()` (library or test use) the clock starts
/// at the first `health_router()` build or `health_body()` call.
pub fn mark_started() {
    started();
}
#[cfg(test)]
pub(crate) fn started_at() -> Instant {
    started()
}
/// Whole seconds from `start` to `now`, truncated; 0 if `now` is earlier.
pub(crate) fn uptime_secs_between(start: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(start).as_secs()
}
/// The `/health` body shared by the API, multi-region and registry servers: liveness only.
pub(crate) fn health_body(service: &'static str) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": service,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": uptime_secs_between(started(), Instant::now()),
    }))
}
pub fn health_router() -> Router {
    started();
    Router::new().route("/health", get(|| async { health_body("sirius-api-proxy") }))
}

pub fn router_at(
    client: Arc<GameClient>,
    api_token: String,
    internal_token: String,
    api_prefix: &str,
    internal_prefix: &str,
    compression: Option<&crate::http_compression::Config>,
) -> Router {
    let api = Router::new()
        .route("/system", get(system))
        .route("/servers", get(servers))
        .route("/regions", get(regions))
        .route("/master-data", get(master_status))
        .route("/master-data/manifest", get(registry_current))
        .route("/master-data/bundle", get(registry_bundle_current))
        .route(
            "/master-data/by-hash/{hash}/bundle",
            get(registry_bundle_hash),
        )
        .route("/master-data/database/manifest", get(database_current))
        .route(
            "/master-data/database/by-hash/{hash}/manifest",
            get(database_manifest),
        )
        .route(
            "/master-data/database/by-hash/{hash}/tables/{table}",
            get(database_table),
        )
        .route("/master-data/database/history", get(database_history))
        .route(
            "/master-data/by-hash/{hash}/manifest",
            get(registry_by_hash),
        )
        .route("/master-data/history", get(registry_history))
        .route(
            "/master-data/snapshots/{snapshot}/manifest",
            get(registry_manifest),
        )
        .route(
            "/master-data/snapshots/{snapshot}/tables/{table}/{hash}",
            get(registry_table),
        )
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
            (Arc::<str>::from(api_token), client.clone()),
            authorize_api,
        ));
    // Public reads only; internal account, identity and player-data output stays identity.
    let api = crate::http_compression::wrap(api, compression);
    let internal = Router::new()
        .route("/nodes", get(nodes))
        .route("/protocol", get(protocol_status))
        .route("/protocol/reload", post(protocol_reload))
        .route("/resources/snapshot", get(snapshot))
        .route("/account", get(account))
        .route("/accounts", get(accounts))
        .route("/accounts/reload", post(reload_accounts))
        .route("/accounts/{name}/identity", get(named_account))
        .route("/accounts/{name}/player-data", get(named_player_data))
        .route("/master-data/updater", get(master_update_status))
        .route("/master-data/database", get(master_database_status))
        .route("/master-data/git", get(master_git_status))
        .route(
            "/master-data/sync",
            post(master_sync_hint).layer(axum::extract::DefaultBodyLimit::max(4096)),
        )
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

async fn master_sync_hint(
    State(c): State<Arc<GameClient>>,
    Json(hint): Json<crate::master_sync::UpdateHint>,
) -> Result<(axum::http::StatusCode, Json<Value>), AppError> {
    c.request_master_sync(&hint)?;
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"status":"accepted"})),
    ))
}

async fn nodes(State(c): State<Arc<GameClient>>) -> Json<Value> {
    Json(c.node_status())
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

/// CURRENT-relative reads revalidate with a content ETag. Integrity, region and table
/// checks all run before the conditional match, so corruption answers 503, never 304.
/// Table reads (not the status document) pass the table read admission first.
async fn master_document(
    c: Arc<GameClient>,
    table: Option<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let directory = c
        .master_directory()
        .ok_or(AppError::MasterUnavailable)?
        .to_path_buf();
    let region = c.region();
    let admission = match table {
        Some(_) => Some(c.table_reads().admit().await?),
        None => None,
    };
    let held = admission.clone();
    let document = tokio::task::spawn_blocking(move || {
        let _held = held;
        crate::master::read_current_in(&directory, table.as_deref(), region)
    })
    .await
    .map_err(|_| AppError::MasterUnavailable)?
    .map_err(|err| match err {
        crate::master::MasterError::NotFound => AppError::NotFound,
        _ => AppError::MasterUnavailable,
    })?;
    registry_document(
        crate::master_registry::Document {
            etag: format!("\"{}\"", document.sha256),
            version: document.version,
            bytes: document.bytes,
        },
        headers,
        false,
    )
    .map(|response| crate::master_admission::attach(admission, response))
}
async fn master_status(
    State(c): State<Arc<GameClient>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    master_document(c, None, headers).await
}
async fn master_table(
    State(c): State<Arc<GameClient>>,
    Path(table): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    master_document(c, Some(table), headers).await
}
/// Per-operation status: `live_verified` was exercised against the production service;
/// `implemented_unverified` uses the verified protocol but was not exercised live.
fn operations(region: crate::region::Region) -> Value {
    use crate::region::Region;
    const OPERATIONS: [&str; 10] = [
        "version",
        "announcements",
        "profile",
        "event_ranking",
        "event_deck",
        "music_ranking",
        "challenge_ranking",
        "account_login",
        "account_identity",
        "player_data",
    ];
    let status = |operation: &str| match region {
        Region::Cn => "reserved",
        Region::Jp if operation == "account_login" => "static_credentials",
        Region::Jp => "live_verified",
        _ if matches!(
            operation,
            "version"
                | "account_login"
                | "player_data"
                | "announcements"
                | "profile"
                | "music_ranking"
        ) =>
        {
            "live_verified"
        }
        _ => "implemented_unverified",
    };
    let mut map = serde_json::Map::new();
    for operation in OPERATIONS {
        map.insert(operation.into(), json!(status(operation)));
    }
    map.insert(
        "servers".into(),
        json!(match region {
            Region::Cn => "reserved",
            Region::Jp => "unsupported",
            _ => "live_verified",
        }),
    );
    Value::Object(map)
}
async fn regions(State(c): State<Arc<GameClient>>) -> Json<Value> {
    use crate::region::Region;
    let regions=[Region::Jp,Region::Hk,Region::En,Region::Kr,Region::Cn].map(|region| json!({
        "region":region,"area_id":region.area_id(),"protocol_family":region.family(),"reserved":region==Region::Cn,
        "master_data":region.master_supported(),
        "capability":if region==Region::Cn {"reserved"} else if region==Region::Jp {"jp_proxy"} else {"global_proxy"},
        "operations":operations(region)
    }));
    Json(json!({"selected":c.region(),"regions":regions}))
}
async fn servers(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    c.public_call(Operation::Servers {}).await.map(Json)
}
async fn system(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    let execution = c.public_query(Operation::Version {}).await;
    match execution.result {
        Ok(_) => Ok(Json(
            json!({"status":"available","region":c.region(),"area_id":c.region().area_id(),"platform":c.platform(),"protocol_family":c.region().family(),"supported_rpcs":c.supported_routes(),"observation":execution.observation}),
        )),
        Err(AppError::Grpc(_) | AppError::Maintenance(_)) => Ok(Json(
            json!({"status":"unavailable","region":c.region(),"platform":c.platform(),"observation":execution.observation}),
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
    c.public_call(Operation::Announcements { tab: q.tab })
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
    c.public_call(Operation::Announcement { id })
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
    c.public_call(Operation::Profile { profile_id: id })
        .await
        .map(Json)
}
async fn snapshot(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    c.snapshot().await.map(Json)
}

async fn accounts(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    c.account_status().map(Json)
}
async fn reload_accounts(State(c): State<Arc<GameClient>>) -> Result<Json<Value>, AppError> {
    c.reload_accounts().await.map(Json)
}
async fn named_account(
    State(c): State<Arc<GameClient>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    c.call_account(&name, WHOAMI).await.map(Json)
}
async fn named_player_data(
    State(c): State<Arc<GameClient>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    c.call_account(&name, PLAYER_DATA).await.map(Json)
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
    c.public_call(Operation::EventRanking {
        event_id: id,
        ranks,
    })
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
    c.public_call(Operation::EventDeck {
        event_id: id,
        player_id: player,
    })
    .await
    .map(Json)
}
async fn public_ranking(
    c: Arc<GameClient>,
    id: i64,
    operation: Operation,
) -> Result<Json<Value>, AppError> {
    if id <= 0 {
        return Err(AppError::InvalidRequest);
    }
    let mut response = c.public_call(operation).await?;
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
    public_ranking(c, id, Operation::MusicRanking { music_id: id }).await
}
async fn challenge_ranking(
    State(c): State<Arc<GameClient>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    public_ranking(
        c,
        id,
        Operation::ChallengeRanking {
            challenge_music_id: id,
        },
    )
    .await
}

async fn registry_response(
    c: Arc<GameClient>,
    snapshot: Option<String>,
    table: Option<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let root = c
        .master_directory()
        .ok_or(AppError::MasterUnavailable)?
        .to_owned();
    let scope = crate::master_registry::Scope {
        region: c.region(),
        environment: c.environment().into(),
        platform: c.platform(),
    };
    let pinned = table.is_some();
    let admission = match table {
        Some(_) => Some(c.table_reads().admit().await?),
        None => None,
    };
    let held = admission.clone();
    let document = tokio::task::spawn_blocking(move || {
        let _held = held;
        match table {
            Some((table, hash)) => crate::master_registry::table(
                &root,
                scope.region,
                snapshot
                    .as_deref()
                    .ok_or(crate::master::MasterError::Format)?,
                &table,
                &hash,
            ),
            None => crate::master_registry::manifest(&root, snapshot.as_deref(), scope),
        }
    })
    .await
    .map_err(|_| AppError::MasterUnavailable)?
    .map_err(|e| match e {
        crate::master::MasterError::NotFound => AppError::NotFound,
        _ => AppError::MasterUnavailable,
    })?;
    registry_document(document, headers, pinned)
        .map(|response| crate::master_admission::attach(admission, response))
}
pub(crate) fn registry_document(
    document: crate::master_registry::Document,
    headers: axum::http::HeaderMap,
    pinned: bool,
) -> Result<Response, AppError> {
    let unchanged = headers
        .get("if-none-match")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|item| {
                item.trim().strip_prefix("W/").unwrap_or(item.trim()) == document.etag
                    || item.trim() == "*"
            })
        });
    Response::builder()
        .status(if unchanged { 304 } else { 200 })
        .header("content-type", "application/json")
        .header("etag", document.etag)
        .header("x-master-version", document.version)
        .header(
            "cache-control",
            if pinned {
                "private, max-age=31536000, immutable"
            } else {
                "private, no-cache"
            },
        )
        .body(if unchanged {
            axum::body::Body::empty()
        } else {
            axum::body::Body::from(document.bytes)
        })
        .map_err(|_| AppError::MasterUnavailable)
}
async fn registry_current(
    State(c): State<Arc<GameClient>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    registry_response(c, None, None, headers).await
}
async fn registry_manifest(
    State(c): State<Arc<GameClient>>,
    Path(snapshot): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    registry_response(c, Some(snapshot), None, headers).await
}
async fn registry_table(
    State(c): State<Arc<GameClient>>,
    Path((snapshot, table, hash)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    registry_response(c, Some(snapshot), Some((table, hash)), headers).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    before: Option<String>,
    #[serde(default = "history_limit")]
    limit: usize,
}
fn history_limit() -> usize {
    20
}
async fn registry_history(
    State(c): State<Arc<GameClient>>,
    Query(query): Query<HistoryQuery>,
) -> Result<Response, AppError> {
    if !(1..=100).contains(&query.limit)
        || query
            .before
            .as_deref()
            .is_some_and(|v| !crate::master_registry::valid_history_cursor(v))
    {
        return Err(AppError::InvalidRequest);
    }
    let root = c
        .master_directory()
        .ok_or(AppError::MasterUnavailable)?
        .to_owned();
    let scope = crate::master_registry::Scope {
        region: c.region(),
        environment: c.environment().into(),
        platform: c.platform(),
    };
    let value = tokio::task::spawn_blocking(move || {
        crate::master_registry::history_page(&root, scope, query.limit, query.before.as_deref())
    })
    .await
    .map_err(|_| AppError::MasterUnavailable)?
    .map_err(|e| match e {
        crate::master::MasterError::NotFound => AppError::NotFound,
        _ => AppError::MasterUnavailable,
    })?;
    Response::builder()
        .header("content-type", "application/json")
        .header("cache-control", "private, no-store")
        .body(axum::body::Body::from(
            serde_json::to_vec(&value).map_err(|_| AppError::MasterUnavailable)?,
        ))
        .map_err(|_| AppError::MasterUnavailable)
}

async fn registry_by_hash(
    State(c): State<Arc<GameClient>>,
    Path(hash): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    if !crate::master_registry::hash_valid(&hash) {
        return Err(AppError::InvalidRequest);
    }
    let root = c
        .master_directory()
        .ok_or(AppError::MasterUnavailable)?
        .to_owned();
    let scope = crate::master_registry::Scope {
        region: c.region(),
        environment: c.environment().into(),
        platform: c.platform(),
    };
    let document = tokio::task::spawn_blocking(move || {
        crate::master_registry::manifest_by_hash(&root, scope, &hash)
    })
    .await
    .map_err(|_| AppError::MasterUnavailable)?
    .map_err(|e| match e {
        crate::master::MasterError::NotFound => AppError::NotFound,
        _ => AppError::MasterUnavailable,
    })?;
    registry_document(document, headers, false)
}

async fn master_database_status(State(c): State<Arc<GameClient>>) -> Json<Value> {
    Json(c.master_database_status().await)
}
async fn master_git_status(State(c): State<Arc<GameClient>>) -> Json<Value> {
    Json(c.master_git_status().await)
}

fn database_error(error: crate::master_database::Error) -> AppError {
    match error {
        crate::master_database::Error::NotFound => AppError::NotFound,
        crate::master_database::Error::InvalidRequest => AppError::InvalidRequest,
        _ => AppError::MasterUnavailable,
    }
}
fn database_scope(c: &GameClient) -> crate::master_registry::Scope {
    crate::master_registry::Scope {
        region: c.region(),
        environment: c.environment().into(),
        platform: c.platform(),
    }
}
async fn database_response(
    c: Arc<GameClient>,
    hash: Option<String>,
    table: Option<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    if hash
        .as_deref()
        .is_some_and(|h| !crate::master_registry::hash_valid(h))
        || table
            .as_deref()
            .is_some_and(|t| !crate::master::safe_component(t))
    {
        return Err(AppError::InvalidRequest);
    }
    let reader = c
        .master_database_reader()
        .ok_or(AppError::MasterUnavailable)?;
    // The admission wait precedes the read deadline: worst case is both budgets in turn.
    let admission = match table {
        Some(_) => Some(c.table_reads().admit().await?),
        None => None,
    };
    let doc = reader
        .document(&database_scope(&c), hash.as_deref(), table.as_deref())
        .await
        .map_err(database_error)?;
    // Manifests contain the first local snapshot UUID for retained content. Retention
    // may permit later re-publication with another UUID, so only exact tables are immutable.
    registry_document(doc, headers, table.is_some())
        .map(|response| crate::master_admission::attach(admission, response))
}
async fn database_current(
    State(c): State<Arc<GameClient>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    database_response(c, None, None, headers).await
}
async fn database_manifest(
    State(c): State<Arc<GameClient>>,
    Path(hash): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    database_response(c, Some(hash), None, headers).await
}
async fn database_table(
    State(c): State<Arc<GameClient>>,
    Path((hash, table)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    database_response(c, Some(hash), Some(table), headers).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseHistoryQuery {
    before: Option<i64>,
    #[serde(default = "history_limit")]
    limit: usize,
}
async fn database_history(
    State(c): State<Arc<GameClient>>,
    Query(q): Query<DatabaseHistoryQuery>,
) -> Result<Response, AppError> {
    if !(1..=200).contains(&q.limit) || q.before.is_some_and(|n| n <= 0) {
        return Err(AppError::InvalidRequest);
    }
    let reader = c
        .master_database_reader()
        .ok_or(AppError::MasterUnavailable)?;
    let page = reader
        .history(&database_scope(&c), q.limit, q.before)
        .await
        .map_err(database_error)?;
    Response::builder()
        .header("content-type", "application/json")
        .header("cache-control", "private, no-store")
        .body(axum::body::Body::from(
            serde_json::to_vec(&page).map_err(|_| AppError::MasterUnavailable)?,
        ))
        .map_err(|_| AppError::MasterUnavailable)
}

async fn registry_bundle_current(
    State(c): State<Arc<GameClient>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    registry_bundle(c, None, headers).await
}
async fn registry_bundle_hash(
    State(c): State<Arc<GameClient>>,
    Path(hash): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    if !crate::master_registry::hash_valid(&hash) {
        return Err(AppError::InvalidRequest);
    }
    registry_bundle(c, Some(hash), headers).await
}
async fn registry_bundle(
    c: Arc<GameClient>,
    hash: Option<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let permit = crate::master_bundle::permit()?;
    let root = c
        .master_directory()
        .ok_or(AppError::MasterUnavailable)?
        .to_owned();
    let scope = database_scope(&c);
    let region = scope.region;
    let source = root.clone();
    let document = tokio::task::spawn_blocking(move || match hash {
        Some(hash) => crate::master_registry::manifest_by_hash(&source, scope, &hash),
        None => crate::master_registry::manifest(&source, None, scope),
    })
    .await
    .map_err(|_| AppError::MasterUnavailable)?
    .map_err(|e| match e {
        crate::master::MasterError::NotFound => AppError::NotFound,
        _ => AppError::MasterUnavailable,
    })?;
    let manifest: crate::master_registry::PublishedManifest =
        serde_json::from_slice(&document.bytes).map_err(|_| AppError::MasterUnavailable)?;
    let snapshot = manifest.snapshot.clone();
    let version = manifest.version.clone();
    let hash = manifest.content_sha256.clone();
    let bundle = crate::master_bundle::build(
        manifest,
        move |file| {
            let root = root.clone();
            let snapshot = snapshot.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let name = file
                        .name
                        .strip_suffix(".json")
                        .ok_or(crate::master::MasterError::Format)?;
                    crate::master_registry::table(&root, region, &snapshot, name, &file.sha256)
                })
                .await
                .map_err(|_| AppError::MasterUnavailable)?
                .map(|d| d.bytes)
                .map_err(|_| AppError::MasterUnavailable)
            }
        },
        permit,
    )
    .await?;
    bundle.response(headers, &version, &hash)
}
