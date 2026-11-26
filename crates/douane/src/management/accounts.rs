use super::{AppState, Json, State, StatusCode, error, json};
use super::{Params, SubscriptionProvider};
use crate::subscriptions::{AccountSummary, EndpointPatch, SubscriptionError};
use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use axum::{http::header, response::Redirect};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub async fn accounts(State(state): State<Arc<AppState>>) -> Json<Value> {
    let now = time::OffsetDateTime::now_utc();
    let mut accounts = vec![];
    for summary in state.accounts.list().await {
        accounts.push(account_value(&state, summary, now).await);
    }
    Json(json!({"accounts":accounts}))
}

/// An account summary with its reserve state, as listed and as returned after
/// a change. `reserve_measured` is null until a quota read has succeeded and
/// false only when the provider reports no account-wide window.
async fn account_value(
    state: &AppState,
    summary: crate::subscriptions::AccountSummary,
    now: time::OffsetDateTime,
) -> Value {
    let snapshot = state.quotas.get(&summary.id).await;
    let measured = snapshot
        .as_ref()
        .filter(|q| !(q.windows.is_empty() && q.error.is_some()))
        .map(|q| q.reports_reserve_window(&summary.reserve_windows));
    let reached = crate::state::reserve_reached_in(
        summary.reserve_percent,
        &summary.reserve_windows,
        snapshot.as_ref(),
        now,
        state.free_before_reset_seconds().await,
    );
    let quota_supported = crate::subscriptions::QuotaService::supports(&summary);
    let mut value = json!(summary);
    value["quota_supported"] = json!(quota_supported);
    value["reserve_measured"] = json!(measured);
    value["reserve_reached"] = json!(reached);
    value
}
pub async fn account_verification(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    verification_redirect(state.quotas.verification_url(&id).await)
}

pub async fn oauth_verification(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    verification_redirect(state.oauth.verification_url(&id).await)
}

fn verification_redirect(url: Option<String>) -> Response {
    let mut response = match url {
        Some(url) => Redirect::temporary(&url).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            "account verification is not available",
        ),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    response
}
pub async fn import(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let files = body
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![json!({"content":body})]);
    if files.len() > 100 {
        return error(
            StatusCode::BAD_REQUEST,
            "import at most 100 accounts at once",
        );
    }
    let mut accounts = vec![];
    let mut errors = vec![];
    for file in files {
        let value = if let Some(s) = file["content"].as_str() {
            match serde_json::from_str(s) {
                Ok(v) => v,
                Err(_) => {
                    errors.push(json!({"name":file["name"],"error":"invalid JSON"}));
                    continue;
                }
            }
        } else {
            file["content"].clone()
        };
        match state
            .accounts
            .import(value, file["name"].as_str().map(str::to_owned))
            .await
        {
            Ok(a) => accounts.push(a),
            Err(e) => errors.push(json!({"name":file["name"],"error":e.to_string()})),
        }
    }
    refresh_imported_accounts(&state, &accounts).await;
    (
        if accounts.is_empty() && !errors.is_empty() {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        },
        Json(json!({"imported":accounts.len(),"accounts":accounts,"errors":errors})),
    )
        .into_response()
}
pub(super) async fn refresh_imported_accounts(state: &Arc<AppState>, accounts: &[AccountSummary]) {
    state.refresh_models().await;
    let ids: Vec<String> = accounts.iter().map(|a| a.id.clone()).collect();
    let worker = state.clone();
    tokio::spawn(async move {
        for id in ids {
            let _ = worker.quotas.refresh(&id).await;
        }
    });
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AccountPatch {
    name: Option<String>,
    enabled: Option<bool>,
    /// Absent keeps the reserve; `null` or `0` removes it.
    #[serde(default, deserialize_with = "present")]
    reserve_percent: Option<Option<f64>>,
    /// Absent keeps the watched windows; `[]` watches the longest window.
    reserve_windows: Option<Vec<String>>,
    /// Custom endpoints only: model prefix, address, wire format (`null`
    /// forwards the client's own), served models (`[]` serves all) and key.
    route: Option<String>,
    base_url: Option<String>,
    #[serde(default, deserialize_with = "present_text")]
    dialect: Option<Option<String>>,
    /// Images per Chat request; `null` returns to the default of 20.
    #[serde(default, deserialize_with = "present_count")]
    image_limit: Option<Option<u32>>,
    /// Entries added to each request's `metadata`; `{}` removes them.
    metadata: Option<std::collections::BTreeMap<String, String>>,
    models: Option<Vec<String>>,
    api_key: Option<String>,
}

fn present_text<'de, D: serde::Deserializer<'de>>(
    value: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(value).map(Some)
}

fn present_count<'de, D: serde::Deserializer<'de>>(
    value: D,
) -> Result<Option<Option<u32>>, D::Error> {
    Option::<u32>::deserialize(value).map(Some)
}

fn present<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<Option<f64>>, D::Error> {
    Option::<f64>::deserialize(value).map(Some)
}

struct AccountMutation {
    name: Option<String>,
    enabled: Option<bool>,
    reserve: Option<Option<f64>>,
    reserve_windows: Option<Vec<String>>,
}

impl AccountPatch {
    fn validate(mut self) -> Result<(Option<AccountMutation>, Option<EndpointPatch>), String> {
        let name = self.name.take().map(|name| name.trim().to_owned());
        if name.as_ref().is_some_and(|name| {
            name.is_empty() || name.chars().count() > 120 || name.chars().any(char::is_control)
        }) {
            return Err("name must contain 1 to 120 characters without control characters".into());
        }
        let reserve = self
            .reserve_percent
            .map(|r| r.filter(|percent| *percent != 0.0));
        if reserve
            .flatten()
            .is_some_and(|percent| !crate::subscriptions::valid_reserve(percent))
        {
            return Err(SubscriptionError::InvalidReserve.to_string());
        }
        if self
            .reserve_windows
            .as_deref()
            .is_some_and(|kinds| crate::subscriptions::reserve_windows(kinds).is_none())
        {
            return Err(SubscriptionError::InvalidReserveWindows.to_string());
        }
        let account = (name.is_some()
            || self.enabled.is_some()
            || reserve.is_some()
            || self.reserve_windows.is_some())
        .then_some(AccountMutation {
            name,
            enabled: self.enabled,
            reserve,
            reserve_windows: self.reserve_windows.take(),
        });
        let endpoint = self.endpoint();
        if account.is_none() && endpoint.is_none() {
            return Err("provide a name, enabled state, reserve or endpoint field".into());
        }
        Ok((account, endpoint))
    }

    fn endpoint(self) -> Option<EndpointPatch> {
        (self.route.is_some()
            || self.base_url.is_some()
            || self.dialect.is_some()
            || self.image_limit.is_some()
            || self.metadata.is_some()
            || self.models.is_some()
            || self.api_key.is_some())
        .then_some(EndpointPatch {
            route: self.route,
            base_url: self.base_url,
            dialect: self.dialect,
            image_limit: self.image_limit,
            metadata: self.metadata,
            models: self.models,
            api_key: self.api_key,
        })
    }
}

pub async fn update_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<AccountPatch>,
) -> Response {
    let (account, endpoint) = match body.validate() {
        Ok(patch) => patch,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    match apply_account_patch(&state, &id, account, endpoint).await {
        Ok(summary) => {
            let account = account_value(&state, summary, time::OffsetDateTime::now_utc()).await;
            Json(json!({ "account": account })).into_response()
        }
        Err(failure) => account_patch_error(failure),
    }
}

async fn apply_account_patch(
    state: &AppState,
    id: &str,
    account: Option<AccountMutation>,
    endpoint: Option<EndpointPatch>,
) -> Result<AccountSummary, SubscriptionError> {
    // A rejected endpoint change must leave the account fields untouched.
    if let Some(endpoint) = endpoint {
        state.accounts.update_endpoint(id, endpoint).await?;
        state.refresh_models().await;
    }
    match account {
        Some(account) => {
            state
                .accounts
                .update(
                    id,
                    account.name,
                    account.enabled,
                    account.reserve,
                    account.reserve_windows,
                )
                .await
        }
        None => state
            .accounts
            .get(id)
            .await
            .map(|account| account.summary())
            .ok_or(SubscriptionError::NotFound),
    }
}

fn account_patch_error(failure: SubscriptionError) -> Response {
    let status = match failure {
        SubscriptionError::NotFound => StatusCode::NOT_FOUND,
        SubscriptionError::InvalidReserve
        | SubscriptionError::InvalidReserveWindows
        | SubscriptionError::InvalidEndpoint(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error(status, &failure.to_string())
}

pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.accounts.delete(&id).await {
        Ok(()) => {
            state.refresh_models().await;
            Json(json!({"deleted":true})).into_response()
        }
        Err(e) => error(StatusCode::NOT_FOUND, &e.to_string()),
    }
}
pub async fn providers(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut result = json!({"providers":[
        {"id":"claude","oauth":true,"quota":true,"docs_url":"https://claude.ai/settings/usage"},
        {"id":"codex","oauth":true,"quota":true,"docs_url":"https://chatgpt.com/codex/settings/usage"},
        {"id":"opencode_go","oauth":false,"quota":true,"docs_url":"https://opencode.ai/docs/go/"},
        {"id":"xai","oauth":true,"quota":true,"docs_url":"https://grok.com"},
        {"id":"copilot","oauth":true,"quota":true,"docs_url":"https://github.com/settings/copilot"},
        {"id":"meta","oauth":true,"quota":true,"docs_url":"https://muse.meta.com"},
        {"id":"antigravity","oauth":true,"quota":true,"docs_url":"https://antigravity.google"},
        {"id":"kimi","oauth":false,"quota":true,"docs_url":"https://kimi.com/code"},
        {"id":"qwen","oauth":false,"quota":false,"docs_url":"https://chat.qwen.ai"},
        {"id":"openai_compatible","oauth":false,"quota":false,"docs_url":"https://platform.openai.com"}
    ]});
    for provider in result["providers"].as_array_mut().unwrap() {
        let id = provider["id"]
            .as_str()
            .and_then(SubscriptionProvider::parse)
            .unwrap();
        provider["name"] = json!(crate::quota_view::provider_name(id));
        provider["oauth"] = json!(state.oauth.supports(id));
    }
    Json(result)
}
pub async fn oauth_start(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let Some(provider) = body["provider"]
        .as_str()
        .and_then(SubscriptionProvider::parse)
    else {
        return error(StatusCode::BAD_REQUEST, "unknown provider");
    };
    match state
        .oauth
        .start(provider, body["redirect_uri"].as_str().map(str::to_owned))
        .await
    {
        Ok(a) => Json(json!(a)).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}
pub async fn oauth_status(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.oauth.status(&id).await {
        Some(a) => {
            if a.status == "complete" {
                state.refresh_models().await;
                if let Some(id) = &a.account_id {
                    let worker = state.clone();
                    let id = id.clone();
                    tokio::spawn(async move {
                        let _ = worker.quotas.refresh(&id).await;
                    });
                }
            }
            Json(json!(a)).into_response()
        }
        None => error(StatusCode::NOT_FOUND, "OAuth attempt not found"),
    }
}
pub async fn oauth_callback(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    match state
        .oauth
        .complete(
            body["id"].as_str().unwrap_or(""),
            body["code"].as_str().unwrap_or(""),
            body["state"].as_str().unwrap_or(""),
        )
        .await
    {
        Ok(a) => {
            state.refresh_models().await;
            let worker = state.clone();
            let id = a.id.clone();
            tokio::spawn(async move {
                let _ = worker.quotas.refresh(&id).await;
            });
            Json(json!({"account":a})).into_response()
        }
        Err(e) => error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

// CLIProxyAPI management discovery uses the same sanitized account data.
pub async fn auth_files(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(
        json!({"files":state.accounts.list().await.into_iter().map(|a|json!({"id":a.id,"auth_index":a.id,"name":format!("{}.json",a.id),"label":a.name,"provider":a.provider,"type":a.provider,"email":a.email,"disabled":!a.enabled,"status":a.status,"status_message":a.last_error,"created_at":a.created_at,"updated_at":a.updated_at})).collect::<Vec<_>>()}),
    )
}
pub async fn delete_auth_file(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Params>,
) -> Response {
    delete_account(
        State(state),
        Path(
            q.name
                .unwrap_or_default()
                .trim_end_matches(".json")
                .to_owned(),
        ),
    )
    .await
}
pub async fn patch_auth_file(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body["name"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches(".json")
        .to_owned();
    update_account(
        State(state),
        Path(id),
        Json(AccountPatch {
            name: None,
            enabled: Some(!body["disabled"].as_bool().unwrap_or(false)),
            ..AccountPatch::default()
        }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn account_listing_reports_reserve_state_per_quota_shape() {
        use crate::subscriptions::{QuotaSnapshot, QuotaWindow};
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = crate::settings::Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let window = |id: &str, remaining: f64, reset: &str| QuotaWindow {
            id: id.into(),
            label: id.into(),
            remaining_percent: Some(remaining),
            reset_at: Some(reset.into()),
            used: None,
            limit: Some(100.0),
        };
        let snapshot = |id: &str, windows: Vec<QuotaWindow>, error: Option<&str>| QuotaSnapshot {
            account_id: id.into(),
            provider: SubscriptionProvider::Claude,
            windows,
            updated_at: crate::subscriptions::now(),
            error: error.map(Into::into),
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        };
        let later = "2099-01-01T00:00:00Z";
        let past = "2020-01-01T00:00:00Z";
        // name, stored quota (windows, error), expected measured, expected reached
        let cases: Vec<(&str, Option<(Vec<QuotaWindow>, Option<&str>)>, Value, bool)> = vec![
            (
                "low",
                Some((vec![window("seven-day", 3.0, later)], None)),
                json!(true),
                true,
            ),
            (
                "five-hour-low",
                Some((
                    vec![
                        window("seven-day", 40.0, later),
                        window("five-hour", 0.0, later),
                    ],
                    None,
                )),
                json!(true),
                false,
            ),
            (
                "scoped",
                Some((vec![window("seven-day-opus", 0.0, later)], None)),
                json!(false),
                false,
            ),
            (
                "reset",
                Some((vec![window("seven-day", 1.0, past)], None)),
                json!(true),
                false,
            ),
            (
                "failed",
                Some((vec![], Some("provider request failed"))),
                Value::Null,
                false,
            ),
            ("unread", None, Value::Null, false),
        ];
        let mut ids = vec![];
        for (name, quota, _, _) in &cases {
            let account = state
                .accounts
                .import(
                    json!({"type":"claude","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            state
                .accounts
                .update(&account.id, None, None, Some(Some(5.0)), None)
                .await
                .unwrap();
            if let Some((windows, error)) = quota {
                state
                    .quotas
                    .insert_for_test(snapshot(&account.id, windows.clone(), *error))
                    .await;
            }
            ids.push(account.id);
        }
        let Json(body) = accounts(State(state.clone())).await;
        for ((name, _, measured, reached), id) in cases.iter().zip(&ids) {
            let row = body["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == *id)
                .unwrap();
            assert_eq!(row["reserve_measured"], *measured, "{name}");
            assert_eq!(row["reserve_reached"], *reached, "{name}");
        }
        // A change returns the same enriched shape as the listing.
        let response = update_account(
            State(state.clone()),
            Path(ids[0].clone()),
            Json(AccountPatch {
                name: Some("Renamed".into()),
                ..AccountPatch::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["account"]["name"], "Renamed");
        assert_eq!(body["account"]["reserve_measured"], true);
        assert_eq!(body["account"]["reserve_reached"], true);
        assert!(body["account"].get("hidden").is_none());
    }

    #[test]
    fn account_patch_rejects_browser_visibility() {
        assert!(
            serde_json::from_value::<AccountPatch>(serde_json::json!({"hidden": true})).is_err()
        );
    }

    #[tokio::test]
    async fn verification_redirect_exposes_challenge_only_as_no_store_location() {
        let challenge = "https://accounts.google.com/signin/continue?challenge=private-fixture";
        let response = verification_redirect(Some(challenge.into()));
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[header::LOCATION], challenge);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );

        let missing = verification_redirect(None);
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(!missing.headers().contains_key(header::LOCATION));
        assert_eq!(missing.headers()[header::CACHE_CONTROL], "no-store");
        let body = missing.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&body).contains("private-fixture"));
    }
}
