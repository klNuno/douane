//! Google OAuth clients change at runtime through the management API.
use axum::{body::Body, http::Request};
use douane::{settings::Config, state::AppState};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::Path;
use tower::ServiceExt;

/// The developer's own `DOUANE_ANTIGRAVITY_*` and `DOUANE_GEMINI_*`
/// variables would change the clients these tests see. Every test in this
/// binary holds this lock first; the first one removes those variables, so
/// no other test is running while the environment changes.
async fn clean_environment() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static CLEAN: std::sync::Once = std::sync::Once::new();
    let guard = LOCK.lock().await;
    CLEAN.call_once(|| {
        let names: Vec<_> = std::env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| {
                name.to_str().is_some_and(|name| {
                    name.starts_with("DOUANE_ANTIGRAVITY_") || name.starts_with("DOUANE_GEMINI_")
                })
            })
            .collect();
        for name in names {
            // SAFETY: the other tests of this binary wait for the lock above
            // before they read the environment.
            unsafe { std::env::remove_var(name) };
        }
    });
    guard
}

fn app(directory: &Path) -> axum::Router {
    let mut config = Config::default();
    config.server.auth_dir = directory.join("auth");
    config.server.database = directory.join("calls.sqlite");
    douane::router(AppState::new(config, None).unwrap())
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (u16, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn json_call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let (status, text) = call(app, method, uri, body).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn oauth_flag(app: &axum::Router, id: &str) -> Value {
    let (_, providers) = json_call(app, "GET", "/admin/api/providers", None).await;
    providers["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["id"] == id)
        .map_or(Value::Null, |provider| provider["oauth"].clone())
}

/// Only the current user can read the file: mode 0600 on Unix, one
/// owner-only ACL entry without inheritance on Windows.
fn assert_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("icacls")
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        let acl = String::from_utf8_lossy(&output.stdout).to_string();
        let entries: Vec<&str> = acl.lines().filter(|line| line.contains(":(")).collect();
        assert_eq!(entries.len(), 1, "{acl}");
        assert!(
            entries[0].contains(&std::env::var("USERNAME").unwrap()),
            "{acl}"
        );
        assert!(!entries[0].contains("(I)"), "{acl}");
    }
}

#[tokio::test]
async fn google_clients_are_read_changed_and_reset_without_restart_or_secret_exposure() {
    let _environment = clean_environment().await;
    let directory = tempfile::tempdir().unwrap();
    let app = app(directory.path());
    let file = directory.path().join("oauth-clients.json");

    let (status, text) = call(&app, "GET", "/admin/api/oauth-clients", None).await;
    assert_eq!(status, 200);
    let body: Value = serde_json::from_str(&text).unwrap();
    // Douane ships no client: both providers start with none.
    assert_eq!(
        body["clients"][1],
        json!({"provider":"gemini","source":"none","client_id":null,"has_secret":false,"project_id":null,"origin":null,"read_only":false})
    );
    assert_eq!(
        body["clients"][0],
        json!({"provider":"antigravity","source":"none","client_id":null,"has_secret":false,"project_id":null,"origin":null,"read_only":false})
    );
    assert!(!text.contains("GOCSPX-"));
    assert_eq!(oauth_flag(&app, "antigravity").await, false);
    assert!(!file.exists());

    // Gemini becomes available at once once a client is set.
    let (status, text) = call(
        &app,
        "PUT",
        "/admin/api/oauth-clients/gemini",
        Some(json!({"client_id":"fixture-id","client_secret":"fixture-client-secret","project_id":"fixture-project"})),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert!(!text.contains("fixture-client-secret"));
    let view: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        view,
        json!({"provider":"gemini","source":"file","client_id":"fixture-id","has_secret":true,"project_id":"fixture-project","origin":null,"read_only":false})
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved["gemini"]["client_secret"], "fixture-client-secret");
    assert_private(&file);
    let (_, text) = call(&app, "GET", "/admin/api/oauth-clients", None).await;
    assert!(!text.contains("fixture-client-secret"));
    assert!(text.contains("fixture-project"));
    // Sign-in uses the new client without a restart.
    let (status, attempt) = gemini_start(&app).await;
    assert_eq!(status, 200, "{attempt}");
    assert!(
        attempt["url"]
            .as_str()
            .unwrap()
            .contains("client_id=fixture-id")
    );
    assert!(!attempt.to_string().contains("fixture-client-secret"));

    // A custom Antigravity client is set, including the public id, and removed.
    let (status, view) = json_call(
        &app,
        "PUT",
        "/admin/api/oauth-clients/antigravity",
        Some(json!({"client_id":"custom-id","client_secret":"custom-client-secret"})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(view["source"], "file");
    assert_eq!(oauth_flag(&app, "antigravity").await, true);
    let saved: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved["gemini"]["client_id"], "fixture-id");
    assert_private(&file);
    let (status, view) =
        json_call(&app, "DELETE", "/admin/api/oauth-clients/antigravity", None).await;
    assert_eq!(status, 200);
    assert_eq!(view["source"], "none");
    assert_eq!(oauth_flag(&app, "antigravity").await, false);

    // Errors name the problem and never echo a submitted secret.
    for (method, uri, body, expected) in [
        (
            "PUT",
            "/admin/api/oauth-clients/claude",
            Some(json!({"client_id":"x","client_secret":"leaked-secret-1"})),
            404,
        ),
        (
            "PUT",
            "/admin/api/oauth-clients/nope",
            Some(json!({"client_id":"x","client_secret":"leaked-secret-2"})),
            404,
        ),
        ("DELETE", "/admin/api/oauth-clients/claude", None, 404),
        (
            "PUT",
            "/admin/api/oauth-clients/gemini",
            Some(json!({"client_secret":"leaked-secret-3"})),
            400,
        ),
        (
            "PUT",
            "/admin/api/oauth-clients/gemini",
            Some(json!({"client_id":"new-id"})),
            400,
        ),
        (
            "PUT",
            "/admin/api/oauth-clients/gemini",
            Some(json!({"client_id":"id","client_secret":"leaked\nsecret-4"})),
            400,
        ),
    ] {
        let (status, text) = call(&app, method, uri, body).await;
        assert_eq!(status, expected, "{uri} {text}");
        assert!(!text.contains("leaked"), "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
    }

    let (status, view) = json_call(&app, "DELETE", "/admin/api/oauth-clients/gemini", None).await;
    assert_eq!(status, 200);
    assert_eq!(view["source"], "none");
    let (status, body) = gemini_start(&app).await;
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "provider OAuth client is not configured"
    );
    assert!(!file.exists());
}

#[tokio::test]
async fn a_provider_without_a_client_is_refused_before_its_body_is_checked() {
    let _environment = clean_environment().await;
    let directory = tempfile::tempdir().unwrap();
    let app = app(directory.path());
    for provider in ["claude", "unknown"] {
        for body in [json!([]), json!({"client_id":7})] {
            let (status, text) = call(
                &app,
                "PUT",
                &format!("/admin/api/oauth-clients/{provider}"),
                Some(body),
            )
            .await;
            assert_eq!(status, 404, "{provider}: {text}");
            assert!(text.contains("this provider has no configurable OAuth client"));
        }
        let (status, _) = call(
            &app,
            "DELETE",
            &format!("/admin/api/oauth-clients/{provider}"),
            None,
        )
        .await;
        assert_eq!(status, 404);
    }
    // A Google provider checks its body.
    let (status, text) = call(
        &app,
        "PUT",
        "/admin/api/oauth-clients/gemini",
        Some(json!([])),
    )
    .await;
    assert_eq!(status, 400);
    assert!(text.contains("request body must be a JSON object"));
}

#[tokio::test]
async fn the_antigravity_cli_credential_file_imports_through_the_management_api() {
    let _environment = clean_environment().await;
    let directory = tempfile::tempdir().unwrap();
    let app = app(directory.path());
    // A known project skips discovery, so the test stays offline.
    let record = json!({
        "token": {
            "access_token": "fixture-access",
            "token_type": "Bearer",
            "refresh_token": "fixture-refresh",
            "expiry": "2099-01-01T00:00:00Z",
            "expires_in": 3599
        },
        "auth_method": "consumer",
        "id_token_email": "fixture@example.test",
        "project_id": "fixture-project"
    });
    let (status, body) = json_call(
        &app,
        "POST",
        "/admin/api/accounts/import",
        Some(json!({"files":[{"name":"","content":record.to_string()}]})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["imported"], 1);
    assert_eq!(body["accounts"][0]["provider"], "antigravity");
    assert_eq!(body["accounts"][0]["email"], "fixture@example.test");
    assert!(!body.to_string().contains("fixture-access"));
}

#[tokio::test]
async fn hand_edits_to_the_client_file_show_on_the_next_read_and_bad_files_change_nothing() {
    let _environment = clean_environment().await;
    let directory = tempfile::tempdir().unwrap();
    let app = app(directory.path());
    let file = directory.path().join("oauth-clients.json");
    assert_eq!(oauth_flag(&app, "antigravity").await, false);

    // A hand-edited client applies to the provider list and to sign-in
    // without reading the client list first.
    std::fs::write(
        &file,
        json!({"antigravity":{"client_id":"hand-signin-id","client_secret":"hand-secret"}})
            .to_string(),
    )
    .unwrap();
    assert_eq!(oauth_flag(&app, "antigravity").await, true);
    let (status, attempt) = json_call(
        &app,
        "POST",
        "/admin/api/oauth/start",
        Some(json!({"provider":"antigravity"})),
    )
    .await;
    assert_eq!(status, 200, "{attempt}");
    assert!(
        attempt["url"]
            .as_str()
            .unwrap()
            .contains("client_id=hand-signin-id"),
        "{attempt}"
    );

    // A custom id without its secret, written by hand: shown as is, and
    // sign-in is off because douane has no secret for it.
    std::fs::write(
        &file,
        json!({"antigravity":{"client_id":"hand-id"}}).to_string(),
    )
    .unwrap();
    assert_eq!(oauth_flag(&app, "antigravity").await, false);
    let (status, body) = json_call(&app, "GET", "/admin/api/oauth-clients", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["clients"][0]["source"], "file");
    assert_eq!(body["clients"][0]["client_id"], "hand-id");
    assert_eq!(body["clients"][0]["has_secret"], false);
    assert_eq!(oauth_flag(&app, "antigravity").await, false);

    // A broken file is ignored: the clients and the file stay as they were,
    // and changes are refused.
    std::fs::write(&file, b"{broken").unwrap();
    let (status, broken) = json_call(&app, "GET", "/admin/api/oauth-clients", None).await;
    assert_eq!(status, 200);
    assert_eq!(broken, body);
    for (method, body) in [
        (
            "PUT",
            Some(json!({"client_id":"x","client_secret":"leaked-secret"})),
        ),
        ("DELETE", None),
    ] {
        let (status, text) = call(&app, method, "/admin/api/oauth-clients/antigravity", body).await;
        assert_eq!(status, 500, "{text}");
        assert!(text.contains("oauth-clients.json cannot be read"), "{text}");
        assert!(!text.contains("leaked"), "{text}");
    }
    let (_, after) = json_call(&app, "GET", "/admin/api/oauth-clients", None).await;
    assert_eq!(after, body);
    assert_eq!(std::fs::read(&file).unwrap(), b"{broken");
}

#[tokio::test]
async fn antigravity_detection_reports_clear_errors_without_a_network_call() {
    let _environment = clean_environment().await;
    let directory = tempfile::tempdir().unwrap();
    let app = app(directory.path());
    let file = directory.path().join("oauth-clients.json");

    // Only Antigravity is detectable.
    let (status, text) = call(&app, "POST", "/admin/api/oauth-clients/gemini/detect", None).await;
    assert_eq!(status, 404, "{text}");
    assert!(text.contains("only available for antigravity"), "{text}");

    // A path that does not exist is reported as not found; no file is written.
    let missing = directory.path().join("no-such-agy.exe");
    let (status, text) = call(
        &app,
        "POST",
        "/admin/api/oauth-clients/antigravity/detect",
        Some(json!({ "path": missing.display().to_string() })),
    )
    .await;
    assert_eq!(status, 404, "{text}");
    assert!(text.contains("no Antigravity CLI"), "{text}");

    // A binary without douane's client id is no match, and no secret of any
    // kind reaches the response. The scan runs; no Google call is made.
    let binary = directory.path().join("agy-no-id.bin");
    std::fs::write(&binary, b"random binary content without the client id").unwrap();
    let (status, text) = call(
        &app,
        "POST",
        "/admin/api/oauth-clients/antigravity/detect",
        Some(json!({ "path": binary.display().to_string() })),
    )
    .await;
    assert_eq!(status, 422, "{text}");
    assert!(
        text.contains("does not hold douane's Antigravity client"),
        "{text}"
    );
    assert!(!text.contains("GOCSPX-"), "{text}");
    assert!(!file.exists());
    assert_eq!(oauth_flag(&app, "antigravity").await, false);
}

async fn gemini_start(app: &axum::Router) -> (u16, Value) {
    json_call(
        app,
        "POST",
        "/admin/api/oauth/start",
        Some(json!({"provider":"gemini"})),
    )
    .await
}
