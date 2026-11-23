use super::{AppState, Json, State, StatusCode, error, json};
use super::{Params, SubscriptionProvider};
use crate::state::{CodexEntry, Model, SpeedCapability, SpeedOption, catalog_rank, tier_id};
use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub async fn models(State(state): State<Arc<AppState>>) -> Json<Value> {
    model_listing(&state).await
}
async fn model_listing(state: &AppState) -> Json<Value> {
    let models = state.models.read().await.clone();
    let listings = state
        .model_listings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Json(json!({"models":models,"listings":listings}))
}
#[derive(Default, Deserialize)]
pub struct RefreshModels {
    /// One pool (a provider or a custom endpoint route); every pool without it.
    pub pool: Option<String>,
}
pub async fn refresh_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RefreshModels>,
) -> Json<Value> {
    state.refresh_models_in(query.pool.as_deref()).await;
    model_listing(&state).await
}
pub async fn set_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(enabled) = body["enabled"].as_bool() else {
        return error(StatusCode::BAD_REQUEST, "enabled must be a boolean");
    };
    match set_models_enabled(&state, std::slice::from_ref(&id), enabled).await {
        Some(refusal) => refusal,
        None => Json(json!({"id":id,"enabled":enabled})).into_response(),
    }
}
#[derive(Deserialize)]
pub struct ModelsPatch {
    pub ids: Vec<String>,
    pub enabled: bool,
}
/// Turns several models on or off in one configuration write, such as a
/// whole provider from the Models page.
pub async fn set_models(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ModelsPatch>,
) -> Response {
    match set_models_enabled(&state, &body.ids, body.enabled).await {
        Some(refusal) => refusal,
        None => Json(json!({"ids":body.ids,"enabled":body.enabled})).into_response(),
    }
}
/// Saves the change, or returns the refusal to send.
async fn set_models_enabled(state: &AppState, ids: &[String], enabled: bool) -> Option<Response> {
    let _write = state.config_write.lock().await;
    let mut config = state.config.read().await.clone();
    {
        let models = state.models.read().await;
        if ids.iter().any(|id| !models.iter().any(|m| &m.id == id)) {
            return Some(error(StatusCode::NOT_FOUND, "model not found"));
        }
    }
    for id in ids {
        if enabled {
            config.disabled_models.remove(id);
        } else {
            config.disabled_models.insert(id.clone());
        }
    }
    if let Some(path) = &state.config_path
        && config.save(path).await.is_err()
    {
        return Some(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot save configuration",
        ));
    }
    *state.config.write().await = config;
    for model in state
        .models
        .write()
        .await
        .iter_mut()
        .filter(|m| ids.contains(&m.id))
    {
        model.enabled = enabled;
    }
    None
}
pub async fn public_models(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(query): Query<Params>,
) -> Response {
    match state.clients.authenticate(&headers) {
        Ok(client) => Json(list_models(&state, &query, Some(&client)).await).into_response(),
        Err(refusal) => error(StatusCode::UNAUTHORIZED, refusal.message()),
    }
}
/// Codex needs its native model rows to negotiate service tiers.
pub async fn codex_models(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    match state.clients.authenticate(&headers) {
        Ok(client) => match state.codex_catalog(&client).await {
            Some(models) => Json(json!({"models": models})).into_response(),
            None => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Codex model catalog is temporarily unavailable",
            ),
        },
        Err(refusal) => error(StatusCode::UNAUTHORIZED, refusal.message()),
    }
}
/// The native Muse CLI needs provider metadata and bare ids from Meta only.
pub async fn muse_models(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    public_models(
        State(state),
        headers,
        Query(Params {
            provider: Some("meta".into()),
            ids: Some("upstream".into()),
            ..Default::default()
        }),
    )
    .await
}
/// The Gemini API model list, for Antigravity's own clients pointed at douane
/// as a custom gateway: the Antigravity models this client may use, by their
/// provider ids.
pub async fn gemini_models(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let client = match state.clients.authenticate(&headers) {
        Ok(client) => client,
        Err(refusal) => return error(StatusCode::UNAUTHORIZED, refusal.message()),
    };
    let mut rows = state.antigravity_catalog_for(&client, None).await;
    rows.sort_by_key(|m| catalog_rank(m.provider, &m.upstream_id));
    let models: Vec<_> = rows
        .iter()
        .map(|m| {
            json!({
                "name": format!("models/{}", m.upstream_id),
                "displayName": m.name,
                "inputTokenLimit": m.context_length,
                "outputTokenLimit": m.max_output_tokens,
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })
        })
        .collect();
    Json(json!({ "models": models })).into_response()
}
/// The unscoped management view of the `/v1/models` catalog.
pub async fn management_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<Params>,
) -> Json<Value> {
    Json(list_models(&state, &query, None).await)
}
/// Antigravity models are listed only for the Gemini API (`antigravity`),
/// the one route that serves them: Google bans an account used outside
/// Antigravity's own agent.
async fn list_models(
    state: &AppState,
    query: &Params,
    client: Option<&crate::clients::Client>,
) -> Value {
    let accounts = state.accounts.list().await;
    let codex_accounts = state
        .accounts
        .enabled_subscriptions(SubscriptionProvider::Codex)
        .await;
    let claude_accounts = state
        .accounts
        .enabled_subscriptions(SubscriptionProvider::Claude)
        .await;
    let disabled = state.config.read().await.disabled_models.clone();
    let upstream = query.ids.as_deref() == Some("upstream");
    let models = state.models.read().await;
    let (codex, _) = state.codex_entries(&models, &codex_accounts, client, &disabled);
    let mut listed: Vec<_> = models
        .iter()
        .filter(|m| {
            let pool = m.route.as_deref().unwrap_or(m.provider.as_str());
            let matches_provider = query.provider.as_ref().is_none_or(|selection| {
                selection
                    .split(',')
                    .any(|p| p == pool || p == m.provider.as_str())
            });
            m.enabled
                && matches_provider
                && match m.provider {
                    SubscriptionProvider::Antigravity => false,
                    SubscriptionProvider::Codex => codex.contains_key(&m.id),
                    SubscriptionProvider::Claude => claude_accounts.iter().any(|account| {
                        account.offers_model(&m.upstream_id)
                            && client.is_none_or(|client| client.may_use(account))
                    }),
                    _ => accounts.iter().any(|a| {
                        a.enabled
                            && a.provider == m.provider
                            && a.route == m.route
                            && client.is_none_or(|c| {
                                c.may_use_summary(a)
                                    && (a.models.is_empty() || a.models.contains(&m.upstream_id))
                            })
                    }),
                }
        })
        .collect();
    if upstream {
        // The provider's own view: its ids, and its order. A client that
        // starts on the first model of a list, as the Grok CLI does, then
        // starts on the provider's default.
        listed.sort_by_key(|m| {
            (
                m.provider.as_str(),
                crate::state::catalog_rank(m.provider, &m.upstream_id),
            )
        });
    }
    let data: Vec<_> = listed
        .into_iter()
        .map(|m| public_model(m, upstream, codex.get(&m.id)))
        .collect();
    json!({"object":"list","data":data})
}
/// One row of the client model list. `context_window` and `reasoning_efforts`
/// repeat `context_length` and `thinking.levels` under the names the Grok CLI
/// reads from an endpoint's list: without them it assumes a 256,000 token
/// window and offers no effort. `upstream` names the model as its provider
/// does, for a client that knows its own models by those names.
fn public_model(model: &Model, upstream: bool, codex: Option<&CodexEntry>) -> Value {
    let id = if upstream && !model.upstream_id.is_empty() {
        &model.upstream_id
    } else {
        &model.id
    };
    let mut value = json!({"id":id,"object":"model","created":0,"owned_by":model.route.as_deref().unwrap_or(model.provider.as_str()),"name":model.name,"label":model.name,"context_length":model.context_length,"context_window":model.context_length,"max_output_tokens":model.max_output_tokens,"thinking":model.thinking,"reasoning_efforts":reasoning_efforts(model.thinking.as_ref()),"supported_endpoint_types":["anthropic","openai","openai-response"],"supports_websockets":true});
    if let Some(metadata) = &model.metadata {
        value["metadata"] = metadata.clone();
    }
    project_capabilities(&mut value, model, codex);
    value
}
fn project_capabilities(row: &mut Value, model: &Model, codex: Option<&CodexEntry>) {
    let mut capabilities = serde_json::Map::new();
    if let Some(effort) = effort_capability(model.thinking.as_ref()) {
        capabilities.insert("effort".into(), effort);
    }
    if let Some(capability) = &model.capabilities {
        if let Some(adaptive) = capability.adaptive_thinking {
            capabilities.insert("adaptive_thinking".into(), json!(adaptive));
        }
        if let Some(speed) = &capability.speed {
            capabilities.insert("speed".into(), json!(speed));
            let tiers: Vec<_> = speed
                .options
                .iter()
                .filter(|option| {
                    option.id != "standard" && speed.default.as_ref() != Some(&option.id)
                })
                .map(|option| json!({"id":option.id,"name":option.label}))
                .collect();
            if !tiers.is_empty() {
                row["service_tiers"] = json!(tiers);
            }
        }
    }
    if let Some(native) = codex.and_then(|entry| entry.native.as_ref()) {
        if let Some(tiers) = native.get("service_tiers") {
            row["service_tiers"] = tiers.clone();
        }
        if let Some(speed) = codex_speed(native) {
            capabilities.insert("speed".into(), json!(speed));
        }
    }
    if !capabilities.is_empty() {
        row["capabilities"] = Value::Object(capabilities);
    }
}

fn codex_speed(native: &serde_json::Map<String, Value>) -> Option<SpeedCapability> {
    let default = native.get("default_service_tier").and_then(tier_id);
    let tiers = |field| {
        native
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    // Other service tiers can change cost or scheduling without promising speed.
    let service_speed =
        tiers("service_tiers").filter(|tier| matches!(tier_id(tier), Some("default" | "priority")));
    let mut options: Vec<SpeedOption> = Vec::new();
    for tier in service_speed.chain(tiers("additional_speed_tiers")) {
        let Some(id) = tier_id(tier) else { continue };
        if !options.iter().any(|option| option.id == id) {
            options.push(SpeedOption {
                id: id.to_owned(),
                label: tier["name"]
                    .as_str()
                    .or_else(|| tier["label"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| speed_label(id)),
            });
        }
    }
    if let Some(id) =
        default.filter(|id| *id == "default" && !options.iter().any(|option| option.id == *id))
    {
        options.insert(
            0,
            SpeedOption {
                id: id.to_owned(),
                label: speed_label(id),
            },
        );
    }
    let default = default
        .filter(|id| options.iter().any(|option| option.id == *id))
        .map(str::to_owned);
    (!options.is_empty()).then_some(SpeedCapability { options, default })
}

fn speed_label(id: &str) -> String {
    match id {
        "default" => "Standard".into(),
        "fast" | "priority" => "Fast".into(),
        _ => id.to_owned(),
    }
}

fn effort_capability(thinking: Option<&Value>) -> Option<Value> {
    let thinking = thinking?;
    let levels: Vec<_> = thinking["levels"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if levels.is_empty() {
        return None;
    }
    let mut effort = json!({"levels":levels});
    if let Some(default) = thinking["default"]
        .as_str()
        .filter(|level| levels.contains(level))
    {
        effort["default"] = json!(default);
    }
    Some(effort)
}

/// A model's effort levels as the Grok CLI lists them: highest first, the
/// thinking default (or `high`) flagged. Null without levels.
fn reasoning_efforts(thinking: Option<&Value>) -> Option<Value> {
    let thinking = thinking?;
    let levels: Vec<&str> = thinking
        .get("levels")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if levels.is_empty() {
        return None;
    }
    let default = thinking["default"].as_str().unwrap_or("high");
    Some(
        levels
            .iter()
            .rev()
            .map(|level| {
                let label = match *level {
                    "xhigh" => "Extra high".to_owned(),
                    other => {
                        let mut letters = other.chars();
                        letters
                            .next()
                            .map(|first| first.to_uppercase().chain(letters).collect())
                            .unwrap_or_default()
                    }
                };
                let mut entry = json!({"value": level, "label": label});
                if *level == default {
                    entry["default"] = json!(true);
                }
                entry
            })
            .collect(),
    )
}
pub async fn auth_file_models(
    State(state): State<Arc<AppState>>,
    Query(q): Query<Params>,
) -> Json<Value> {
    let id = q.name.unwrap_or_default();
    let account = state.accounts.get(id.trim_end_matches(".json")).await;
    let models: Vec<Value> = state
        .models
        .read()
        .await
        .iter()
        .filter(|m| account.as_ref().is_some_and(|a| a.provider == m.provider))
        .map(|m| json!({"id":m.id,"display_name":m.name,"context_length":m.context_length}))
        .collect();
    Json(json!({"models":models}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_rows_carry_the_window_and_efforts_the_grok_cli_reads() {
        let model = |thinking: Option<Value>| crate::state::Model {
            id: "xai/grok-test".into(),
            provider: SubscriptionProvider::Xai,
            name: "Grok Test".into(),
            context_length: Some(500_000),
            max_output_tokens: None,
            enabled: true,
            upstream_id: "grok-test".into(),
            thinking,
            ..crate::state::Model::default()
        };
        let scaled = model(Some(
            json!({"levels":["none","low","high","xhigh"],"zero_allowed":true}),
        ));
        let row = public_model(&scaled, false, None);
        assert_eq!(row["id"], "xai/grok-test");
        assert_eq!(public_model(&scaled, true, None)["id"], "grok-test");
        assert_eq!(row["context_length"], 500_000);
        assert_eq!(row["context_window"], 500_000);
        assert_eq!(
            row["thinking"]["levels"],
            json!(["none", "low", "high", "xhigh"])
        );
        assert_eq!(
            row["reasoning_efforts"],
            json!([
                {"value":"xhigh","label":"Extra high"},
                {"value":"high","label":"High","default":true},
                {"value":"low","label":"Low"},
                {"value":"none","label":"None"}
            ])
        );
        // A model without a level scale offers no effort; without a thinking
        // default, `high` is flagged when on the scale and none otherwise.
        for thinking in [None, Some(json!({"min":1024,"max":128000}))] {
            assert_eq!(
                public_model(&model(thinking), false, None)["reasoning_efforts"],
                Value::Null
            );
        }
        let unflagged = public_model(
            &model(Some(json!({"levels":["minimal","low"]}))),
            false,
            None,
        );
        assert_eq!(
            unflagged["reasoning_efforts"],
            json!([{"value":"low","label":"Low"},{"value":"minimal","label":"Minimal"}])
        );
        let explicit_default = public_model(
            &model(Some(json!({"levels":["low","high"],"default":"low"}))),
            false,
            None,
        );
        assert_eq!(
            explicit_default["reasoning_efforts"],
            json!([{"value":"high","label":"High"},{"value":"low","label":"Low","default":true}])
        );
    }

    #[tokio::test]
    async fn a_provider_view_lists_its_own_ids_in_its_own_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::settings::Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        state
            .accounts
            .import(
                json!({"type":"xai","access_token":"fixture-access","account_id":"fixture-identity"}),
                None,
            )
            .await
            .unwrap();
        state.refresh_models().await;
        let ids = |ids: Option<&str>| {
            let query = Params {
                provider: Some("xai".into()),
                ids: ids.map(Into::into),
                ..Params::default()
            };
            let state = state.clone();
            async move {
                let Json(body) = management_models(State(state), Query(query)).await;
                body["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["id"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }
        };
        let catalog: Vec<String> = crate::state::catalog()
            .iter()
            .filter(|model| model.provider == SubscriptionProvider::Xai)
            .map(|model| model.id.clone())
            .collect();
        assert!(catalog.len() > 1);
        assert_eq!(ids(Some("upstream")).await, catalog);
        let mut prefixed: Vec<String> = catalog.iter().map(|id| format!("xai/{id}")).collect();
        prefixed.sort();
        assert_eq!(ids(None).await, prefixed);
    }
}
