//! Bounded administrative commands processed by the outbox's sole owner.
use crate::asset_outbox::{Error, Outbox};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub struct Control {
    sender: mpsc::Sender<Command>,
}
pub(crate) struct Command {
    action: Action,
    reply: oneshot::Sender<Result<Value, StatusCode>>,
}
enum Action {
    List(Page),
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
pub(crate) fn channel() -> (Control, mpsc::Receiver<Command>) {
    let (sender, receiver) = mpsc::channel(16);
    (Control { sender }, receiver)
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
            .route("/entries", get(list))
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
