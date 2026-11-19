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

fn blocked(body: &Value) -> bool {
    body.pointer("/request/contents/0/parts/0/text") == Some(&json!("blocked prompt"))
}

fn refusal() -> Value {
    json!({"promptFeedback":{"blockReason":"PROHIBITED_CONTENT"},"usageMetadata":{"promptTokenCount":5}})
}

fn answer() -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":[
        {"thought":true,"text":"synthetic hidden"},
        {"functionCall":{"name":"synthetic_tool","args":{"n":1}},"thoughtSignature":"SYNTHETIC_CALL"},
        {"text":"pong"}
    ]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3}})
}

async fn generate(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> impl IntoResponse {
    let reply = if blocked(&body) { refusal() } else { answer() };
    mock.received.lock().unwrap().push(body);
    Json(json!({"response": reply, "traceId": "synthetic"}))
}

async fn stream(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> impl IntoResponse {
    if blocked(&body) {
        mock.received.lock().unwrap().push(body);
        let event = json!({ "response": refusal() });
        return (
            [("content-type", "text/event-stream")],
            Body::from(format!("data: {event}\r\n\r\n")),
        );
    }
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
        Self::with(
            &[
                "synthetic-flash-low",
                "synthetic-flash-high",
                "synthetic-pro",
            ],
            1,
        )
        .await
    }

    async fn with(models: &[&str], accounts: usize) -> Self {
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
        for n in 0..accounts {
            let project = if n == 0 {
                "synthetic-project".to_owned()
            } else {
                format!("synthetic-project-{n}")
            };
            state
                .accounts
                .import(
                    json!({"type":"antigravity","access_token":format!("synthetic-access-{n}"),
                        "base_url":upstream,"project_id":project,"models":models}),
                    None,
                )
                .await
                .unwrap();
        }
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

async fn generate_on(fixture: &Fixture, model: &str, body: &Value) -> reqwest::Response {
    fixture
        .client
        .post(format!(
            "{}/v1beta/models/{model}:generateContent",
            fixture.url
        ))
        .bearer_auth(&fixture.key)
        .json(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_provider_prefix_cannot_leave_the_antigravity_pool() {
    let fixture = Fixture::new().await;
    let other = generate_on(&fixture, "gemini-cli/synthetic-pro", &request(None)).await;
    assert_eq!(other.status(), 404);
    assert!(fixture.mock.received.lock().unwrap().is_empty());
    let own = generate_on(&fixture, "antigravity/synthetic-pro", &request(None)).await;
    assert_eq!(own.status(), 200);
    assert_eq!(
        fixture.mock.received.lock().unwrap()[0]["model"],
        "synthetic-pro"
    );
}

#[tokio::test]
async fn the_leveled_variant_wins_over_a_listed_family_and_a_preview_suffix() {
    let fixture = Fixture::with(
        &["synthetic-pro", "synthetic-pro-low", "synthetic-pro-high"],
        1,
    )
    .await;
    for (model, level, want) in [
        ("synthetic-pro", "LOW", "synthetic-pro-low"),
        ("synthetic-pro-preview", "HIGH", "synthetic-pro-high"),
        ("synthetic-pro-preview", "MINIMAL", "synthetic-pro"),
    ] {
        let response = generate_on(&fixture, model, &request(Some(level))).await;
        assert_eq!(response.status(), 200, "{model} {level}");
        let received = fixture.mock.received.lock().unwrap();
        assert_eq!(received.last().unwrap()["model"], want, "{model} {level}");
    }
}

#[tokio::test]
async fn one_client_session_stays_on_one_account() {
    let fixture = Fixture::with(&["synthetic-pro"], 2).await;
    let mut body = request(None);
    body["sessionId"] = json!("synthetic-session-a");
    for _ in 0..4 {
        assert_eq!(
            generate_on(&fixture, "synthetic-pro", &body).await.status(),
            200
        );
    }
    let received = fixture.mock.received.lock().unwrap();
    assert!(
        received
            .iter()
            .all(|r| r["project"] == received[0]["project"]),
        "{:?}",
        received
            .iter()
            .map(|r| r["project"].clone())
            .collect::<Vec<_>>()
    );
    assert!(
        received
            .iter()
            .all(|r| r["request"].get("prompt_cache_key").is_none())
    );
}

#[tokio::test]
async fn google_refusals_reach_the_client_as_google_wrote_them() {
    let fixture = Fixture::new().await;
    let mut body = request(None);
    body["contents"][0]["parts"][0]["text"] = json!("blocked prompt");
    let plain = generate_on(&fixture, "synthetic-pro", &body).await;
    assert_eq!(plain.status(), 200);
    assert_eq!(plain.json::<Value>().await.unwrap(), refusal());
    let streamed = fixture
        .client
        .post(format!(
            "{}/v1beta/models/synthetic-pro:streamGenerateContent?alt=sse",
            fixture.url
        ))
        .bearer_auth(&fixture.key)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(streamed.status(), 200);
    let text = streamed.text().await.unwrap();
    let event: Value =
        serde_json::from_str(text.trim_end().strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(event, refusal(), "{text}");
}
