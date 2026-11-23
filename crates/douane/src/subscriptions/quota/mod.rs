use super::compatible_quota::BillingApi;
use super::cooldown::{Provenance, QuotaScope, Refusal, RefusalKind, window_scope};
use super::oauth::{send_json, send_json_received};
use super::store::{
    AccountQuota, ConfirmedReset, Exhaustion, WindowReading, subscription_from_value, text,
};
use super::{
    AccountCredential, AccountStore, AccountSummary, OAuthManager, SubscriptionError,
    SubscriptionProvider, SubscriptionTier, antigravity_user_agent, now, parse_timestamp,
    timestamp,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::RwLock;

mod persistence;
mod refresh;
mod sources;
mod state;
#[cfg(test)]
mod tests;
mod windows;

pub(crate) use persistence::write_private;
use persistence::*;
use refresh::*;
use sources::*;
use state::newer_than;
pub(super) use state::resolved_before_recording;
pub(crate) use windows::window_seconds;
use windows::*;
pub use windows::{QuotaCredits, QuotaSnapshot, QuotaWindow, RESERVE_WINDOWS, reserve_kind};

struct Observation {
    received: time::OffsetDateTime,
    windows: Vec<QuotaWindow>,
    plan: Option<String>,
    subscription: Option<SubscriptionTier>,
    credits: Vec<QuotaCredits>,
    organization_id: Option<String>,
}

pub struct QuotaService {
    client: reqwest::Client,
    billing_client: reqwest::Client,
    store: Arc<AccountStore>,
    oauth: Arc<OAuthManager>,
    cache: RwLock<BTreeMap<String, QuotaSnapshot>>,
    // Serialize publication of cache, failure status, and account windows.
    transitions: tokio::sync::Mutex<()>,
    // The key revision is process-local, so billing readings only live in memory.
    billing: RwLock<BTreeMap<String, BillingReading>>,
    save_lock: tokio::sync::Mutex<()>,
    verification_urls: RwLock<BTreeMap<String, String>>,
    failures: RwLock<BTreeMap<String, Failures>>,
    /// GitHub's Copilot entitlement endpoint; tests point it at a loopback.
    pub(crate) copilot_user_url: String,
    /// Claude's OAuth API base; tests use a loopback for usage and profile.
    pub(crate) claude_api: String,
    /// ChatGPT's Codex usage endpoint; tests use a loopback.
    pub(crate) codex_usage_url: String,
    #[cfg(test)]
    billing_test_origin: Option<String>,
}

impl QuotaService {
    pub fn new(client: reqwest::Client, store: Arc<AccountStore>) -> Self {
        let oauth = Arc::new(OAuthManager::new(client.clone(), store.clone()));
        Self::with_oauth(client, store, oauth)
    }

    pub fn with_oauth(
        client: reqwest::Client,
        store: Arc<AccountStore>,
        oauth: Arc<OAuthManager>,
    ) -> Self {
        let mut cache = load_snapshots(&store.quota_snapshot_path());
        cache.retain(|_, snapshot| snapshot.provider != SubscriptionProvider::OpenaiCompatible);
        Self {
            client,
            billing_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("billing HTTP client"),
            store,
            oauth,
            cache: RwLock::new(cache),
            transitions: tokio::sync::Mutex::new(()),
            billing: RwLock::new(BTreeMap::new()),
            save_lock: tokio::sync::Mutex::new(()),
            verification_urls: RwLock::new(BTreeMap::new()),
            failures: RwLock::new(BTreeMap::new()),
            copilot_user_url: "https://api.github.com/copilot_internal/user".into(),
            claude_api: "https://api.anthropic.com".into(),
            codex_usage_url: "https://chatgpt.com/backend-api/wham/usage".into(),
            #[cfg(test)]
            billing_test_origin: None,
        }
    }

    pub async fn list(&self) -> Vec<QuotaSnapshot> {
        let accounts = self.store.list().await;
        let mut snapshots = Vec::new();
        for account in accounts {
            if let Some(snapshot) = self.get(&account.id).await {
                snapshots.push(snapshot);
            }
        }
        snapshots
    }

    #[cfg(test)]
    pub(crate) async fn insert_for_test(&self, snapshot: QuotaSnapshot) {
        self.cache
            .write()
            .await
            .insert(snapshot.account_id.clone(), snapshot);
    }

    pub async fn get(&self, id: &str) -> Option<QuotaSnapshot> {
        let account = self.store.read_account(id).await?;
        self.for_account(&account).await
    }

    pub(crate) async fn for_account(&self, account: &AccountCredential) -> Option<QuotaSnapshot> {
        let id = &account.id;
        if account.provider != SubscriptionProvider::OpenaiCompatible {
            let cached = self.cache.read().await.get(id).cloned();
            return state::snapshot_for(account, cached);
        }
        self.billing
            .read()
            .await
            .get(id)
            .filter(|reading| reading.revision == account.endpoint_revision)
            .map(|reading| reading.snapshot.clone())
    }

    pub(crate) fn supports(account: &AccountSummary) -> bool {
        account.provider.reports_quota()
            || (account.provider == SubscriptionProvider::OpenaiCompatible
                && BillingApi::for_base_url(account.base_url.as_deref()).is_some())
    }

    pub async fn verification_url(&self, id: &str) -> Option<String> {
        if self.store.get(id).await.is_none() {
            self.verification_urls.write().await.remove(id);
            return None;
        }
        self.verification_urls.read().await.get(id).cloned()
    }
}
