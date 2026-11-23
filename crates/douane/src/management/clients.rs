use super::{AppState, Json, State, StatusCode, error, json};
use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub async fn clients(State(state): State<Arc<AppState>>) -> Json<Value> {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let list: Vec<Value> = state.clients.list().iter().map(|c| c.public()).collect();
    Json(json!({
        "clients": list,
        "usage": state.clients.usage_report(now),
    }))
}

#[derive(Deserialize)]
pub struct StatsPeriod {
    hours: Option<u32>,
}

/// Longest period the client statistics cover: 90 days.
const MAX_STATS_HOURS: u32 = 90 * 24;

/// What each client spent over the last `hours` (24 by default): totals by
pub async fn client_stats(
    State(state): State<Arc<AppState>>,
    Query(period): Query<StatsPeriod>,
) -> Json<Value> {
    let hours = period.hours.unwrap_or(24).clamp(1, MAX_STATS_HOURS);
    let since = crate::subscriptions::timestamp(
        time::OffsetDateTime::now_utc() - time::Duration::hours(hours.into()),
    );
    let stats = state.ledger.client_stats(since.clone()).await;
    Json(json!({"since": since, "totals": stats.totals, "hours": stats.hours}))
}

pub async fn create_client(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<crate::clients::ClientPatch>,
) -> Response {
    match state.clients.create(patch) {
        Ok((client, key)) => {
            let client = reachable_shares(&state, client).await;
            (
                StatusCode::CREATED,
                Json(json!({"client": client.public(), "key": key})),
            )
                .into_response()
        }
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

pub async fn update_client(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(patch): Json<crate::clients::ClientPatch>,
) -> Response {
    match state.clients.update(&id, patch) {
        Ok((client, key)) => {
            let client = reachable_shares(&state, client).await;
            Json(json!({"client": client.public(), "key": key})).into_response()
        }
        Err(e) if e == "client not found" => error(StatusCode::NOT_FOUND, &e),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

/// The saved client without the shares its access does not reach: a share
/// left on an account whose access was removed must not stay listed.
async fn reachable_shares(
    state: &AppState,
    client: crate::clients::Client,
) -> crate::clients::Client {
    let accounts = state.accounts.list().await;
    if let Err(error) = state.clients.drop_unreachable_shares(&accounts) {
        tracing::warn!(%error, "unreachable client shares were not dropped");
    }
    state
        .clients
        .list()
        .into_iter()
        .find(|c| c.id == client.id)
        .unwrap_or(client)
}

pub async fn delete_client(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.clients.delete(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if e == "client not found" => error(StatusCode::NOT_FOUND, &e),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}
