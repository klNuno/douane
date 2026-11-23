//! The Gemini API route serves Antigravity's own clients through a synthetic
//! Cloud Code upstream.
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use douane::{settings::Config, state::AppState};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

#[derive(Default)]
struct Mock {
    received: Mutex<Vec<Value>>,
    /// The bearer each request carried, in order.
    tokens: Mutex<Vec<String>>,
    /// A token this upstream answers with 429.
    limited: Mutex<Option<String>>,
}

impl Mock {
    fn limit(&self, headers: &HeaderMap) -> Option<Response> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        self.tokens.lock().unwrap().push(token.clone());
        (self.limited.lock().unwrap().as_deref() == Some(token.as_str())).then(|| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(
                    json!({"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[
                    {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"30s"}]}}),
                ),
            )
                .into_response()
        })
    }
}

struct Fixture {
    mock: Arc<Mock>,
    state: Arc<AppState>,
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

async fn generate(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(limited) = mock.limit(&headers) {
        return limited;
    }
    let reply = if blocked(&body) { refusal() } else { answer() };
    mock.received.lock().unwrap().push(body);
    Json(json!({"response": reply, "traceId": "synthetic"})).into_response()
}

async fn stream(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(limited) = mock.limit(&headers) {
        return limited;
    }
    if blocked(&body) {
        mock.received.lock().unwrap().push(body);
        let event = json!({ "response": refusal() });
        return (
            [("content-type", "text/event-stream")],
            Body::from(format!("data: {event}\r\n\r\n")),
        )
            .into_response();
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
    ([("content-type", "text/event-stream")], Body::from(data)).into_response()
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
        state
            .accounts
            .import(
                json!({"type":"gemini","access_token":"synthetic-gemini-access",
                    "base_url":upstream,"project_id":"synthetic-gemini-project",
                    "models":["synthetic-gemini","synthetic-pro"]}),
                None,
            )
            .await
            .unwrap();
        state.refresh_models().await;
        let (url, gate_task) = serve(douane::router(state.clone())).await;
        Self {
            mock,
            state,
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
    assert_eq!(received[0]["project"], "synthetic-project");
    assert_eq!(received[0]["request"], request(Some("LOW")));
    assert_eq!(
        *fixture.mock.tokens.lock().unwrap(),
        ["Bearer synthetic-access-0"]
    );
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
    let empty = generate_on(&fixture, "antigravity/", &request(None)).await;
    assert_eq!(empty.status(), 404);
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

#[tokio::test]
async fn a_signed_turn_waits_for_the_account_that_signed_it() {
    let fixture = Fixture::with(&["synthetic-pro"], 2).await;
    let mut body = request(None);
    body["sessionId"] = json!("synthetic-session-b");
    assert_eq!(
        generate_on(&fixture, "synthetic-pro", &body).await.status(),
        200
    );
    let signer = fixture.mock.tokens.lock().unwrap()[0].clone();
    *fixture.mock.limited.lock().unwrap() = Some(signer.clone());
    body["contents"] = json!([
        {"role":"user","parts":[{"text":"say pong"}]},
        {"role":"model","parts":[{"text":"pong","thoughtSignature":"U1lOVEhFVElD"}]},
        {"role":"user","parts":[{"text":"again"}]}
    ]);
    let refused = generate_on(&fixture, "synthetic-pro", &body).await;
    assert_eq!(refused.status(), 429);
    let first_delay = refused.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!((29..=30).contains(&first_delay));
    let sent = fixture.mock.tokens.lock().unwrap().len();
    let local = generate_on(&fixture, "synthetic-pro", &body).await;
    assert_eq!(local.status(), 429);
    let local_delay = local.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(local_delay <= first_delay && local_delay >= 28);
    let tokens = fixture.mock.tokens.lock().unwrap();
    assert_eq!(tokens.len(), sent);
    assert!(tokens.iter().all(|t| *t == signer), "{tokens:?}");
}

async fn post_v1(
    fixture: &Fixture,
    path: &str,
    body: Value,
    provider: Option<&str>,
) -> (StatusCode, String) {
    let mut request = fixture
        .client
        .post(format!("{}/v1/{path}", fixture.url))
        .bearer_auth(&fixture.key)
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    if let Some(provider) = provider {
        request = request.header("x-douane-provider", provider);
    }
    let response = request.send().await.unwrap();
    (response.status(), response.text().await.unwrap())
}

fn chat(model: &str) -> Value {
    json!({"model":model,"max_tokens":64,"messages":[{"role":"user","content":"say pong"}]})
}

fn responses(model: &str) -> Value {
    json!({"model":model,"input":"say pong"})
}

#[tokio::test]
async fn chat_messages_and_responses_never_reach_an_antigravity_account() {
    let fixture = Fixture::new().await;
    let cases = [
        // A prefix, a bare id only Antigravity lists, and a header naming it.
        ("chat/completions", chat("antigravity/synthetic-pro"), None),
        ("chat/completions", chat("synthetic-flash-low"), None),
        (
            "chat/completions",
            chat("synthetic-pro"),
            Some("antigravity"),
        ),
        ("messages", chat("antigravity/synthetic-pro"), None),
        ("messages", chat("synthetic-flash-low"), None),
        (
            "messages/count_tokens",
            chat("antigravity/synthetic-pro"),
            None,
        ),
        ("responses", responses("antigravity/synthetic-pro"), None),
        ("responses", responses("synthetic-flash-high"), None),
        (
            "responses/compact",
            responses("antigravity/synthetic-pro"),
            None,
        ),
    ];
    for (path, body, provider) in cases {
        let model = body["model"].clone();
        let (status, text) = post_v1(&fixture, path, body, provider).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} {model}: {text}");
        assert!(
            text.contains("Antigravity accounts serve only Antigravity's own agent"),
            "{path} {model}: {text}"
        );
    }
    assert!(fixture.mock.tokens.lock().unwrap().is_empty());
    assert!(fixture.mock.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn responses_websocket_never_reaches_an_antigravity_account() {
    let fixture = Fixture::new().await;
    let url = format!(
        "{}/v1/responses",
        fixture.url.replacen("http://", "ws://", 1)
    );
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", fixture.key).parse().unwrap(),
    );
    let (mut socket, _) = connect_async(request).await.unwrap();
    let mut body = responses("antigravity/synthetic-pro");
    body["type"] = json!("response.create");
    socket
        .send(Message::Text(body.to_string().into()))
        .await
        .unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the gateway must answer the request")
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(reply["type"], "error", "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Antigravity accounts serve only")),
        "{reply}"
    );
    assert!(fixture.mock.tokens.lock().unwrap().is_empty());
    assert!(fixture.mock.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_model_antigravity_shares_goes_to_the_other_provider_from_chat() {
    // Both pools serve synthetic-pro; the Antigravity pool has two accounts.
    let fixture = Fixture::with(&["synthetic-pro"], 2).await;
    for _ in 0..3 {
        let (status, text) =
            post_v1(&fixture, "chat/completions", chat("synthetic-pro"), None).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
    let tokens = fixture.mock.tokens.lock().unwrap();
    assert_eq!(tokens.len(), 3, "{tokens:?}");
    assert!(
        tokens.iter().all(|t| t == "Bearer synthetic-gemini-access"),
        "{tokens:?}"
    );
}

#[tokio::test]
async fn a_saved_antigravity_default_provider_leaves_bare_ids_to_the_other_pools() {
    let fixture = Fixture::with(&["synthetic-pro"], 2).await;
    fixture.state.config.write().await.settings.default_provider = Some("antigravity".into());
    let (status, text) = post_v1(&fixture, "chat/completions", chat("synthetic-pro"), None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let tokens = fixture.mock.tokens.lock().unwrap();
    assert_eq!(*tokens, ["Bearer synthetic-gemini-access"], "{tokens:?}");
}

#[tokio::test]
async fn client_model_lists_leave_antigravity_to_the_gemini_api() {
    let fixture = Fixture::new().await;
    for query in [
        "",
        "?provider=antigravity",
        "?provider=antigravity,gemini",
        "?ids=upstream",
        "?provider=antigravity&ids=upstream",
    ] {
        let listed: Value = fixture
            .client
            .get(format!("{}/v1/models{query}", fixture.url))
            .bearer_auth(&fixture.key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let rows = listed["data"].as_array().unwrap();
        assert!(
            rows.iter().all(|m| m["owned_by"] != "antigravity"
                && !m["id"].as_str().unwrap().starts_with("antigravity/")),
            "{query}: {listed}"
        );
        if !query.contains("provider=antigravity&") && query != "?provider=antigravity" {
            assert!(
                rows.iter().any(|m| m["owned_by"] == "gemini"),
                "{query}: {listed}"
            );
        }
    }
    let gemini: Value = fixture
        .client
        .get(format!("{}/v1beta/models", fixture.url))
        .bearer_auth(&fixture.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        gemini["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "models/synthetic-flash-low"),
        "{gemini}"
    );
}
