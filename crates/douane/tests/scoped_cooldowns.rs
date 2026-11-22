//! Provider refusals must retain the scope of the limit they describe.
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use douane::{settings::Config, state::AppState, subscriptions::AccountStore};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::net::TcpListener;

struct Upstream {
    calls: AtomicUsize,
    google: bool,
    global: bool,
}

async fn upstream(
    State(mock): State<Arc<Upstream>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    mock.calls.fetch_add(1, Ordering::Relaxed);
    let model = body["model"].as_str().unwrap();
    if mock.global || model.contains("fable") || model.contains("argon") {
        let error = if mock.google {
            json!({"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED",
                 "metadata":if mock.global { json!({"quotaResetDelay":"172800s"}) } else { json!({"model":model,"quotaResetDelay":"172800s"}) }},
                {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"60s"}
            ]}})
        } else {
            json!({"error":{"type":"rate_limit_error","message":"fixture limit"}})
        };
        let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response();
        response.headers_mut().insert(
            "retry-after",
            if mock.google { "60" } else { "172800" }.parse().unwrap(),
        );
        if !mock.google {
            for (key, value) in [
                ("anthropic-ratelimit-unified-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    if mock.global {
                        "seven_day"
                    } else {
                        "seven_day_overage_included"
                    },
                ),
                (
                    "anthropic-ratelimit-unified-7d-utilization",
                    if mock.global { "1.0" } else { "0.51" },
                ),
            ] {
                response.headers_mut().insert(key, value.parse().unwrap());
            }
        }
        return response;
    }
    if mock.google {
        Json(json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}})).into_response()
    } else {
        Json(json!({"id":"msg_fixture","type":"message","role":"assistant","model":model,
            "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})).into_response()
    }
}

async fn exercise(provider: &str, responses: bool, global: bool) {
    let google = provider != "claude";
    let directory = tempfile::tempdir().unwrap();
    let mock = Arc::new(Upstream {
        calls: AtomicUsize::new(0),
        google,
        global,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/v1/messages", post(upstream))
                .route("/v1internal:generateContent", post(upstream))
                .with_state(mock.clone()),
        )
        .into_future(),
    );
    let mut config = Config::default();
    config.server.auth_dir = directory.path().join("accounts");
    config.server.database = directory.path().join("calls.sqlite");
    let state = AppState::new(config.clone(), None).unwrap();
    let limited = if google {
        "gemini-4-argon"
    } else {
        "claude-fable-5-2"
    };
    let other = if google {
        "gemini-3.8-flash"
    } else {
        "claude-opus-5-5"
    };
    let account = state
        .accounts
        .import(
            json!({"type":provider,"access_token":"fixture-only",
        "project_id":"fixture-project","base_url":url,"models":[limited,other]}),
            None,
        )
        .await
        .unwrap();
    let (_, key) = state
        .clients
        .create(douane::clients::ClientPatch {
            name: Some("fixture".into()),
            ..Default::default()
        })
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let gateway = tokio::spawn(axum::serve(listener, douane::router(state.clone())).into_future());
    let client = reqwest::Client::new();
    let request = |model: &str| {
        let (path, body): (String, Value) = if provider == "antigravity" {
            // Antigravity accounts answer only Antigravity's own agent, through
            // the Gemini API.
            (
                format!("/v1beta/models/{model}:generateContent"),
                json!({"contents":[{"role":"user","parts":[{"text":"fixture"}]}]}),
            )
        } else if google && responses {
            (
                "/v1/responses".into(),
                json!({"model":format!("{provider}/{model}"),"input":"fixture"}),
            )
        } else if google {
            (
                "/v1/chat/completions".into(),
                json!({"model":format!("{provider}/{model}"),"messages":[{"role":"user","content":"fixture"}]}),
            )
        } else {
            (
                "/v1/messages".into(),
                json!({"model":model,"messages":[{"role":"user","content":"fixture"}],"max_tokens":8}),
            )
        };
        client
            .post(format!("{base}{path}"))
            .bearer_auth(&key)
            .header("x-douane-provider", provider)
            .json(&body)
    };
    assert_eq!(
        request(limited).send().await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let second = request(other).send().await.unwrap();
    if global {
        assert!(
            !second.status().is_success(),
            "a global quota must still block every model"
        );
        assert_eq!(mock.calls.load(Ordering::Relaxed), 1);
    } else {
        assert_eq!(
            second.status(),
            StatusCode::OK,
            "a model quota must leave other models eligible: {}",
            second.text().await.unwrap()
        );
        assert!(!request(limited).send().await.unwrap().status().is_success());
        assert_eq!(
            mock.calls.load(Ordering::Relaxed),
            2,
            "the limited model must not reach upstream again"
        );
        let reopened = AccountStore::open(&config.server.auth_dir).unwrap();
        let saved: Value = serde_json::from_slice(
            &std::fs::read(config.server.auth_dir.join(format!("{}.json", account.id))).unwrap(),
        )
        .unwrap();
        let until = time::OffsetDateTime::parse(
            saved["scoped_cooldowns"][0]["until"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert!(
            until > time::OffsetDateTime::now_utc() + time::Duration::hours(47),
            "a shorter Retry-After must not mask the weekly delay"
        );
        let provider = account.provider;
        assert!(reopened.eligible(provider, Some(limited)).await.is_empty());
        assert_eq!(
            reopened.eligible(provider, Some(other)).await.len(),
            1,
            "scope must survive restart"
        );
    }
    gateway.abort();
    server.abort();
}

#[tokio::test]
async fn fable_weekly_refusal_does_not_block_opus() {
    exercise("claude", false, false).await;
}

#[tokio::test]
async fn argon_weekly_refusal_does_not_block_other_google_models() {
    for responses in [false, true] {
        exercise("gemini", responses, false).await;
    }
    exercise("antigravity", false, false).await;
}

#[tokio::test]
async fn global_quota_refusals_still_block_every_model() {
    for provider in ["claude", "gemini", "antigravity"] {
        exercise(provider, false, true).await;
    }
}
