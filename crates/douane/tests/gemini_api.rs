//! The Gemini API route serves Antigravity's own clients through a synthetic
//! Cloud Code upstream.
use axum::{Json, Router, body::Body, extract::State, response::IntoResponse, routing::post};
use douane::{settings::Config, state::AppState};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Default)]
struct Mock {
    received: Mutex<Vec<Value>>,
}

struct Fixture {
    mock: Arc<Mock>,
    url: String,
    key: String,
    client: reqwest::Client,
    tasks: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn serve(router: Router) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }),
    )
}

fn answer() -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":[
        {"thought":true,"text":"synthetic hidden"},
        {"functionCall":{"name":"synthetic_tool","args":{"n":1}},"thoughtSignature":"SYNTHETIC_CALL"},
        {"text":"pong"}
    ]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3}})
}

async fn generate(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> impl IntoResponse {
    mock.received.lock().unwrap().push(body);
    Json(json!({"response": answer(), "traceId": "synthetic"}))
}

async fn stream(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> impl IntoResponse {
    mock.received.lock().unwrap().push(body);
    let full = answer();
    let mut data = String::new();
    for part in full["candidates"][0]["content"]["parts"]
        .as_array()
        .unwrap()
    {
        let event =
            json!({"response":{"candidates":[{"content":{"role":"model","parts":[part]}}]}});
        data.push_str(&format!("data: {event}\r\n\r\n"));
    }
    let last = json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":full["usageMetadata"]}});
    data.push_str(&format!("data: {last}\r\n\r\n"));
    ([("content-type", "text/event-stream")], Body::from(data))
}

impl Fixture {
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        let (upstream, up_task) = serve(
            Router::new()
                .route("/v1internal:generateContent", post(generate))
                .route("/v1internal:streamGenerateContent", post(stream))
                .with_state(mock.clone()),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = dir.path().join("auth");
        config.server.database = dir.path().join("calls.sqlite");
        let state = AppState::new(config, None).unwrap();
        let (_, key) = state
            .clients
            .create(douane::clients::ClientPatch {
                name: Some("tests".into()),
                ..Default::default()
            })
            .unwrap();
        state
            .accounts
            .import(
                json!({"type":"antigravity","access_token":"synthetic-access","base_url":upstream,
                    "project_id":"synthetic-project",
                    "models":["synthetic-flash-low","synthetic-flash-high","synthetic-pro"]}),
                None,
            )
            .await
            .unwrap();
        state.refresh_models().await;
        let (url, gate_task) = serve(douane::router(state.clone())).await;
        Self {
            mock,
            url,
            key,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            tasks: vec![up_task, gate_task],
            _dir: dir,
        }
    }
}

/// The request Antigravity's ACP server sends to a custom gateway.
fn request(level: Option<&str>) -> Value {
    let mut body = json!({
        "contents":[{"role":"user","parts":[{"text":"say pong"}]}],
        "systemInstruction":{"parts":[{"text":"<identity>\nYou are Antigravity."}]},
        "tools":[{"functionDeclarations":[{"name":"synthetic_tool","description":"a tool","parameters":{"type":"object"}}]}],
        "toolConfig":{"functionCallingConfig":{"mode":"AUTO"}},
        "generationConfig":{"maxOutputTokens":65535,"thinkingConfig":{"includeThoughts":true}},
        "sessionId":"1234567890"
    });
    if let Some(level) = level {
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"] = json!(level);
    }
    body
}

#[tokio::test]
async fn lists_antigravity_models_for_a_known_key_only() {
    let fixture = Fixture::new().await;
    let response = fixture
        .client
        .get(format!("{}/v1beta/models?pageSize=1000", fixture.url))
        .bearer_auth(&fixture.key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let mut names: Vec<&str> = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "models/synthetic-flash-high",
            "models/synthetic-flash-low",
            "models/synthetic-pro"
        ]
    );
    assert_eq!(
        body["models"][0]["supportedGenerationMethods"],
        json!(["generateContent", "streamGenerateContent"])
    );
    let refused = fixture
        .client
        .get(format!("{}/v1beta/models", fixture.url))
        .bearer_auth("wrong-key")
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
}

#[tokio::test]
async fn generate_content_wraps_the_native_request_and_returns_the_bare_answer() {
    let fixture = Fixture::new().await;
    let response = fixture
        .client
        .post(format!(
            "{}/v1beta/models/synthetic-pro:generateContent",
            fixture.url
        ))
        .header("x-goog-api-key", &fixture.key)
        .json(&request(None))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body, answer());
    let received = fixture.mock.received.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["model"], "synthetic-pro");
    assert_eq!(received[0]["project"], "synthetic-project");
    // The client's request goes up as sent, without the path's model or stream.
    assert_eq!(received[0]["request"], request(None));
}

#[tokio::test]
async fn stream_resolves_the_leveled_model_and_unwraps_every_event() {
    let fixture = Fixture::new().await;
    let response = fixture
        .client
        .post(format!(
            "{}/v1beta/models/synthetic-flash:streamGenerateContent?alt=sse",
            fixture.url
        ))
        .bearer_auth(&fixture.key)
        .json(&request(Some("LOW")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    let events: Vec<Value> = text
        .split("\r\n\r\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert_eq!(events.len(), 4, "{text}");
    assert!(events.iter().all(|e| e.get("response").is_none()));
    assert_eq!(
        events[1]["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
        "SYNTHETIC_CALL"
    );
    assert_eq!(events[3]["candidates"][0]["finishReason"], "STOP");
    let received = fixture.mock.received.lock().unwrap();
    assert_eq!(received[0]["model"], "synthetic-flash-low");
    assert_eq!(received[0]["request"], request(Some("LOW")));
}

#[tokio::test]
async fn other_gemini_methods_and_missing_keys_are_refused() {
    let fixture = Fixture::new().await;
    let unknown = fixture
        .client
        .post(format!(
            "{}/v1beta/models/synthetic-pro:embedContent",
            fixture.url
        ))
        .bearer_auth(&fixture.key)
        .json(&request(None))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let anonymous = fixture
        .client
        .post(format!(
            "{}/v1beta/models/synthetic-pro:generateContent",
            fixture.url
        ))
        .json(&request(None))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    assert!(fixture.mock.received.lock().unwrap().is_empty());
}
