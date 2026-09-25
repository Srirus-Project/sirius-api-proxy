//! Standalone Master reads without game configuration, credentials or runtime proto files.
use crate::{error::AppError, master_database as db, master_registry as registry};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    middleware,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::Read,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub token_env: String,
    pub scope: registry::Scope,
    #[serde(default)]
    pub regional_paths: bool,
    pub backend: Backend,
    pub owner: Option<crate::registry_owner::Config>,
    pub tls: Option<crate::server::TlsConfig>,
    pub logging: Option<crate::application_log::Config>,
    pub access_log: Option<crate::access_log::Config>,
}
#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Backend {
    Files { directory: PathBuf },
    Postgres { connection: db::Config },
}
impl Config {
    pub fn load(path: &FsPath) -> Result<Self, AppError> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| invalid())?
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        if bytes.len() > 65536 {
            return Err(invalid());
        }
        yaml_serde::from_slice(&bytes).map_err(|_| invalid())
    }
    pub fn prepare(&self) -> Result<Prepared, AppError> {
        if self.scope.region != crate::region::Region::Jp
            || self.scope.environment.is_empty()
            || self.scope.environment.len() > 256
            || !self
                .scope
                .environment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || self.token_env.is_empty()
            || self.token_env.len() > 128
            || !self
                .token_env
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(invalid());
        }
        let token = crate::config::secret(&self.token_env)?;
        if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err(invalid());
        }
        if let Some(log) = &self.logging {
            log.validate().map_err(|_| invalid())?;
        }
        let tls = self
            .tls
            .as_ref()
            .map(|t| t.load())
            .transpose()
            .map_err(|_| invalid())?;
        let backend = match &self.backend {
            Backend::Files { directory } => {
                if directory.as_os_str().is_empty() {
                    return Err(invalid());
                }
                Source::Files(directory.clone())
            }
            Backend::Postgres { connection } => {
                if crate::config::secret(&connection.password_env)? == token {
                    return Err(invalid());
                }
                Source::Postgres(Box::new(
                    db::Reader::new(connection).map_err(|_| invalid())?,
                ))
            }
        };
        let mut internal_token = None;
        let owner = if let Some(owner) = &self.owner {
            if let Some(source) = &owner.source {
                source.validate()?;
            }
            if owner.internal_token_env.is_empty()
                || owner.internal_token_env.len() > 128
                || !owner
                    .internal_token_env
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(invalid());
            }
            let internal = crate::config::secret(&owner.internal_token_env)?;
            let upstream = owner
                .source
                .as_ref()
                .map(|source| crate::config::secret(&source.token_env))
                .transpose()?;
            if internal.is_empty()
                || internal.len() > 4096
                || !internal.bytes().all(|b| (33..=126).contains(&b))
                || internal == token
                || upstream
                    .as_ref()
                    .is_some_and(|upstream| upstream == &token || upstream == &internal)
            {
                return Err(invalid());
            }
            let (directory, database) = match &self.backend {
                Backend::Files { directory } => {
                    if owner.staging_directory.is_some() {
                        return Err(invalid());
                    }
                    (directory.clone(), None)
                }
                Backend::Postgres { connection } => {
                    let password = crate::config::secret(&connection.password_env)?;
                    if password == internal || upstream.as_ref() == Some(&password) {
                        return Err(invalid());
                    }
                    (
                        owner.staging_directory.clone().ok_or_else(invalid)?,
                        Some(connection.clone()),
                    )
                }
            };
            internal_token = Some(internal);
            Some(crate::registry_owner::Worker::new(
                owner,
                self.scope.clone(),
                directory,
                database,
            )?)
        } else {
            None
        };
        let state = Arc::new(Service {
            scope: self.scope.clone(),
            backend,
            owner: owner.clone(),
        });
        let routes = Router::new()
            .route("/manifest", get(current))
            .route("/bundle", get(bundle_current))
            .route("/by-hash/{hash}/bundle", get(bundle_hash))
            .route("/by-hash/{hash}/manifest", get(by_hash))
            .route("/snapshots/{snapshot}/manifest", get(snapshot_manifest))
            .route("/snapshots/{snapshot}/tables/{table}/{hash}", get(table))
            .route("/history", get(history))
            .route_layer(middleware::from_fn_with_state(
                Arc::<str>::from(token),
                crate::api::authorize,
            ))
            .with_state(state.clone());
        let prefix = if self.regional_paths {
            "/api/v1/jp/master-data"
        } else {
            "/api/v1/master-data"
        };
        let mut router = Router::new().route("/health",get(||async {Json(json!({"status":"ok","service":"sirius-master-registry","version":env!("CARGO_PKG_VERSION")}))})).nest(prefix,routes);
        if let Some(token) = internal_token {
            let internal = Router::new()
                .route("/master-data/updater", get(owner_status))
                .route("/master-data/refresh", post(owner_refresh))
                .route("/master-data/publish", post(owner_publish))
                .route(
                    "/master-data/sync",
                    post(owner_hint).layer(axum::extract::DefaultBodyLimit::max(4096)),
                )
                .route_layer(middleware::from_fn_with_state(
                    Arc::<str>::from(token),
                    crate::api::authorize,
                ))
                .with_state(state);
            router = router.nest(
                if self.regional_paths {
                    "/internal/v1/jp"
                } else {
                    "/internal/v1"
                },
                internal,
            );
        }
        if let Some(log) = &self.access_log {
            router = crate::access_log::AccessLog::new(log.clone())
                .map_err(|_| invalid())?
                .wrap(router);
        }
        Ok(Prepared {
            listen: self.listen,
            tls,
            router,
            owner,
        })
    }
}
fn invalid() -> AppError {
    AppError::Config("invalid standalone registry configuration")
}
pub struct Prepared {
    pub listen: SocketAddr,
    pub tls: Option<crate::server::LoadedTls>,
    pub router: Router,
    pub owner: Option<Arc<crate::registry_owner::Worker>>,
}
enum Source {
    Files(PathBuf),
    Postgres(Box<db::Reader>),
}
struct Service {
    scope: registry::Scope,
    backend: Source,
    owner: Option<Arc<crate::registry_owner::Worker>>,
}
enum Selection {
    Current,
    Snapshot(String),
    Hash(String),
}
fn file_error(e: crate::master::MasterError) -> AppError {
    match e {
        crate::master::MasterError::NotFound => AppError::NotFound,
        _ => AppError::MasterUnavailable,
    }
}
fn db_error(e: db::Error) -> AppError {
    match e {
        db::Error::NotFound => AppError::NotFound,
        db::Error::InvalidRequest => AppError::InvalidRequest,
        _ => AppError::MasterUnavailable,
    }
}
impl Service {
    async fn document(
        &self,
        selection: Selection,
        table: Option<(String, String)>,
    ) -> Result<registry::Document, AppError> {
        if match &selection {
            Selection::Hash(h) => !registry::hash_valid(h),
            Selection::Snapshot(s) => !registry::valid_history_cursor(s),
            Selection::Current => false,
        } || table.as_ref().is_some_and(|(name, hash)| {
            !crate::master::safe_component(name) || !registry::hash_valid(hash)
        }) {
            return Err(AppError::InvalidRequest);
        }
        match &self.backend {
            Source::Files(root) => {
                let root = root.clone();
                let scope = self.scope.clone();
                tokio::task::spawn_blocking(move || match (selection, table) {
                    (Selection::Snapshot(id), Some((name, hash))) => {
                        registry::table(&root, &id, &name, &hash)
                    }
                    (Selection::Snapshot(id), None) => registry::manifest(&root, Some(&id), scope),
                    (Selection::Hash(hash), None) => {
                        registry::manifest_by_hash(&root, scope, &hash)
                    }
                    (Selection::Current, None) => registry::manifest(&root, None, scope),
                    _ => Err(crate::master::MasterError::Format),
                })
                .await
                .map_err(|_| AppError::MasterUnavailable)?
                .map_err(file_error)
            }
            Source::Postgres(reader) => {
                // Stable virtual IDs make the standard snapshot/table contract independent
                // of local writer UUIDs and subsequent retention/republication.
                let hash = match selection {
                    Selection::Current => None,
                    Selection::Hash(hash) => Some(hash),
                    Selection::Snapshot(id) => Some(
                        id.strip_prefix("master-")
                            .filter(|h| registry::hash_valid(h))
                            .ok_or(AppError::InvalidRequest)?
                            .to_owned(),
                    ),
                };
                let mut doc = reader
                    .document(
                        &self.scope,
                        hash.as_deref(),
                        table.as_ref().map(|(n, _)| n.as_str()),
                    )
                    .await
                    .map_err(db_error)?;
                if let Some((_, expected)) = table {
                    if registry::digest(&doc.bytes) != expected {
                        return Err(AppError::NotFound);
                    }
                } else {
                    let mut manifest: registry::PublishedManifest =
                        serde_json::from_slice(&doc.bytes)
                            .map_err(|_| AppError::MasterUnavailable)?;
                    manifest.snapshot = format!("master-{}", manifest.content_sha256);
                    doc.bytes =
                        serde_json::to_vec(&manifest).map_err(|_| AppError::MasterUnavailable)?;
                    doc.etag = format!("\"{}\"", registry::digest(&doc.bytes));
                }
                Ok(doc)
            }
        }
    }
}
async fn respond(
    s: Arc<Service>,
    selection: Selection,
    table: Option<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let pinned = table.is_some();
    crate::api::registry_document(s.document(selection, table).await?, headers, pinned)
}
async fn current(State(s): State<Arc<Service>>, headers: HeaderMap) -> Result<Response, AppError> {
    respond(s, Selection::Current, None, headers).await
}
async fn by_hash(
    State(s): State<Arc<Service>>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    respond(s, Selection::Hash(hash), None, headers).await
}
async fn snapshot_manifest(
    State(s): State<Arc<Service>>,
    Path(snapshot): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    respond(s, Selection::Snapshot(snapshot), None, headers).await
}
async fn table(
    State(s): State<Arc<Service>>,
    Path((snapshot, table, hash)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    respond(
        s,
        Selection::Snapshot(snapshot),
        Some((table, hash)),
        headers,
    )
    .await
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
async fn history(
    State(s): State<Arc<Service>>,
    Query(q): Query<HistoryQuery>,
) -> Result<Response, AppError> {
    if !(1..=100).contains(&q.limit) {
        return Err(AppError::InvalidRequest);
    }
    let value: Value = match &s.backend {
        Source::Files(root) => {
            if q.before
                .as_deref()
                .is_some_and(|v| !registry::valid_history_cursor(v))
            {
                return Err(AppError::InvalidRequest);
            }
            let root = root.clone();
            let scope = s.scope.clone();
            let page = tokio::task::spawn_blocking(move || {
                registry::history_page(&root, scope, q.limit, q.before.as_deref())
            })
            .await
            .map_err(|_| AppError::MasterUnavailable)?
            .map_err(file_error)?;
            json!({"backend":"files","history":page})
        }
        Source::Postgres(reader) => {
            let before = q
                .before
                .map(|v| v.parse::<i64>().map_err(|_| AppError::InvalidRequest))
                .transpose()?;
            let page = reader
                .history(&s.scope, q.limit, before)
                .await
                .map_err(db_error)?;
            json!({"backend":"postgres","history":page})
        }
    };
    Response::builder()
        .header("content-type", "application/json")
        .header("cache-control", "private, no-store")
        .body(axum::body::Body::from(
            serde_json::to_vec(&value).map_err(|_| AppError::MasterUnavailable)?,
        ))
        .map_err(|_| AppError::MasterUnavailable)
}

async fn bundle_current(
    State(s): State<Arc<Service>>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    bundle(s, Selection::Current, headers).await
}
async fn bundle_hash(
    State(s): State<Arc<Service>>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    bundle(s, Selection::Hash(hash), headers).await
}
async fn bundle(
    s: Arc<Service>,
    selection: Selection,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let permit = crate::master_bundle::permit()?;
    let document = s.document(selection, None).await?;
    let manifest: registry::PublishedManifest =
        serde_json::from_slice(&document.bytes).map_err(|_| AppError::MasterUnavailable)?;
    let snapshot = manifest.snapshot.clone();
    let version = manifest.version.clone();
    let hash = manifest.content_sha256.clone();
    let bundle = crate::master_bundle::build(
        manifest,
        move |file| {
            let s = s.clone();
            let snapshot = snapshot.clone();
            async move {
                let name = file
                    .name
                    .strip_suffix(".json")
                    .ok_or(AppError::MasterUnavailable)?
                    .to_owned();
                s.document(Selection::Snapshot(snapshot), Some((name, file.sha256)))
                    .await
                    .map(|d| d.bytes)
            }
        },
        permit,
    )
    .await?;
    bundle.response(headers, &version, &hash)
}

async fn owner_status(State(s): State<Arc<Service>>) -> Result<Json<Value>, AppError> {
    Ok(Json(
        s.owner.as_ref().ok_or(AppError::NotFound)?.status().await,
    ))
}
async fn owner_refresh(
    State(s): State<Arc<Service>>,
) -> Result<(axum::http::StatusCode, Json<Value>), AppError> {
    let owner = s.owner.as_ref().ok_or(AppError::NotFound)?;
    if !owner.has_source() {
        return Err(AppError::NotFound);
    }
    owner.refresh();
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"status":"accepted"})),
    ))
}
async fn owner_hint(
    State(s): State<Arc<Service>>,
    Json(hint): Json<crate::master_sync::UpdateHint>,
) -> Result<(axum::http::StatusCode, Json<Value>), AppError> {
    s.owner.as_ref().ok_or(AppError::NotFound)?.hint(&hint)?;
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"status":"accepted"})),
    ))
}

async fn owner_publish(
    State(s): State<Arc<Service>>,
) -> Result<(axum::http::StatusCode, Json<Value>), AppError> {
    s.owner.as_ref().ok_or(AppError::NotFound)?.publish_local();
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"status":"accepted"})),
    ))
}
