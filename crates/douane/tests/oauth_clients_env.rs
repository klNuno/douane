//! A Google OAuth client set by environment variables is read-only through
//! the management API. This binary holds one test: it changes the process
//! environment, which other tests must not see.
use axum::{body::Body, http::Request};
use douane::{settings::Config, state::AppState};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn environment_clients_win_and_cannot_be_changed_through_the_api() {
    // SAFETY: this test binary has no other test and no other thread reads
    // the environment while it is changed.
    unsafe {
        std::env::set_var("DOUANE_GEMINI_CLIENT_ID", "env-id");
        std::env::set_var("DOUANE_GEMINI_CLIENT_SECRET", "env-client-secret");
        std::env::remove_var("DOUANE_ANTIGRAVITY_CLIENT_ID");
        std::env::remove_var("DOUANE_ANTIGRAVITY_CLIENT_SECRET");
        std::env::set_var("DOUANE_ANTIGRAVITY_PROJECT_ID", "env-project");
        std::env::remove_var("DOUANE_GEMINI_PROJECT_ID");
    }
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("oauth-clients.json"),
        json!({"gemini":{"client_id":"file-id","client_secret":"file-client-secret"}}).to_string(),
    )
    .unwrap();
    let mut config = Config::default();
    config.server.auth_dir = directory.path().join("auth");
    config.server.database = directory.path().join("calls.sqlite");
    let app = douane::router(AppState::new(config, None).unwrap());
    let call = |method: &str, uri: &str, body: Option<Value>| {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
            .unwrap();
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status().as_u16();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        }
    };

    let (status, text) = call("GET", "/admin/api/oauth-clients", None).await;
    assert_eq!(status, 200);
    assert!(!text.contains("client-secret"));
    let body: Value = serde_json::from_str(&text).unwrap();
    let gemini = &body["clients"][1];
    assert_eq!(gemini["provider"], "gemini");
    assert_eq!(gemini["source"], "env");
    assert_eq!(gemini["client_id"], "env-id");
    assert_eq!(gemini["read_only"], true);
    assert_eq!(gemini["has_secret"], true);

    for (method, body) in [
        (
            "PUT",
            Some(json!({"client_id":"x","client_secret":"put-client-secret"})),
        ),
        ("DELETE", None),
    ] {
        let (status, text) = call(method, "/admin/api/oauth-clients/gemini", body).await;
        assert_eq!(status, 409, "{text}");
        assert!(!text.contains("client-secret"));
        assert!(
            text.contains("DOUANE_GEMINI_* environment variables"),
            "{text}"
        );
    }
    let saved = std::fs::read_to_string(directory.path().join("oauth-clients.json")).unwrap();
    assert!(saved.contains("file-id"));

    // A project variable alone sets only the project: the client stays
    // unconfigured (none) and editable.
    let antigravity = &body["clients"][0];
    assert_eq!(antigravity["source"], "none");
    assert_eq!(antigravity["read_only"], false);
    assert_eq!(antigravity["project_id"], "env-project");
    let (status, text) = call(
        "PUT",
        "/admin/api/oauth-clients/antigravity",
        Some(json!({"client_id":"file-antigravity","client_secret":"put-client-secret"})),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let view: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(view["source"], "file");
    assert_eq!(view["read_only"], false);
    assert_eq!(view["project_id"], "env-project");
}
