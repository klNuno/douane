use crate::{
    ledger::Ledger,
    scheduler::{Candidate, Scheduler, Strategy},
    settings::Config,
    subscriptions::{
        AccountCredential, AccountStore, AccountSummary, OAuthManager, QuotaService, ResetManager,
        SubscriptionProvider,
    },
};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::RwLock;

mod models;
mod quotas;
mod routing;
use models::AccountListing;
#[cfg(test)]
use models::antigravity_catalog;
#[cfg(test)]
pub(crate) use models::catalog;
pub(crate) use models::{CodexEntry, catalog_rank, tier_id};
pub use models::{Model, ModelCapabilities, ModelListing, SpeedCapability, SpeedOption};
pub use quotas::reserve_reached_in;

pub struct AppState {
    pub config: RwLock<Config>,
    pub config_path: Option<PathBuf>,
    pub accounts: Arc<AccountStore>,
    pub oauth: Arc<OAuthManager>,
    pub quotas: Arc<QuotaService>,
    pub resets: Arc<ResetManager>,
    pub ledger: Arc<Ledger>,
    pub clients: Arc<crate::clients::Clients>,
    pub client: reqwest::Client,
    pub models: RwLock<Vec<Model>>,
    /// When each pool's catalog was last read, and whether its live listing
    /// failed; the background loop rereads a stale pool until it answers.
    pub model_listings: Mutex<BTreeMap<String, ModelListing>>,
    /// One catalog refresh at a time: each one starts from the rows the
    /// last one published.
    model_refresh: tokio::sync::Mutex<()>,
    /// Milliseconds between native WebSocket pings; a peer silent for 3.6
    /// of them (90 s at the default 25 s) is dropped. Tests shorten it.
    pub socket_heartbeat_ms: std::sync::atomic::AtomicU64,
    pub scheduler: Mutex<Scheduler>,
    pub affinity: Mutex<crate::affinity::Affinity>,
    pub management_token: Option<String>,
    pub config_write: tokio::sync::Mutex<()>,
    /// Which quota readings go to the pool chart history.
    pub quota_sampler: crate::quota_history::Sampler,
    /// Last background quota refresh started for an account near its reserve.
    reserve_refresh: Mutex<BTreeMap<String, std::time::Instant>>,
    /// Models an account refused although a catalog lists them, by account
    /// and model, with the time of the refusal.
    refused: Mutex<BTreeMap<(String, String), std::time::Instant>>,
    /// Each account's last live model list, with native rows for Codex.
    listed: Mutex<BTreeMap<String, AccountListing>>,
    /// Effort levels an OpenAI-compatible endpoint named for a model, by
    /// account and model, read once and kept across refreshes. Empty when it
    /// answered without a list.
    reasoning_levels: Mutex<BTreeMap<(String, String), Vec<String>>>,
}

/// The refusal of an Antigravity model outside the Gemini API route.
pub const ANTIGRAVITY_GEMINI_ONLY: &str =
    "Antigravity accounts serve only Antigravity's own agent, through the Gemini API (/v1beta)";

impl AppState {
    pub fn new(mut config: Config, config_path: Option<PathBuf>) -> Result<Arc<Self>> {
        config.settings.validate()?;
        config.server.normalize()?;
        let management_token = config
            .server
            .management_token_env
            .as_ref()
            .map(|name| std::env::var(name))
            .transpose()?;
        let accounts = Arc::new(AccountStore::open(config.server.auth_dir.clone())?);
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(120))
            .pool_max_idle_per_host(32)
            .tcp_nodelay(true)
            .user_agent(concat!("douane/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mut oauth = OAuthManager::new(client.clone(), accounts.clone());
        if let Some(url) = &config.server.codex_token_url {
            oauth.codex_token_url = url.clone();
        }
        let oauth = Arc::new(oauth);
        let quotas = Arc::new(QuotaService::with_oauth(
            client.clone(),
            accounts.clone(),
            oauth.clone(),
        ));
        let resets = Arc::new(ResetManager::new(accounts.clone(), oauth.clone())?);
        let ledger = Ledger::open(&config.server.database)?;
        let clients = Arc::new(crate::clients::Clients::open(&config.server.auth_dir)?);
        Ok(Arc::new(Self {
            config: RwLock::new(config),
            config_path,
            accounts,
            oauth,
            quotas,
            resets,
            ledger,
            clients,
            client,
            models: RwLock::new(vec![]),
            model_listings: Mutex::new(BTreeMap::new()),
            model_refresh: tokio::sync::Mutex::new(()),
            socket_heartbeat_ms: std::sync::atomic::AtomicU64::new(25_000),
            scheduler: Mutex::new(Scheduler::default()),
            affinity: Mutex::new(crate::affinity::Affinity::default()),
            management_token,
            config_write: tokio::sync::Mutex::new(()),
            quota_sampler: Default::default(),
            reserve_refresh: Mutex::new(BTreeMap::new()),
            refused: Mutex::new(BTreeMap::new()),
            listed: Mutex::new(BTreeMap::new()),
            reasoning_levels: Mutex::new(BTreeMap::new()),
        }))
    }

    /// Stores the quota readings that moved since the last pass.
    pub async fn sample_quotas(&self) {
        let due = self.quota_sampler.due(&self.quotas.list().await);
        self.ledger.record_samples(due);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Strategy;
    use crate::subscriptions::{QuotaCredits, QuotaSnapshot, QuotaWindow};
    use serde_json::json;

    #[test]
    fn antigravity_catalog_keeps_agent_models_in_upstream_order() {
        let value = serde_json::json!({
            "models": {
                "chat_20706": {"isInternal": true, "maxTokens": 16384},
                "tab_flash_lite_preview": {"maxTokens": 16384},
                "gemini-3.8-flash-tiered": {"maxTokens": 1048576},
                "gemini-2.5-flash": {"displayName": "Gemini 3.5 Flash Lite", "maxTokens": 1048576},
                "gemini-3.5-flash-lite": {"displayName": "Gemini 3.5 Flash Lite", "maxTokens": 1048576},
                "claude-opus-4-6-thinking": {"displayName": "Claude Opus 4.6 (Thinking)", "maxTokens": 250000, "maxOutputTokens": 64000},
                "gemini-3.8-flash-high": {"displayName": "Gemini 3.8 Flash (High)", "maxTokens": 1048576, "maxOutputTokens": 65536},
                "gemini-new": {"displayName": "Gemini New", "supportsThinking": true, "minThinkingBudget": 32, "thinkingBudget": -1}
            },
            "agentModelSorts": [{"groups": [{"modelIds": ["gemini-3.8-flash-high", "claude-opus-4-6-thinking"]}]}],
            "tabModelIds": ["chat_20706", "tab_flash_lite_preview"]
        });
        let models = super::antigravity_catalog(&value);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gemini-3.8-flash-high",
                "claude-opus-4-6-thinking",
                "gemini-3.5-flash-lite",
                "gemini-new"
            ]
        );
        assert_eq!(models[1].context_length, Some(250000));
        assert_eq!(models[1].max_output_tokens, Some(64000));
        assert_eq!(
            models[3].thinking,
            Some(serde_json::json!({"min": 32, "dynamic_allowed": true}))
        );
    }

    #[test]
    fn bundled_antigravity_ids_are_the_live_agent_models() {
        let ids: Vec<&str> = super::catalog()
            .iter()
            .filter(|m| m.provider == SubscriptionProvider::Antigravity)
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids[0], "gemini-3.8-flash-high");
        assert!(ids.contains(&"claude-opus-4-6-thinking"));
        assert!(!ids.contains(&"gemini-pro-agent"));
    }

    fn credits_only(id: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: id.into(),
            provider: SubscriptionProvider::Codex,
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                label: "Codex Weekly".into(),
                remaining_percent: Some(0.0),
                reset_at: Some("2099-01-01T00:00:00Z".into()),
                used: Some(100.0),
                limit: Some(100.0),
            }],
            updated_at: crate::subscriptions::now(),
            error: None,
            subscription: None,
            credits: vec![QuotaCredits {
                id: "codex-credits".into(),
                label: "Credits".into(),
                used: None,
                limit: None,
                remaining: Some(10.0),
                unit: "credits".into(),
                reset_at: None,
                unlimited: Some(false),
                enabled: Some(true),
            }],
            verification_required: false,
            verification_available: false,
        }
    }

    fn weekly(id: &str, reset: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            windows: vec![QuotaWindow {
                id: "weekly".into(),
                label: "Codex Weekly".into(),
                remaining_percent: Some(60.0),
                reset_at: Some(reset.into()),
                used: Some(40.0),
                limit: Some(100.0),
            }],
            credits: vec![],
            ..credits_only(id)
        }
    }

    #[tokio::test]
    async fn overbooked_account_caps_fail_over_and_match_public_quotas() {
        use axum::{body::Body, http::Request};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["a", "b"] {
            ids.push(
                state
                    .accounts
                    .import(
                        json!({"type":"codex","access_token":"fixture",
                "account_id":name,"name":name}),
                        None,
                    )
                    .await
                    .unwrap()
                    .id,
            );
        }
        let (small, key) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("small".into()),
                shares: Some([("codex".into(), 20.0)].into()),
                account_shares: Some([(ids[0].clone(), 10.0)].into()),
                ..Default::default()
            })
            .unwrap();
        state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("large".into()),
                shares: Some([("codex".into(), 95.0)].into()),
                ..Default::default()
            })
            .unwrap();
        let at = |id: &str, used: f64| {
            let mut snapshot = weekly(id, "2099-01-08T00:00:00Z");
            snapshot.windows[0].remaining_percent = Some(100.0 - used);
            snapshot
        };
        for id in &ids {
            state.quotas.insert_for_test(at(id, 0.0)).await;
            state.observe_usage(id).await;
        }
        state.clients.call_ended(&ids[0], Some(&small.id), 1.0);
        state.quotas.insert_for_test(at(&ids[0], 10.0)).await;
        state.observe_usage(&ids[0]).await;
        let (_, candidates) = state
            .candidates_for("gpt-5.5", Some("codex"), None, Some(&small))
            .await
            .unwrap();
        assert_eq!(
            candidates.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
            vec![ids[1].clone()]
        );

        let response = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v1/quotas")
                    .header("authorization", format!("Bearer {key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let view: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let entries = view["providers"][0]["entries"].as_array().unwrap();
        for (id, expected) in [(&ids[0], 0.0), (&ids[1], 100.0)] {
            let entry = entries.iter().find(|e| e["id"] == id.as_str()).unwrap();
            assert_eq!(entry["windows"][0]["remaining_percent"], expected, "{view}");
            assert!(entry["error"].is_null() && entry["read_error"].is_null());
            assert!(entry["status_reason"].is_null());
            assert!(entry["retry_at"].is_null());
        }
        // With access restricted to the spent account, the HTTP router returns 429.
        state
            .clients
            .update(
                &small.id,
                crate::clients::ClientPatch {
                    access: Some(crate::clients::Access {
                        all: false,
                        pools: Default::default(),
                        accounts: [ids[0].clone()].into(),
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        let response = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("authorization", format!("Bearer {key}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"codex/gpt-5.5","input":"test"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&bytes).contains("overbooked share cap"));
    }

    #[tokio::test]
    async fn a_client_past_what_other_shares_leave_it_is_held_back_with_429() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let account = state
            .accounts
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":"a","name":"a"}),
                None,
            )
            .await
            .unwrap();
        let patch = |name: &str, share: Option<f64>| crate::clients::ClientPatch {
            name: Some(name.into()),
            shares: share.map(|s| [("codex".to_owned(), s)].into()),
            ..Default::default()
        };
        let (famille, _) = state.clients.create(patch("famille", Some(30.0))).unwrap();
        let (perso, _) = state.clients.create(patch("perso", None)).unwrap();
        let (outsider, _) = state
            .clients
            .create(crate::clients::ClientPatch {
                access: Some(crate::clients::Access {
                    all: false,
                    pools: ["claude".to_owned()].into(),
                    accounts: Default::default(),
                }),
                ..patch("claude only", None)
            })
            .unwrap();
        let at = |used: f64| QuotaSnapshot {
            windows: vec![QuotaWindow {
                remaining_percent: Some(100.0 - used),
                ..weekly(&account.id, "2099-01-08T00:00:00Z").windows[0].clone()
            }],
            ..weekly(&account.id, "2099-01-08T00:00:00Z")
        };
        state.quotas.insert_for_test(at(0.0)).await;
        state.observe_usage(&account.id).await;
        state.clients.call_ended(&account.id, Some(&perso.id), 10.0);
        state.quotas.insert_for_test(at(70.0)).await;
        state.observe_usage(&account.id).await;
        let first = |client: &crate::clients::Client| {
            let state = state.clone();
            let client = client.clone();
            async move {
                state
                    .candidates_for("gpt-5.5", Some("codex"), None, Some(&client))
                    .await
                    .map(|(_, accounts)| accounts[0].id.clone())
            }
        };
        let refused = first(&perso).await.unwrap_err();
        assert!(
            refused.starts_with(crate::clients::SHARE_USED_UP),
            "{refused}"
        );
        assert_eq!(
            crate::relay::refusal_status(&refused),
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(first(&famille).await.unwrap(), account.id);
        assert!(
            first(&outsider)
                .await
                .unwrap_err()
                .starts_with("no available subscription")
        );
    }

    #[tokio::test]
    async fn a_rate_limit_rests_an_account_without_closing_its_pool() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["main", "spare"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        let (main, spare) = (ids[0].clone(), ids[1].clone());
        let first = || async {
            state
                .candidates("gpt-5.5", Some("codex"), None)
                .await
                .map(|(_, accounts)| accounts.iter().map(|a| a.id.clone()).collect::<Vec<_>>())
        };
        // With another account ready, the rested one stays out of rotation.
        state
            .accounts
            .cooldown(&main, 400, "upstream temporarily unavailable")
            .await
            .unwrap();
        assert_eq!(first().await.unwrap(), vec![spare.clone()]);
        // A ready account outside the client's access must not hide the
        // rested one it may use.
        let (main_only, _) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("main only".into()),
                access: Some(crate::clients::Access {
                    all: false,
                    pools: Default::default(),
                    accounts: [main.clone()].into(),
                }),
                ..Default::default()
            })
            .unwrap();
        let (_, allowed) = state
            .candidates_for("gpt-5.5", Some("codex"), None, Some(&main_only))
            .await
            .unwrap();
        assert_eq!(allowed[0].id, main);
        // With none ready, the pool still serves: the upstream answers the
        // client itself rather than a 503 naming a missing subscription.
        state
            .accounts
            .cooldown(&spare, 60, "websocket upstream rate limited")
            .await
            .unwrap();
        let mut resting = first().await.unwrap();
        resting.sort();
        let mut both = vec![main.clone(), spare.clone()];
        both.sort();
        assert_eq!(resting, both);
        // An empty window or a refused login still closes the pool: retrying
        // either cannot succeed before its reset or a new login.
        state
            .accounts
            .cooldown(&main, 300, "authentication needs reconnecting")
            .await
            .unwrap();
        state
            .accounts
            .cooldown(&spare, 3600, "websocket upstream quota exceeded")
            .await
            .unwrap();
        assert!(
            first()
                .await
                .unwrap_err()
                .starts_with("no available subscription")
        );
    }

    #[tokio::test]
    async fn the_chosen_order_and_keep_fable_decide_which_account_leads() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        config.settings.strategy = Strategy::Manual;
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["one", "two", "three"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"claude","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        for (id, fable_left) in ids.iter().zip([40.0, 0.0, 40.0]) {
            let mut quota = weekly(id, "2099-01-01T00:00:00Z");
            quota.windows[0].id = "seven-day".into();
            quota.windows.push(QuotaWindow {
                id: "seven-day-fable".into(),
                label: "Fable".into(),
                remaining_percent: Some(fable_left),
                reset_at: Some("2099-01-01T00:00:00Z".into()),
                used: None,
                limit: None,
            });
            state.quotas.insert_for_test(quota).await;
        }
        async fn order(state: &AppState, model: &str) -> Vec<String> {
            let (_, accounts) = state.candidates(model, Some("claude"), None).await.unwrap();
            accounts.into_iter().map(|a| a.id).collect()
        }
        let chosen = vec![ids[2].clone(), ids[0].clone()];
        state
            .config
            .write()
            .await
            .settings
            .account_order
            .insert("claude".into(), chosen);
        let mut unlisted_last = vec![ids[2].clone(), ids[0].clone(), ids[1].clone()];
        assert_eq!(order(&state, "claude-opus-5-5").await, unlisted_last);

        // Opus now starts on the account whose Fable week is over; Fable does not.
        state.config.write().await.settings.keep_fable = true;
        assert_eq!(order(&state, "claude-fable-5-2").await, unlisted_last);
        unlisted_last.rotate_right(1);
        assert_eq!(order(&state, "claude-opus-5-5").await, unlisted_last);
    }

    #[tokio::test]
    async fn a_borrowed_account_serves_only_the_family_its_home_week_holds_back() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["x5", "magali"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"claude","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        let (home, lent) = (ids[0].clone(), ids[1].clone());
        let (pote, _) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("pote".into()),
                access: Some(crate::clients::Access {
                    all: false,
                    pools: Default::default(),
                    accounts: [home.clone()].into(),
                }),
                borrows: Some(
                    [(
                        lent.clone(),
                        crate::clients::Borrow {
                            home: home.clone(),
                            scale: 1.0,
                        },
                    )]
                    .into(),
                ),
                ..Default::default()
            })
            .unwrap();
        let quota = |id: &str, week_left: f64, fable_left: f64| {
            let mut quota = weekly(id, "2099-01-01T00:00:00Z");
            quota.provider = SubscriptionProvider::Claude;
            quota.windows[0].id = "seven-day".into();
            quota.windows[0].remaining_percent = Some(week_left);
            quota.windows.push(QuotaWindow {
                id: "seven-day-fable".into(),
                label: "Fable".into(),
                remaining_percent: Some(fable_left),
                reset_at: Some("2099-01-01T00:00:00Z".into()),
                used: None,
                limit: None,
            });
            quota
        };
        state.quotas.insert_for_test(quota(&lent, 90.0, 90.0)).await;
        let routes = |model: &'static str| {
            let state = state.clone();
            let pote = pote.clone();
            async move {
                let (_, accounts) = state
                    .candidates_for(model, Some("claude"), None, Some(&pote))
                    .await
                    .unwrap_or_default();
                accounts.into_iter().map(|a| a.id).collect::<Vec<_>>()
            }
        };
        // Its own Fable week still has room: no borrowing.
        state.quotas.insert_for_test(quota(&home, 40.0, 10.0)).await;
        assert_eq!(routes("claude-fable-5-2").await, [home.clone()]);
        // Spent: Fable may go to the borrowed account, Opus may not.
        state.quotas.insert_for_test(quota(&home, 40.0, 0.0)).await;
        assert!(routes("claude-fable-5-2").await.contains(&lent));
        assert_eq!(routes("claude-opus-5-5").await, [home.clone()]);
        assert!(
            !state
                .client_excludes(
                    Some(&pote),
                    &state.accounts.get(&lent).await.unwrap(),
                    "claude-fable-5-2"
                )
                .await
        );
        assert!(
            state
                .client_excludes(
                    Some(&pote),
                    &state.accounts.get(&lent).await.unwrap(),
                    "claude-opus-5-5"
                )
                .await
        );
        // Without room in the home week, nothing is borrowed.
        state.quotas.insert_for_test(quota(&home, 0.0, 0.0)).await;
        assert!(!routes("claude-fable-5-2").await.contains(&lent));
    }

    #[tokio::test]
    async fn reset_first_moves_a_pinned_session_to_the_nearest_reset() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["later", "soon"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        let (later, soon) = (ids[0].clone(), ids[1].clone());
        state
            .quotas
            .insert_for_test(weekly(&later, "2099-01-08T00:00:00Z"))
            .await;
        state
            .quotas
            .insert_for_test(weekly(&soon, "2099-01-01T00:00:00Z"))
            .await;
        // A failover or an earlier strategy left the conversation on "later".
        state
            .affinity
            .lock()
            .unwrap()
            .bind("codex", "session", &later);
        async fn first(state: &AppState, strategy: Strategy) -> String {
            state.config.write().await.settings.strategy = strategy;
            let (_, accounts) = state
                .candidates("gpt-5.5", Some("codex"), Some("session"))
                .await
                .unwrap();
            accounts[0].id.clone()
        }
        for strategy in [Strategy::Smart, Strategy::RoundRobin, Strategy::FillFirst] {
            assert_eq!(first(&state, strategy).await, later, "{strategy:?}");
        }
        assert_eq!(first(&state, Strategy::ResetFirst).await, soon);

        // While the nearest reset is unavailable, the pin stays on "later".
        state.accounts.set_enabled(&soon, false).await.unwrap();
        assert_eq!(first(&state, Strategy::ResetFirst).await, later);
    }

    #[tokio::test]
    async fn a_session_leaves_usage_credits_only_for_subscription_quota() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["paid", "quota"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        state.quotas.insert_for_test(credits_only(&ids[0])).await;
        state
            .affinity
            .lock()
            .unwrap()
            .bind("codex", "session", &ids[0]);
        let paid = state.accounts.get(&ids[0]).await.unwrap();
        async fn first(state: &AppState) -> String {
            let (_, accounts) = state
                .candidates("gpt-5.5", Some("codex"), Some("session"))
                .await
                .unwrap();
            accounts[0].id.clone()
        }
        assert_eq!(first(&state).await, ids[1]);
        assert!(
            !state
                .can_reuse_socket(&paid.id, paid.provider, "gpt-5.5", Some("session"), None)
                .await
        );

        let (restricted, _) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("only paid".into()),
                access: Some(crate::clients::Access {
                    all: false,
                    accounts: [ids[0].clone()].into(),
                    pools: Default::default(),
                }),
                ..Default::default()
            })
            .unwrap();
        assert!(
            state
                .can_reuse_socket(
                    &paid.id,
                    paid.provider,
                    "gpt-5.5",
                    Some("session"),
                    Some(&restricted)
                )
                .await
        );
        let (holder, _) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("subscription holder".into()),
                account_shares: Some([(ids[1].clone(), 100.0)].into()),
                ..Default::default()
            })
            .unwrap();
        state
            .quotas
            .insert_for_test(weekly(&ids[1], "2099-01-08T00:00:00Z"))
            .await;
        let (guest, _) = state
            .clients
            .create(crate::clients::ClientPatch {
                name: Some("guest".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            state
                .can_reuse_socket(
                    &paid.id,
                    paid.provider,
                    "gpt-5.5",
                    Some("session"),
                    Some(&guest)
                )
                .await
        );
        assert!(
            !state
                .can_reuse_socket(
                    &paid.id,
                    paid.provider,
                    "gpt-5.5",
                    Some("session"),
                    Some(&holder)
                )
                .await
        );

        // Once no subscription quota remains, the pinned account pays.
        state.accounts.set_enabled(&ids[1], false).await.unwrap();
        assert_eq!(first(&state).await, ids[0]);
        assert!(
            state
                .can_reuse_socket(&paid.id, paid.provider, "gpt-5.5", Some("session"), None)
                .await
        );
    }

    #[tokio::test]
    async fn expired_five_hour_snapshot_routes_with_and_without_a_pin() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let account = state
            .accounts
            .import(json!({"type":"codex","access_token":"fixture"}), None)
            .await
            .unwrap();
        let mut snapshot = weekly(&account.id, "2000-01-01T00:00:00Z");
        snapshot.windows[0].id = "five-hour".into();
        snapshot.windows[0].remaining_percent = Some(0.0);
        state.quotas.insert_for_test(snapshot).await;
        state
            .affinity
            .lock()
            .unwrap()
            .bind("codex", "cached", &account.id);
        for session in [None, Some("cached")] {
            let (_, candidates) = state
                .candidates("gpt-5.5", Some("codex"), session)
                .await
                .unwrap();
            assert_eq!(candidates[0].id, account.id);
        }
    }

    #[tokio::test]
    async fn claude_fable_5_2_goes_to_a_claude_account() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let claude = state
            .accounts
            .import(json!({"type":"claude","access_token":"fixture"}), None)
            .await
            .unwrap();
        state.refresh_models().await;
        for model in ["claude-fable-5-2", "claude/claude-fable-5-2"] {
            let (upstream, accounts) = state.candidates(model, None, None).await.unwrap();
            assert_eq!(upstream, "claude-fable-5-2");
            assert_eq!(accounts[0].id, claude.id);
        }
        // Like Claude Fable 5.1, thinking cannot be turned off.
        let fable = catalog()
            .iter()
            .find(|m| m.id == "claude-fable-5-2")
            .unwrap();
        assert_eq!(fable.thinking.as_ref().unwrap()["zero_allowed"], false);
    }

    #[tokio::test]
    async fn claude_haiku_5_5_goes_to_a_claude_account() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let claude = state
            .accounts
            .import(json!({"type":"claude","access_token":"fixture"}), None)
            .await
            .unwrap();
        state.refresh_models().await;
        let (upstream, accounts) = state
            .candidates("claude-haiku-5-5", None, None)
            .await
            .unwrap();
        assert_eq!(upstream, "claude-haiku-5-5");
        assert_eq!(accounts[0].id, claude.id);
    }

    #[tokio::test]
    async fn an_account_at_its_reserve_leaves_rotation_and_releases_its_pin() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let mut ids = vec![];
        for name in ["kept", "spare"] {
            let account = state
                .accounts
                .import(
                    json!({"type":"codex","access_token":"fixture","account_id":name,"name":name}),
                    None,
                )
                .await
                .unwrap();
            ids.push(account.id);
        }
        let (kept, spare) = (ids[0].clone(), ids[1].clone());
        let mut low = weekly(&kept, "2099-01-01T00:00:00Z");
        low.windows[0].remaining_percent = Some(5.0);
        state.quotas.insert_for_test(low).await;
        state
            .quotas
            .insert_for_test(weekly(&spare, "2099-01-08T00:00:00Z"))
            .await;
        state
            .affinity
            .lock()
            .unwrap()
            .bind("codex", "session", &kept);
        async fn order(state: &AppState) -> Result<Vec<String>, String> {
            state
                .candidates("gpt-5.5", Some("codex"), Some("session"))
                .await
                .map(|(_, accounts)| accounts.into_iter().map(|a| a.id).collect())
        }
        // Without a reserve, 5% left is still spent.
        assert_eq!(order(&state).await.unwrap()[0], kept);

        state
            .accounts
            .update(&kept, None, None, Some(Some(5.0)), None)
            .await
            .unwrap();
        assert_eq!(order(&state).await.unwrap(), vec![spare.clone()]);
        let reserved = state.accounts.get(&kept).await.unwrap();
        assert!(state.reserve_reached(&reserved).await);
        let overview = crate::quota_view::overview(
            vec![reserved.summary()],
            vec![state.quotas.get(&kept).await.unwrap()],
            &state.config.read().await.settings,
            crate::subscriptions::now(),
        );
        let entry = &overview["providers"][0]["entries"][0];
        assert_eq!(entry["windows"][0]["remaining_percent"], 5.0);
        for field in ["error", "read_error", "status_reason", "retry_at"] {
            assert_eq!(entry.get(field), Some(&serde_json::Value::Null), "{field}");
        }

        // The reserve is a hard floor: no other account means no request.
        state.accounts.set_enabled(&spare, false).await.unwrap();
        assert!(order(&state).await.is_err());

        // Above the reserve, the account serves again.
        state
            .accounts
            .update(&kept, None, None, Some(Some(4.0)), None)
            .await
            .unwrap();
        assert_eq!(order(&state).await.unwrap(), vec![kept]);
    }

    #[tokio::test]
    async fn a_reserve_stops_applying_hours_before_the_weekly_reset() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let account = state
            .accounts
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":"kept","name":"kept"}),
                None,
            )
            .await
            .unwrap();
        state
            .accounts
            .update(&account.id, None, None, Some(Some(5.0)), None)
            .await
            .unwrap();
        let reset_in = |hours: i64| {
            (time::OffsetDateTime::now_utc() + time::Duration::hours(hours))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let low = |reset: String| {
            let mut low = weekly(&account.id, &reset);
            low.windows[0].remaining_percent = Some(5.0);
            low
        };
        async fn serves(state: &AppState) -> bool {
            state
                .candidates("gpt-5.5", Some("codex"), None)
                .await
                .is_ok()
        }
        let credential = state.accounts.get(&account.id).await.unwrap();
        // Seven hours before the reset, the reserve still holds.
        state.quotas.insert_for_test(low(reset_in(7))).await;
        assert!(!serves(&state).await);
        assert!(state.reserve_reached(&credential).await);
        // Within the default six hours, the rest of the week gets used.
        state.quotas.insert_for_test(low(reset_in(5))).await;
        assert!(serves(&state).await);
        assert!(!state.reserve_reached(&credential).await);
        assert!(!state.reserve_excludes(&credential).await);
        // At 0 the reserve holds to the end.
        state.config.write().await.settings.free_before_reset_hours = 0;
        assert!(!serves(&state).await);
    }

    #[tokio::test]
    async fn a_stale_reached_reserve_refreshes_and_a_bad_stored_value_is_ignored() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let account = state
            .accounts
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":"low"}),
                None,
            )
            .await
            .unwrap();
        state
            .accounts
            .update(&account.id, None, None, Some(Some(5.0)), None)
            .await
            .unwrap();
        let mut low = weekly(&account.id, "2099-01-01T00:00:00Z");
        low.windows[0].remaining_percent = Some(2.0);
        low.updated_at = "2026-01-01T00:00:00Z".into();
        state.quotas.insert_for_test(low).await;
        let credential = state.accounts.get(&account.id).await.unwrap();
        // The listing check reads only.
        assert!(state.reserve_reached(&credential).await);
        assert!(state.reserve_refresh.lock().unwrap().is_empty());
        // Removing the account keeps the spawned refresh off the network.
        let snapshot = state.quotas.get(&account.id).await.unwrap();
        state.accounts.delete(&account.id).await.unwrap();
        assert!(state.reserve_excludes_with(
            &credential,
            Some(&snapshot),
            time::OffsetDateTime::now_utc(),
            state.free_before_reset_seconds().await,
        ));
        assert!(
            state
                .reserve_refresh
                .lock()
                .unwrap()
                .contains_key(&account.id)
        );

        // A hand-edited file outside 0 < reserve < 100 never parks the account.
        let mut edited = credential.clone();
        for invalid in [150.0, f64::INFINITY, f64::NAN, -3.0] {
            edited.reserve_percent = Some(invalid);
            assert!(!state.reserve_reached(&edited).await, "{invalid}");
            assert_eq!(edited.summary().reserve_percent, None);
        }
    }
    #[tokio::test]
    async fn gemini_owner_in_cooldown_still_obeys_reserves_and_disabled_models() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("requests.sqlite");
        let state = AppState::new(config, None).unwrap();
        let owner=state.accounts.import(json!({"type":"antigravity","access_token":"synthetic-reserve","models":["synthetic"]}),None).await.unwrap();
        state
            .accounts
            .cooldown(&owner.id, 60, "synthetic cooldown")
            .await
            .unwrap();
        let pick = || {
            state.candidates_with_owner(
                "antigravity/synthetic",
                None,
                None,
                None,
                Some(&owner.id),
                true,
                true,
            )
        };
        assert_eq!(pick().await.unwrap().1[0].id, owner.id);
        let mut snapshot = weekly(&owner.id, "2099-01-08T00:00:00Z");
        snapshot.provider = SubscriptionProvider::Antigravity;
        state.quotas.insert_for_test(snapshot).await;
        state
            .accounts
            .update(&owner.id, None, None, Some(Some(70.0)), None)
            .await
            .unwrap();
        assert!(
            pick().await.is_err(),
            "cooldown ownership cannot spend the reserved quota"
        );
        state
            .accounts
            .update(&owner.id, None, None, Some(None), None)
            .await
            .unwrap();
        state
            .config
            .write()
            .await
            .disabled_models
            .insert("antigravity/synthetic".into());
        assert_eq!(pick().await.err().unwrap(), "model is disabled");
    }
}
