use crate::{relay::error, state::AppState, subscriptions::SubscriptionProvider};
use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::Ordering},
};

mod accounts;
mod backup;
mod clients;
mod models;
mod quotas;

pub use accounts::*;
pub use backup::*;
pub use clients::*;
pub use models::*;
pub use quotas::*;

static ASSETS: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../dash/build");
fn asset_response(path: &str) -> Response {
    let Some(file) = ASSETS.get_file(path) else {
        return error(StatusCode::NOT_FOUND, "asset not found");
    };
    let mime = if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CACHE_CONTROL,
                if path.contains("immutable/") {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        file.contents(),
    )
        .into_response()
}
pub async fn page() -> Response {
    asset_response(if ASSETS.get_file("index.html").is_some() {
        "index.html"
    } else {
        "200.html"
    })
}
pub async fn asset(Path(path): Path<String>) -> Response {
    asset_response(&format!("_app/{path}"))
}
pub async fn health() -> Json<Value> {
    Json(
        json!({"status":"ok","version":env!("CARGO_PKG_VERSION"),"service":"douane","websocket":true}),
    )
}

pub async fn guard(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    if let Some(expected) = &state.management_token {
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .or_else(|| {
                request
                    .headers()
                    .get("x-management-key")
                    .and_then(|v| v.to_str().ok())
            })
            .or_else(|| {
                request
                    .headers()
                    .get("x-douane-admin")
                    .and_then(|v| v.to_str().ok())
            });
        if presented != Some(expected.as_str()) {
            return error(
                StatusCode::UNAUTHORIZED,
                "management authentication required",
            );
        }
    }
    if request.method() != axum::http::Method::GET {
        if let Some(origin) = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        {
            let host = request
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let matches = reqwest::Url::parse(origin)
                .ok()
                .is_some_and(|u| u.authority() == host);
            if !matches {
                return error(
                    StatusCode::FORBIDDEN,
                    "management writes require the same origin",
                );
            }
        }
    }
    next.run(request).await
}

#[derive(Default, Deserialize)]
pub struct Params {
    pub limit: Option<usize>,
    pub provider: Option<String>,
    pub name: Option<String>,
    pub state: Option<String>,
    pub ids: Option<String>,
}
pub async fn calls(State(state): State<Arc<AppState>>, Query(q): Query<Params>) -> Json<Value> {
    Json(json!({"calls":state.ledger.calls(q.limit.unwrap_or(100)).await}))
}
pub async fn summary(State(state): State<Arc<AppState>>) -> Json<Value> {
    let accounts = state.accounts.list().await;
    let l = &state.ledger;
    let client_api_base_url = state.config.read().await.server.client_api_base_url.clone();
    Json(
        json!({"accounts":accounts.len(),"enabled_accounts":accounts.iter().filter(|a|a.enabled).count(),"providers":accounts.iter().map(|a|a.provider).collect::<BTreeSet<_>>().len(),"requests":l.requests.load(Ordering::Relaxed),"errors":l.errors.load(Ordering::Relaxed),"input_tokens":l.input_tokens.load(Ordering::Relaxed),"output_tokens":l.output_tokens.load(Ordering::Relaxed),"cache_read_tokens":l.cache_read_tokens.load(Ordering::Relaxed),"cache_write_tokens":l.cache_write_tokens.load(Ordering::Relaxed),"in_flight":l.in_flight.load(Ordering::Relaxed),"dropped_logs":l.dropped_logs.load(Ordering::Relaxed),"uptime_seconds":l.started.elapsed().as_secs(),"version":env!("CARGO_PKG_VERSION"),"websocket":true,"client_api_base_url":client_api_base_url,"clients":state.clients.list().len()}),
    )
}
pub async fn settings(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!(state.config.read().await.settings))
}
pub async fn update_settings(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<Value>,
) -> Response {
    let _write = state.config_write.lock().await;
    let mut config = state.config.read().await.clone();
    let mut value = serde_json::to_value(&config.settings).unwrap();
    let Some(fields) = patch.as_object() else {
        return error(StatusCode::BAD_REQUEST, "settings must be an object");
    };
    for (key, v) in fields {
        value[key] = v.clone();
    }
    let settings: crate::settings::Settings = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid settings"),
    };
    if let Err(e) = settings.validate() {
        return error(StatusCode::BAD_REQUEST, &e.to_string());
    }
    config.settings = settings.clone();
    if let Some(path) = &state.config_path {
        if config.save(path).await.is_err() {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot save configuration",
            );
        }
    }
    *state.config.write().await = config;
    Json(json!(settings)).into_response()
}
pub async fn compatible_config(State(state): State<Arc<AppState>>) -> Json<Value> {
    let config = state.config.read().await;
    Json(
        json!({"port":config.server.listen.rsplit(':').next().and_then(|p|p.parse::<u16>().ok()),"routing":{"strategy":config.settings.strategy},"access":{"api-keys":[]},"providers":[],"websocket-auth":false,"request-retry":1,"quota-exceeded":{"switch-project":true},"version":env!("CARGO_PKG_VERSION")}),
    )
}
pub async fn version() -> Json<Value> {
    Json(json!({"version":env!("CARGO_PKG_VERSION"),"latest-version":env!("CARGO_PKG_VERSION")}))
}
