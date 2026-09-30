//! Bounded administrative commands processed by the outbox's sole owner, and the worker status
//! it publishes without going through that queue.
use crate::asset_outbox::{Error, Outbox, State as EntryState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
pub struct Control {
    sender: mpsc::Sender<Command>,
    status: watch::Receiver<DispatchStatus>,
}
/// Worker state for `GET .../asset-dispatch/status`. Every string is a closed-set constant:
/// no origin, token, target label, updater body or error text is ever stored here.
#[derive(Clone, Serialize)]
pub struct DispatchStatus {
    /// `pending`, `observing`, `reconciling`, `idle` or `stopped`.
    pub status: &'static str,
    /// `shutdown`, `asset_outbox_storage` or `exited` once stopped.
    pub stop_reason: Option<&'static str>,
    pub updated_at: DateTime<Utc>,
    pub cycle_started_at: Option<DateTime<Utc>>,
    pub last_cycle_at: Option<DateTime<Utc>>,
    pub next_cycle_at: Option<DateTime<Utc>>,
    pub last_observation: Option<ObservationStatus>,
    pub last_reconcile: Option<ReconcileStatus>,
    pub entries: EntryCounts,
    /// Working-ledger failures by code; unknown persisted codes count as `other`.
    pub failed_by_code: BTreeMap<&'static str, usize>,
}
#[derive(Clone, Serialize)]
pub struct ObservationStatus {
    pub at: DateTime<Utc>,
    /// `recorded`, `unavailable`, `rejected`, `capacity_exhausted` or `storage_failed`.
    pub result: &'static str,
    /// Only for `recorded`; already validated as a dispatch identity component.
    pub resource_version: Option<String>,
}
#[derive(Clone, Serialize)]
pub struct ReconcileStatus {
    pub at: DateTime<Utc>,
    /// `completed` or `storage_failed`.
    pub result: &'static str,
    pub batch: usize,
    pub transport_errors: usize,
}
/// Working ledger only, like the list's `total`; archived completions are excluded.
#[derive(Clone, Default, Serialize)]
pub struct EntryCounts {
    pub total: usize,
    pub capacity: usize,
    pub pending: usize,
    pub sending: usize,
    pub submitted: usize,
    pub completed: usize,
    pub failed: usize,
    /// Pending entries the updater refused as busy since this process started.
    pub busy_retrying: usize,
}
impl DispatchStatus {
    pub(crate) fn pending(outbox: &Outbox, capacity: usize) -> Self {
        let (entries, failed_by_code) = summarize(outbox, capacity, &HashMap::new());
        Self {
            status: "pending",
            stop_reason: None,
            updated_at: Utc::now(),
            cycle_started_at: None,
            last_cycle_at: None,
            next_cycle_at: None,
            last_observation: None,
            last_reconcile: None,
            entries,
            failed_by_code,
        }
    }
}
/// One pass over the working ledger (at most `history_capacity` entries).
pub(crate) fn summarize(
    outbox: &Outbox,
    capacity: usize,
    refusals: &HashMap<String, u32>,
) -> (EntryCounts, BTreeMap<&'static str, usize>) {
    let mut counts = EntryCounts {
        total: outbox.entries().len(),
        capacity,
        ..Default::default()
    };
    let mut failed = BTreeMap::new();
    for (key, entry) in outbox.entries() {
        match &entry.state {
            EntryState::Pending => {
                counts.pending += 1;
                counts.busy_retrying += usize::from(refusals.contains_key(key));
            }
            EntryState::Sending { .. } => counts.sending += 1,
            EntryState::Submitted { .. } => counts.submitted += 1,
            EntryState::Completed { .. } => counts.completed += 1,
            EntryState::Failed { code, .. } => {
                counts.failed += 1;
                let code = crate::asset_dispatch::FAILURE_CODES
                    .iter()
                    .find(|known| **known == code.as_str())
                    .copied()
                    .unwrap_or("other");
                *failed.entry(code).or_default() += 1;
            }
        }
    }
    (counts, failed)
}
pub(crate) struct Command {
    action: Action,
    reply: oneshot::Sender<Result<Value, StatusCode>>,
}
enum Action {
    List(Page),
    Detail { key: String },
    Archive { key: String, job_id: String },
    Adopt { key: String, job_id: String },
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    limit: Option<usize>,
    after: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Adoption {
    job_id: String,
}
pub(crate) fn channel(
    initial: DispatchStatus,
) -> (
    Control,
    mpsc::Receiver<Command>,
    watch::Sender<DispatchStatus>,
) {
    let (sender, receiver) = mpsc::channel(16);
    let (publisher, status) = watch::channel(initial);
    (Control { sender, status }, receiver, publisher)
}
fn valid_key(key: &str) -> bool {
    key.len() == 71
        && key.starts_with("sirius-")
        && key[7..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
impl Control {
    async fn call(&self, action: Action) -> Result<Json<Value>, StatusCode> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .try_send(Command { action, reply })
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        tokio::time::timeout(Duration::from_secs(5), receiver)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .map(Json)
    }
}
pub fn router(control: Control, prefix: &str, token: String) -> Router {
    Router::new().nest(
        prefix,
        Router::new()
            .route("/status", get(status))
            .route("/entries", get(list))
            .route("/entries/{key}", get(detail))
            .route(
                "/entries/{key}/archive",
                post(archive).layer(axum::extract::DefaultBodyLimit::max(4096)),
            )
            .route(
                "/entries/{key}/adopt",
                post(adopt).layer(axum::extract::DefaultBodyLimit::max(4096)),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                Arc::<str>::from(token),
                crate::api::authorize,
            ))
            .with_state(control),
    )
}
/// Always 200 and never queued: readable while a reconciliation holds the worker and after
/// the worker has stopped. A dropped sender without a final status means the task exited.
async fn status(State(control): State<Control>) -> Json<DispatchStatus> {
    let exited = control.status.has_changed().is_err();
    let mut status = control.status.borrow().clone();
    if exited && status.status != "stopped" {
        status.status = "stopped";
        status.stop_reason = Some("exited");
        status.cycle_started_at = None;
        status.next_cycle_at = None;
    }
    Json(status)
}
async fn list(
    State(control): State<Control>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, StatusCode> {
    if !(1..=200).contains(&page.limit.unwrap_or(50))
        || page.after.as_ref().is_some_and(|k| !valid_key(k))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    control.call(Action::List(page)).await
}
async fn adopt(
    State(control): State<Control>,
    Path(key): Path<String>,
    Json(body): Json<Adoption>,
) -> Result<Json<Value>, StatusCode> {
    if !valid_key(&key)
        || uuid::Uuid::parse_str(&body.job_id).map_or(true, |id| id.to_string() != body.job_id)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    control
        .call(Action::Adopt {
            key,
            job_id: body.job_id,
        })
        .await
}
async fn detail(
    State(control): State<Control>,
    Path(key): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    if !valid_key(&key) {
        return Err(StatusCode::BAD_REQUEST);
    }
    control.call(Action::Detail { key }).await
}
async fn archive(
    State(control): State<Control>,
    Path(key): Path<String>,
    Json(body): Json<Adoption>,
) -> Result<Json<Value>, StatusCode> {
    if !valid_key(&key)
        || uuid::Uuid::parse_str(&body.job_id).map_or(true, |id| id.to_string() != body.job_id)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    control
        .call(Action::Archive {
            key,
            job_id: body.job_id,
        })
        .await
}
pub(crate) fn handle(command: Command, outbox: &mut Outbox) {
    // Requests abandoned while waiting for network reconciliation must not mutate later.
    if command.reply.is_closed() {
        return;
    }
    let result = match command.action {
        Action::List(page) => {
            let mut selected = outbox
                .entries()
                .iter()
                .filter(|(key, _)| page.after.as_ref().is_none_or(|cursor| *key > cursor));
            let entries: Vec<Value> = selected
                .by_ref()
                .take(page.limit.unwrap_or(50))
                .map(|(key, entry)| json!({"key":key,"entry":entry}))
                .collect();
            let next = if selected.next().is_some() {
                entries.last().map(|e| e["key"].clone())
            } else {
                None
            };
            Ok(
                json!({"status":"ready","total":outbox.entries().len(),"entries":entries,"next_after":next}),
            )
        }
        Action::Detail { key } => {
            if let Some(entry) = outbox.entries().get(&key) {
                Ok(json!({"key":key,"archived":false,"entry":entry}))
            } else {
                match outbox.archived(&key) {
                    Ok(Some(entry)) => Ok(json!({"key":key,"archived":true,"entry":entry})),
                    Ok(None) => Err(StatusCode::NOT_FOUND),
                    Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
                }
            }
        }
        Action::Archive { key, job_id } => outbox
            .archive_completed(&key, &job_id)
            .map(|entry| json!({"key":key,"archived":true,"entry":entry}))
            .map_err(|error| match error {
                Error::Invalid => StatusCode::CONFLICT,
                _ => StatusCode::SERVICE_UNAVAILABLE,
            }),
        Action::Adopt { key, job_id } => {
            if !outbox.entries().contains_key(&key) {
                Err(StatusCode::NOT_FOUND)
            } else {
                outbox
                    .adopt(&key, &job_id)
                    .map(|()| json!({"key":key,"entry":outbox.entries()[&key]}))
                    .map_err(|error| match error {
                        Error::Invalid => StatusCode::CONFLICT,
                        _ => StatusCode::SERVICE_UNAVAILABLE,
                    })
            }
        }
    };
    let _ = command.reply.send(result);
}
