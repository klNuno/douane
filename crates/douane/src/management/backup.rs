use super::{AppState, Json, State, StatusCode, error, json};
use crate::{clients::Client, settings::Settings};
use axum::{
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Bundle format written by `export` and read by `restore`.
const EXPORT_FORMAT: &str = "douane-export";
const EXPORT_VERSION: u64 = 1;
/// Most accounts one restore accepts.
const MAX_RESTORE_ACCOUNTS: usize = 1000;

/// The whole configuration as one JSON file: settings, disabled models and
/// every stored account with its tokens. Refresh tokens rotate, so an
/// account restored elsewhere must leave this instance.
pub async fn export(State(state): State<Arc<AppState>>) -> Response {
    let (settings, disabled_models) = {
        let config = state.config.read().await;
        (config.settings.clone(), config.disabled_models.clone())
    };
    let exported_at = crate::subscriptions::now();
    let date = exported_at.get(..10).unwrap_or("export").to_owned();
    let body = json!({
        "format": EXPORT_FORMAT,
        "version": EXPORT_VERSION,
        "exported_at": exported_at,
        "settings": settings,
        "disabled_models": disabled_models,
        "accounts": state.accounts.export().await,
        "clients": state.clients.list(),
    });
    let mut response = Json(body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    if let Ok(value) = header::HeaderValue::from_str(&format!(
        "attachment; filename=\"douane-export-{date}.json\""
    )) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// Restores the bundle itself or an envelope opting into its settings.
pub async fn restore(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let plan = match RestorePlan::decode(&body) {
        Ok(plan) => plan,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let (results, renamed, missing) = match state
        .accounts
        .restore_batch(plan.accounts, |remapped| {
            if let Some(clients) = &plan.clients {
                state.clients.restore(clients.clone(), remapped, true)?;
            }
            Ok(())
        })
        .await
    {
        Ok(result) => result,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let mut accounts = vec![];
    let mut errors = vec![];
    for (index, result) in results.into_iter().enumerate() {
        match result {
            Ok(account) => accounts.push(account),
            Err(e) => errors.push(json!({
                "index": index,
                "name": plan.accounts[index].get("name").and_then(Value::as_str),
                "error": e.to_string(),
            })),
        }
    }
    let restored_clients = match plan
        .clients
        .map(|list| {
            if client_references_missing(&list, &missing) {
                return Err(
                    "a restored client references an account that could not be saved".into(),
                );
            }
            state.clients.restore(list, &renamed, false)
        })
        .transpose()
    {
        Ok(count) => count.unwrap_or(0),
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let accepted = !accounts.is_empty() || errors.is_empty();
    let mut settings_applied = false;
    if let Some(configuration) = plan.configuration.filter(|_| accepted) {
        if save_restored_settings(&state, configuration).await.is_err() {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot save configuration",
            );
        }
        settings_applied = true;
    }
    super::accounts::refresh_imported_accounts(&state, &accounts).await;
    let status = if accepted {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(json!({
            "restored": accounts.len(),
            "accounts": accounts,
            "errors": errors,
            "settings": settings_applied,
            "clients": restored_clients,
        })),
    )
        .into_response()
}

struct RestorePlan<'a> {
    accounts: &'a [Value],
    clients: Option<Vec<Client>>,
    configuration: Option<RestoredSettings>,
}

impl<'a> RestorePlan<'a> {
    fn decode(body: &'a Value) -> Result<Self, String> {
        let (bundle, apply_settings) = restore_envelope(body)?;
        if bundle.get("format").and_then(Value::as_str) != Some(EXPORT_FORMAT) {
            return Err("not a douane export: format must be \"douane-export\"".into());
        }
        if bundle.get("version").and_then(Value::as_u64) != Some(EXPORT_VERSION) {
            return Err(format!(
                "unsupported export version {}; this douane reads version {EXPORT_VERSION}",
                bundle.get("version").unwrap_or(&Value::Null)
            ));
        }
        let accounts = bundle
            .get("accounts")
            .and_then(Value::as_array)
            .ok_or("accounts must be a list")?;
        if accounts.len() > MAX_RESTORE_ACCOUNTS {
            return Err(format!(
                "restore at most {MAX_RESTORE_ACCOUNTS} accounts at once"
            ));
        }
        let clients = bundle
            .get("clients")
            .filter(|v| !v.is_null())
            .map(|list| serde_json::from_value(list.clone()))
            .transpose()
            .map_err(|_| "invalid clients in the export")?;
        let configuration = apply_settings
            .then(|| RestoredSettings::decode(bundle))
            .transpose()?;
        Ok(Self {
            accounts,
            clients,
            configuration,
        })
    }
}

fn restore_envelope(body: &Value) -> Result<(&Value, bool), String> {
    let Some(bundle) = body.get("bundle") else {
        return Ok((body, false));
    };
    let apply = match body.get("settings") {
        None => false,
        Some(Value::Bool(apply)) => *apply,
        Some(_) => return Err("settings beside bundle must be true or false".into()),
    };
    Ok((bundle, apply))
}

struct RestoredSettings {
    settings: Settings,
    disabled_models: BTreeSet<String>,
}

impl RestoredSettings {
    fn decode(bundle: &Value) -> Result<Self, String> {
        let settings: Settings =
            serde_json::from_value(bundle.get("settings").cloned().unwrap_or(json!({})))
                .map_err(|_| "invalid settings in the export")?;
        settings.validate().map_err(|error| error.to_string())?;
        let disabled_models =
            serde_json::from_value(bundle.get("disabled_models").cloned().unwrap_or(json!([])))
                .map_err(|_| "disabled_models must be a list of model ids")?;
        Ok(Self {
            settings,
            disabled_models,
        })
    }
}

async fn save_restored_settings(
    state: &AppState,
    restored: RestoredSettings,
) -> anyhow::Result<()> {
    let _write = state.config_write.lock().await;
    let mut config = state.config.read().await.clone();
    config.settings = restored.settings;
    config.disabled_models = restored.disabled_models;
    if let Some(path) = &state.config_path {
        config.save(path).await?;
    }
    *state.config.write().await = config;
    Ok(())
}

fn client_references_missing(clients: &[Client], missing: &BTreeSet<String>) -> bool {
    if missing.is_empty() {
        return false;
    }
    clients.iter().any(|client| {
        client.access.accounts.iter().any(|id| missing.contains(id))
            || client.account_shares.keys().any(|id| missing.contains(id))
            || client
                .borrows
                .iter()
                .any(|(id, borrow)| missing.contains(id) || missing.contains(&borrow.home))
    })
}
