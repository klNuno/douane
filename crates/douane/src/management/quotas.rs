use super::{AppState, Json, State, StatusCode, error, json};
use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub async fn quotas(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"quotas":state.quotas.list().await}))
}
pub async fn account_resets(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.resets.status(&id).await {
        Ok(status) => Json(json!(status)).into_response(),
        Err(error) => reset_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetConfirmation {
    #[serde(default)]
    confirmation: bool,
    request_id: String,
    /// Which resets to draw from; absent uses the one that expires first.
    #[serde(default)]
    scope: Option<crate::subscriptions::ResetScope>,
}

pub async fn redeem_reset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ResetConfirmation>,
) -> Response {
    if !body.confirmation {
        return error(
            StatusCode::BAD_REQUEST,
            "confirm before consuming a reset credit",
        );
    }
    match state.resets.redeem(&id, &body.request_id, body.scope).await {
        Ok(result) => {
            let _ = state.quotas.refresh(&id).await;
            Json(json!(result)).into_response()
        }
        Err(error) => reset_error(error),
    }
}

fn reset_error(failure: crate::subscriptions::ResetError) -> Response {
    use crate::subscriptions::{ResetError, SubscriptionError};
    let status = match &failure {
        ResetError::InvalidRequest => StatusCode::BAD_REQUEST,
        ResetError::Pending | ResetError::MissingOrganization | ResetError::HistoryFull => {
            StatusCode::CONFLICT
        }
        ResetError::Cooldown => StatusCode::TOO_MANY_REQUESTS,
        ResetError::Subscription(SubscriptionError::NotFound) => StatusCode::NOT_FOUND,
        ResetError::Subscription(SubscriptionError::Unsupported) => StatusCode::BAD_REQUEST,
        ResetError::Subscription(SubscriptionError::InvalidImport(_)) => StatusCode::BAD_REQUEST,
        ResetError::Subscription(SubscriptionError::Storage) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_GATEWAY,
    };
    error(status, &failure.to_string())
}
pub async fn refresh_quotas(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.refresh_quotas(body["account_id"].as_str()).await;
    Json(json!({"quotas":state.quotas.list().await}))
}
#[derive(Deserialize, Default)]
pub struct QuotaRead {
    /// `1` or `true`: the client's user asked for fresh numbers.
    refresh: Option<String>,
}

/// Longest a client waits for a refresh it asked for; slower reads land in
/// the next answer.
const CLIENT_REFRESH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn public_quotas(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(read): Query<QuotaRead>,
) -> Response {
    let client = match state.clients.authenticate(&headers) {
        Ok(client) => Some(client),
        Err(refusal) => return error(StatusCode::UNAUTHORIZED, refusal.message()),
    };
    if matches!(read.refresh.as_deref(), Some("1" | "true")) {
        // A read asked by hand: failures show at once, and a success clears
        // any shown error. The task outlives a timeout so no write is cut.
        let worker = state.clone();
        let task = tokio::spawn(async move {
            worker
                .refresh_quotas_matching(|account| account.provider.reports_quota())
                .await;
        });
        let _ = tokio::time::timeout(CLIENT_REFRESH_WAIT, task).await;
    }
    let settings = state.config.read().await.settings.clone();
    let mut accounts = state.accounts.list().await;
    let mut snapshots = state.quotas.list().await;
    // A client sees its accounts, each window as the part of what it may
    // use that it has used.
    if let Some(client) = &client {
        accounts.retain(|a| client.may_use_summary(a));
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        snapshots = snapshots
            .into_iter()
            .filter_map(|snapshot| {
                let account = accounts.iter().find(|a| a.id == snapshot.account_id)?;
                let pool = account
                    .route
                    .as_deref()
                    .unwrap_or(account.provider.as_str());
                Some(
                    state
                        .clients
                        .client_view(client, snapshot, pool, now, settings.release()),
                )
            })
            .collect();
    }
    Json(crate::quota_view::overview(
        accounts,
        snapshots,
        &settings,
        crate::subscriptions::now(),
    ))
    .into_response()
}

/// provider and model, and calls and tokens by UTC hour.
#[derive(Deserialize)]
pub struct HistoryRange {
    from: Option<String>,
    to: Option<String>,
}

/// Stored quota readings for the pool chart, by account window. The range
/// defaults to the last two weeks and spans at most the retention period.
pub async fn quota_history(
    State(state): State<Arc<AppState>>,
    Query(range): Query<HistoryRange>,
) -> Response {
    let parse = |value: &Option<String>| match value.as_deref() {
        None => Ok(None),
        Some(value) => crate::subscriptions::parse_timestamp(value)
            .map(|t| Some(t.unix_timestamp()))
            .ok_or(()),
    };
    let (Ok(from), Ok(to)) = (parse(&range.from), parse(&range.to)) else {
        return error(
            StatusCode::BAD_REQUEST,
            "from and to must be RFC 3339 times",
        );
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let to = to.unwrap_or(now);
    let from = from.unwrap_or(to - 14 * 86400);
    if from > to || to - from > crate::quota_history::RETENTION_DAYS * 86400 {
        return error(
            StatusCode::BAD_REQUEST,
            "the range must end after it starts and span at most 45 days",
        );
    }
    let accounts: std::collections::BTreeSet<String> = state
        .accounts
        .list()
        .await
        .into_iter()
        .map(|account| account.id)
        .collect();
    let samples = state.ledger.quota_history(from, to).await;
    // A deleted account's readings stay until they age out, but are not shown.
    let samples = samples
        .into_iter()
        .filter(|sample| accounts.contains(&sample.account_id))
        .collect();
    Json(crate::quota_history::series(samples)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;

    #[tokio::test]
    async fn public_quota_refresh_skips_compatible_billing() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::settings::Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let account = state
            .accounts
            .import(
                json!({
                    "type": "openai_compatible", "route": "openrouter",
                    "base_url": "https://openrouter.ai/api/v1", "api_key": "fixture-key"
                }),
                None,
            )
            .await
            .unwrap();
        let (_, key) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("Fixture".into()),
                ..Default::default()
            })
            .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {key}").parse().unwrap(),
        );

        let response = public_quotas(
            State(state.clone()),
            headers,
            Query(QuotaRead {
                refresh: Some("1".into()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(state.quotas.get(&account.id).await.is_none());

        state.refresh_quotas(Some(&account.id)).await;
        // In tests the billing client fails locally unless given a loopback origin.
        assert!(state.quotas.get(&account.id).await.unwrap().error.is_some());
    }
}
