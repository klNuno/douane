//! Native Codex discovery must follow each subscription's current model list.
use axum::{
    Json, Router,
    extract::{Query, State},
    http::HeaderMap,
    routing::get,
};
use douane::{settings::Config, state::AppState};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

type ReadGate = (oneshot::Sender<()>, oneshot::Receiver<()>);

struct Fixture {
    state: Arc<AppState>,
    replies: Arc<Mutex<HashMap<String, Value>>>,
    read_gate: Arc<Mutex<Option<ReadGate>>>,
    accounts: Vec<String>,
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

impl Fixture {
    async fn new(replies: Vec<Value>) -> Self {
        let replies: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(
            replies
                .into_iter()
                .enumerate()
                .map(|(i, reply)| (i.to_string(), reply))
                .collect(),
        ));
        let read_gate: Arc<Mutex<Option<ReadGate>>> = Arc::default();
        let router = Router::new()
            .route(
                "/models",
                get(
                    |State((replies, gate)): State<(
                        Arc<Mutex<HashMap<String, Value>>>,
                        Arc<Mutex<Option<ReadGate>>>,
                    )>,
                     Query(query): Query<HashMap<String, String>>,
                     headers: HeaderMap| async move {
                        assert_eq!(query["client_version"], "0.170.0");
                        assert_eq!(headers["originator"], "codex_cli_rs");
                        assert_eq!(headers["user-agent"], "codex_cli_rs/0.170.0");
                        let id = headers["chatgpt-account-id"].to_str().unwrap();
                        assert_eq!(headers["authorization"], format!("Bearer fixture-{id}"));
                        let gate = gate.lock().unwrap().take();
                        if let Some((started, release)) = gate {
                            started.send(()).unwrap();
                            release.await.unwrap();
                        }
                        Json(replies.lock().unwrap()[id].clone())
                    },
                ),
            )
            .with_state((replies.clone(), read_gate.clone()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = dir.path().join("auth");
        config.server.database = dir.path().join("calls.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut accounts = vec![];
        for i in 0..replies.lock().unwrap().len() {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex",
                "access_token":format!("fixture-{i}"),"account_id":i.to_string(),
                "base_url":base}),
                    None,
                )
                .await
                .unwrap();
            accounts.push(account.id);
        }
        Self {
            state,
            replies,
            read_gate,
            accounts,
            tasks: vec![task],
            _dir: dir,
        }
    }

    async fn ids(&self) -> Vec<String> {
        self.state
            .models
            .read()
            .await
            .iter()
            .map(|m| m.id.clone())
            .collect()
    }
}

fn listing(id: &str, context: u64) -> Value {
    json!({"models":[{"slug":id,"display_name":"Native model", "context_window":context,
        "supported_in_api":true,"supported_reasoning_levels":[{"effort":"low"},{"effort":"ultra"}]},
        {}]})
}

#[tokio::test]
async fn native_catalog_only_offers_tiers_supported_by_the_clients_subscriptions() {
    let priority = json!({"id":"priority", "name":"Fast", "description":"Fixture"});
    let model = |slug: &str, tiers: Value| {
        json!({
            "slug":slug, "supported_in_api":false, "service_tiers":tiers,
            "additional_speed_tiers":["fast","ultrafast"], "default_service_tier":"ultrafast"
        })
    };
    let f = Fixture::new(vec![
        json!({"models":[model("gpt-6.2-sol", json!([priority, "ultrafast"]))]}),
        json!({"error":"unavailable"}),
    ])
    .await;
    let create_key = |name, access| {
        f.state
            .clients
            .create(douane::clients::ClientPatch {
                name: Some(String::from(name)),
                access,
                ..Default::default()
            })
            .unwrap()
            .1
    };
    let first = create_key(
        "First subscription",
        Some(douane::clients::Access {
            all: false,
            accounts: [f.accounts[0].clone()].into(),
            ..Default::default()
        }),
    );
    let both = create_key("Both subscriptions", None);
    let second = create_key(
        "Second subscription",
        Some(douane::clients::Access {
            all: false,
            accounts: [f.accounts[1].clone()].into(),
            ..Default::default()
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/codex/models", listener.local_addr().unwrap());
    let generic_url = url.replace("/v1/codex/models", "/v1/models");
    let mut f = f;
    let router = douane::router(f.state.clone());
    f.tasks.push(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    let http = reqwest::Client::new();
    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        http.head(&url)
            .bearer_auth(&first)
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    f.state.refresh_models().await;
    let get = |key: String| {
        let request = http.get(&url).bearer_auth(key);
        async move {
            request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let one = get(first.clone()).await;
    assert_eq!(one["models"].as_array().unwrap().len(), 2);
    assert_eq!(one["models"][0]["slug"], "gpt-6.2-sol");
    assert_eq!(one["models"][0]["supported_in_api"], true);
    assert_eq!(
        one["models"][0]["service_tiers"],
        json!([priority, "ultrafast"])
    );
    assert_eq!(one["models"][0]["default_service_tier"], "ultrafast");
    assert_eq!(one["models"][1]["slug"], "codex/gpt-6.2-sol");
    assert_eq!(one["models"][1]["visibility"], "hide");
    assert_eq!(
        one["models"][1]["service_tiers"],
        one["models"][0]["service_tiers"]
    );
    for method in [reqwest::Method::HEAD, reqwest::Method::GET] {
        assert_eq!(
            http.request(method, &url)
                .bearer_auth(&both)
                .send()
                .await
                .unwrap()
                .status(),
            503
        );
    }
    let generic = |key: &str| {
        let request = http.get(&generic_url).bearer_auth(key);
        async move {
            let catalog: Value = request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            catalog["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == "codex/gpt-6.2-sol")
                .cloned()
                .unwrap()
        }
    };
    assert!(generic(&both).await.get("service_tiers").is_none());
    assert!(generic(&both).await["capabilities"].get("speed").is_none());
    assert_eq!(
        generic(&first).await["service_tiers"],
        json!([priority, "ultrafast"])
    );
    f.replies.lock().unwrap().insert(
        "1".into(),
        json!({"models":[{"slug":"gpt-6.2-sol", "service_tiers":["priority"], "additional_speed_tiers":[{"id":"fast","name":"Fast"}]},
            model("gpt-6.2-astra", json!([priority]))]}),
    );
    f.state.refresh_models_in(Some("codex")).await;
    let all = get(both.clone()).await;
    assert_eq!(all["models"].as_array().unwrap().len(), 4);
    let sol = all["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["slug"] == "gpt-6.2-sol")
        .unwrap();
    if f.accounts[0] < f.accounts[1] {
        assert_eq!(sol["service_tiers"], json!([priority]));
        assert_eq!(sol["additional_speed_tiers"], json!(["fast"]));
    } else {
        assert_eq!(sol["service_tiers"], json!(["priority"]));
        assert_eq!(
            sol["additional_speed_tiers"],
            json!([{"id":"fast","name":"Fast"}])
        );
    }
    assert!(sol.get("default_service_tier").is_none());
    let generic_sol = generic(&both).await;
    assert_eq!(generic_sol["service_tiers"], sol["service_tiers"]);
    let options = generic_sol["capabilities"]["speed"]["options"]
        .as_array()
        .unwrap();
    assert!(options.iter().any(|option| option["id"] == "priority"));
    assert!(options.iter().any(|option| option["id"] == "fast"));
    assert!(!options.iter().any(|option| option["id"] == "ultrafast"));
    let mut account =
        serde_json::to_value(f.state.accounts.get(&f.accounts[1]).await.unwrap()).unwrap();
    account["models"] = json!(["gpt-6.2-astra"]);
    f.state.accounts.import(account, None).await.unwrap();
    let restricted = get(second).await;
    assert_eq!(restricted["models"].as_array().unwrap().len(), 2);
    assert_eq!(restricted["models"][0]["slug"], "gpt-6.2-astra");
    let all = get(both).await;
    let sol = all["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["slug"] == "gpt-6.2-sol")
        .unwrap();
    assert_eq!(sol["service_tiers"], json!([priority, "ultrafast"]));
    f.replies.lock().unwrap().insert(
        "0".into(),
        json!({"models":[{"slug":"gpt-6.2-sol","service_tiers":["default",priority],
            "default_service_tier":{"id":"priority"}}]}),
    );
    f.state.refresh_models_in(Some("codex")).await;
    assert_eq!(
        get(first.clone()).await["models"][0]["default_service_tier"],
        json!({"id":"priority"})
    );
    assert_eq!(
        generic(&first).await["capabilities"]["speed"]["default"],
        "priority"
    );
    f.state
        .config
        .write()
        .await
        .disabled_models
        .insert("codex/gpt-6.2-sol".into());
    assert_eq!(get(first.clone()).await["models"], json!([]));
    f.replies
        .lock()
        .unwrap()
        .insert("0".into(), json!({"models":[]}));
    f.state.refresh_models_in(Some("codex")).await;
    assert_eq!(get(first).await["models"], json!([]));
}

#[tokio::test]
async fn a_targeted_refresh_preserves_other_pools_and_a_disable_during_discovery() {
    let id = "codex/gpt-future";
    let f = Fixture::new(vec![listing("gpt-future", 700000)]).await;
    f.state.accounts.import(json!({
        "type":"claude","access_token":"fixture","account_id":"claude-fixture","models":["claude-fable-5-2"]
    }), None).await.unwrap();
    f.state.refresh_models().await;
    let (started, observed) = oneshot::channel();
    let (release, waiting) = oneshot::channel();
    *f.read_gate.lock().unwrap() = Some((started, waiting));
    let state = f.state.clone();
    let task = tokio::spawn(async move {
        state.refresh_models_in(Some("codex")).await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    f.state
        .config
        .write()
        .await
        .disabled_models
        .insert(id.into());
    release.send(()).unwrap();
    task.await.unwrap();
    let rows = f.state.models.read().await;
    assert!(rows.iter().any(|m| m.id == id && !m.enabled));
    assert!(
        rows.iter()
            .any(|m| m.id == "claude/claude-fable-5-2" && m.enabled)
    );
}

#[tokio::test]
async fn native_codex_models_are_discovered_with_current_metadata_and_account_eligibility() {
    let f = Fixture::new(vec![
        listing("gpt-6.1-terra", 500000),
        listing("gpt-future", 700000),
    ])
    .await;
    f.state.refresh_models().await;
    let ids = f.ids().await;
    assert!(ids.contains(&"codex/gpt-6.1-terra".into()), "{ids:?}");
    assert!(ids.contains(&"codex/gpt-future".into()), "{ids:?}");
    assert!(!ids.contains(&"codex/gpt-6-astra".into()));
    let models = f.state.models.read().await;
    let future = models.iter().find(|m| m.id == "codex/gpt-future").unwrap();
    assert_eq!(future.context_length, Some(700000));
    assert_eq!(
        future.thinking.as_ref().unwrap()["levels"],
        json!(["low", "ultra"])
    );
    assert_eq!(future.name, "Native model");
    drop(models);
    let (_, accounts) = f
        .state
        .candidates("gpt-6.1-terra", Some("codex"), None)
        .await
        .unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        serde_json::to_value(&accounts[0]).unwrap()["id"],
        f.accounts[0]
    );

    // Models added later become visible without a new bundled registry.
    f.replies
        .lock()
        .unwrap()
        .insert("1".into(), listing("gpt-next-release", 800000));
    f.state.refresh_models_in(Some("codex")).await;
    let ids = f.ids().await;
    assert!(ids.contains(&"codex/gpt-next-release".into()));
    assert!(!ids.contains(&"codex/gpt-future".into()));
}

#[tokio::test]
async fn subscription_models_remain_available_when_public_api_support_is_false() {
    let f = Fixture::new(vec![json!({"models":[{
        "slug":"gpt-subscription-only", "supported_in_api":false
    }]})])
    .await;
    f.state.refresh_models().await;
    assert!(
        f.ids()
            .await
            .contains(&"codex/gpt-subscription-only".into())
    );
    let (_, accounts) = f
        .state
        .candidates("gpt-subscription-only", Some("codex"), None)
        .await
        .unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        serde_json::to_value(&accounts[0]).unwrap()["id"],
        f.accounts[0]
    );
}

#[tokio::test]
async fn failed_native_discovery_keeps_previous_models_and_successful_empty_listing_clears_them() {
    let f = Fixture::new(vec![json!({"error":"unavailable"})]).await;
    f.state.refresh_models().await;
    assert!(f.ids().await.contains(&"codex/gpt-6.1-terra".into()));
    assert!(f.state.model_listings.lock().unwrap()["codex"].stale);
    f.replies
        .lock()
        .unwrap()
        .insert("0".into(), listing("gpt-future", 700000));
    f.state.refresh_models().await;
    assert_eq!(f.ids().await, vec!["codex/gpt-future"]);
    f.replies
        .lock()
        .unwrap()
        .insert("0".into(), json!({"models":"malformed"}));
    f.state.refresh_models().await;
    assert_eq!(f.ids().await, vec!["codex/gpt-future"]);
    assert!(f.state.model_listings.lock().unwrap()["codex"].stale);
    f.replies
        .lock()
        .unwrap()
        .insert("0".into(), json!({"models":[]}));
    f.state.refresh_models().await;
    assert!(f.ids().await.is_empty());
    assert!(!f.state.model_listings.lock().unwrap()["codex"].stale);
    assert!(
        f.state
            .candidates("gpt-future", Some("codex"), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_refresh_keeps_model_disables_and_account_allowlists() {
    let f = Fixture::new(vec![listing("gpt-6.1-terra", 500000)]).await;
    f.state
        .config
        .write()
        .await
        .disabled_models
        .insert("codex/gpt-6.1-terra".into());
    f.state.refresh_models().await;
    assert!(
        !f.state
            .models
            .read()
            .await
            .iter()
            .find(|m| m.id == "codex/gpt-6.1-terra")
            .unwrap()
            .enabled
    );
    assert!(
        f.state
            .candidates("gpt-6.1-terra", Some("codex"), None)
            .await
            .is_err()
    );
    // A successful listing must not override an explicit imported allowlist.
    let mut account =
        serde_json::to_value(f.state.accounts.get(&f.accounts[0]).await.unwrap()).unwrap();
    account["models"] = json!(["gpt-restricted"]);
    f.state.accounts.import(account, None).await.unwrap();
    // The imported account is the same identity; its explicit list is retained.
    f.state.refresh_models().await;
    assert!(!f.ids().await.contains(&"codex/gpt-6.1-terra".into()));
}
