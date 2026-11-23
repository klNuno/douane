use super::AppState;
use crate::subscriptions::{AccountCredential, SubscriptionProvider};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

mod antigravity;
mod codex;
mod compatible;
mod copilot;
#[cfg(test)]
pub(super) use antigravity::antigravity_catalog;
pub(crate) use codex::{CodexEntry, tier_id};

pub(super) enum AccountListing {
    Ids(BTreeSet<String>),
    Codex(BTreeMap<String, Value>),
    Antigravity(BTreeMap<String, Model>),
}

impl AccountListing {
    fn contains(&self, id: &str) -> bool {
        match self {
            Self::Ids(ids) => ids.contains(id),
            Self::Codex(rows) => rows.contains_key(id),
            Self::Antigravity(rows) => rows.contains_key(id),
        }
    }
}

/// How long a model refusal hides it from an account.
const REFUSAL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

#[derive(Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: SubscriptionProvider,
    pub name: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub upstream_id: String,
    #[serde(default)]
    pub thinking: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ModelCapabilities>,
    /// The custom endpoint route serving this model, absent for providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Upstream paths the model accepts, when its provider says so
    /// (`/v1/messages`, `/responses`, `/chat/completions`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<String>,
    /// Native provider catalog fields required by its own client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<SpeedCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SpeedCapability {
    pub options: Vec<SpeedOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SpeedOption {
    pub id: String,
    pub label: String,
}

fn yes() -> bool {
    true
}

/// The last catalog read of one pool.
#[derive(Clone, Serialize)]
pub struct ModelListing {
    pub refreshed_at: String,
    /// The live listing failed, so the pool shows the previous catalog.
    pub stale: bool,
}

/// Whether `model` belongs to `pool`, whose name prefixes its id.
fn in_pool(model: &Model, pool: &str) -> bool {
    model
        .id
        .strip_prefix(pool)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// A blank enabled row; constructors name the fields they know.
impl Default for Model {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider: SubscriptionProvider::OpenaiCompatible,
            name: String::new(),
            context_length: None,
            max_output_tokens: None,
            enabled: true,
            upstream_id: String::new(),
            thinking: None,
            capabilities: None,
            route: None,
            endpoints: vec![],
            metadata: None,
        }
    }
}

/// The bundled catalog, each provider's models in that provider's own order.
pub(crate) fn catalog() -> &'static [Model] {
    static CATALOG: std::sync::OnceLock<Vec<Model>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../../../data/models.json")).unwrap_or_default()
    })
}

/// A model's place in its provider's own list, the provider's default first.
/// A model the catalog does not name comes after the ones it does.
pub(crate) fn catalog_rank(provider: SubscriptionProvider, upstream_id: &str) -> usize {
    catalog()
        .iter()
        .filter(|model| model.provider == provider)
        .position(|model| model.id == upstream_id)
        .unwrap_or(usize::MAX)
}

impl AppState {
    pub(crate) async fn claude_catalog_contains(
        &self,
        client: &crate::clients::Client,
        id: &str,
    ) -> bool {
        let accounts = self
            .accounts
            .enabled_subscriptions(SubscriptionProvider::Claude)
            .await;
        accounts
            .iter()
            .any(|a| client.may_use(a) && a.offers_model(id) && self.serves(&a.id, id, a.provider))
            && self.models.read().await.iter().any(|m| {
                m.upstream_id == id && m.provider == SubscriptionProvider::Claude && m.enabled
            })
    }

    fn refuses(&self, account: &str, model: &str) -> bool {
        self.refused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(account.to_owned(), model.to_owned()))
            .is_some_and(|at| at.elapsed() < REFUSAL)
    }

    /// Whether `account` can be sent `model`: it did not refuse it lately,
    /// and its own last live listing, when it has one, names it.
    pub(crate) fn serves(
        &self,
        account: &str,
        model: &str,
        provider: SubscriptionProvider,
    ) -> bool {
        !self.refuses(account, model)
            && self
                .listed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(account)
                .map_or(provider != SubscriptionProvider::Meta, |ids| {
                    ids.contains(model)
                })
    }

    /// A catalog can list models an account's plan cannot use (Copilot), or
    /// that only another account of the pool offers (Antigravity); the first
    /// refusal skips the account for that model, and the model leaves the
    /// list once no account of the provider serves it.
    pub(crate) async fn model_refused(
        &self,
        provider: SubscriptionProvider,
        account: &str,
        model: &str,
    ) {
        self.refused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (account.to_owned(), model.to_owned()),
                std::time::Instant::now(),
            );
        let served = self
            .accounts
            .eligible_in(provider, None, Some(model))
            .await
            .iter()
            .any(|other| self.serves(&other.id, model, other.provider));
        if !served {
            self.models
                .write()
                .await
                .retain(|m| !(m.provider == provider && m.upstream_id == model));
        }
    }

    pub async fn refresh_models(&self) {
        self.refresh_models_in(None).await;
    }

    /// Pools whose last live listing failed.
    pub fn stale_pools(&self) -> Vec<String> {
        self.model_listings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, listing)| listing.stale)
            .map(|(pool, _)| pool.clone())
            .collect()
    }

    /// Rereads one pool or every pool, retaining unrelated rows.
    pub async fn refresh_models_in(&self, only: Option<&str>) {
        let _refresh = self.model_refresh.lock().await;
        let previous = self.models.read().await.clone();
        let mut models = BTreeMap::new();
        if let Some(pool) = only {
            models.extend(
                previous
                    .iter()
                    .filter(|m| !in_pool(m, pool))
                    .map(|m| (m.id.clone(), m.clone())),
            );
        }
        let mut listings = BTreeMap::new();
        let mut stale = BTreeMap::new();
        for summary in self.accounts.list().await {
            let Some(account) = self.accounts.get(&summary.id).await else {
                continue;
            };
            let pool = account.pool();
            if only.is_some_and(|only| only != pool) {
                continue;
            }
            let (available, failed) = self
                .account_models(&account, &previous, &mut listings)
                .await;
            *stale.entry(pool.to_owned()).or_default() |= failed;
            for mut model in available {
                model.upstream_id = model.id;
                model.id = format!("{pool}/{}", model.upstream_id);
                model.route = account.route.clone();
                models.insert(model.id.clone(), model);
            }
        }
        self.publish_models(models, listings).await;
        self.record_refresh(only, stale);
    }

    async fn discover_models(&self, account: &AccountCredential) -> Discovery {
        use SubscriptionProvider::*;
        let native = match account.provider {
            Codex => self.codex_models(account).await,
            Copilot => self.copilot_models(account).await,
            Antigravity if account.models.is_empty() => self.antigravity_models(account).await,
            Meta | OpencodeGo | OpenaiCompatible | Qwen => {
                return self.compatible_models(account).await;
            }
            _ => return Discovery::Bundled,
        };
        native.map_or(Discovery::NativeUnavailable, Discovery::Live)
    }

    async fn account_models(
        &self,
        account: &AccountCredential,
        previous: &[Model],
        listings: &mut BTreeMap<String, AccountListing>,
    ) -> (Vec<Model>, bool) {
        use SubscriptionProvider::*;
        let (mut available, failed) = match self.discover_models(account).await {
            Discovery::Live(mut live) => {
                if matches!(account.provider, Codex | Copilot | Antigravity | Meta) {
                    listings.insert(
                        account.id.clone(),
                        catalog_rows(account.provider, &mut live),
                    );
                }
                if matches!(account.provider, Codex | Copilot | Antigravity) {
                    live.retain(|m| !self.refuses(&account.id, &m.id));
                }
                (live, false)
            }
            Discovery::Bundled => (bundled_models(account), false),
            Discovery::NativeUnavailable => (self.native_fallback(account, previous), true),
            Discovery::CompatibleUnavailable => {
                tracing::warn!(pool = %account.pool(), "model listing unavailable, keeping the previous catalog");
                (
                    compatible_fallback(account, previous),
                    account.models.is_empty(),
                )
            }
        };
        // A fallback or explicit list is also account-specific.
        if account.provider == Antigravity && !listings.contains_key(&account.id) {
            listings.insert(
                account.id.clone(),
                catalog_rows(account.provider, &mut available),
            );
            available.retain(|m| !self.refuses(&account.id, &m.id));
        }
        available.retain(|m| account.offers_model(&m.id));
        (available, failed)
    }

    fn native_fallback(&self, account: &AccountCredential, previous: &[Model]) -> Vec<Model> {
        if account.provider == SubscriptionProvider::Antigravity {
            let listed = self.listed.lock().unwrap_or_else(|e| e.into_inner());
            return match listed.get(&account.id) {
                Some(AccountListing::Antigravity(rows)) => rows.values().cloned().collect(),
                _ => bundled_models(account),
            };
        }
        let kept: Vec<_> = previous_rows(account, previous).collect();
        if kept.is_empty() {
            return bundled_models(account);
        }
        kept.into_iter()
            .filter(|m| !self.refuses(&account.id, &m.upstream_id))
            .map(unqualify)
            .collect()
    }

    async fn publish_models(
        &self,
        models: BTreeMap<String, Model>,
        listings: BTreeMap<String, AccountListing>,
    ) {
        // Read disables with the catalog locked so a change made during the
        // live reads is reflected in the rows we publish.
        let mut published = self.models.write().await;
        let disabled = self.config.read().await.disabled_models.clone();
        self.listed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(listings);
        *published = models
            .into_values()
            .map(|mut model| {
                model.enabled =
                    !disabled.contains(&model.id) && !disabled.contains(&model.upstream_id);
                model
            })
            .collect();
    }

    fn record_refresh(&self, only: Option<&str>, stale: BTreeMap<String, bool>) {
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let mut listings = self
            .model_listings
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match only {
            Some(pool) => {
                listings.remove(pool);
            }
            None => listings.clear(),
        }
        listings.extend(stale.into_iter().map(|(pool, stale)| {
            (
                pool,
                ModelListing {
                    refreshed_at: now.clone(),
                    stale,
                },
            )
        }));
    }
}

enum Discovery {
    Bundled,
    Live(Vec<Model>),
    NativeUnavailable,
    CompatibleUnavailable,
}

fn bundled_models(account: &AccountCredential) -> Vec<Model> {
    let bundled = catalog()
        .iter()
        .filter(|model| model.provider == account.provider);
    if account.models.is_empty() {
        return bundled.cloned().collect();
    }
    account
        .models
        .iter()
        .map(|id| {
            if account.provider == SubscriptionProvider::Claude
                && let Some(model) = bundled.clone().find(|model| &model.id == id)
            {
                return model.clone();
            }
            Model {
                id: id.clone(),
                upstream_id: id.clone(),
                provider: account.provider,
                name: id.clone(),
                ..Model::default()
            }
        })
        .collect()
}

fn previous_rows<'a>(
    account: &'a AccountCredential,
    previous: &'a [Model],
) -> impl Iterator<Item = &'a Model> {
    previous.iter().filter(|m| {
        m.provider == account.provider
            && in_pool(m, account.pool())
            && account.offers_model(&m.upstream_id)
    })
}

fn unqualify(row: &Model) -> Model {
    Model {
        id: row.upstream_id.clone(),
        ..row.clone()
    }
}

fn compatible_fallback(account: &AccountCredential, previous: &[Model]) -> Vec<Model> {
    if account.models.is_empty() {
        return previous_rows(account, previous).map(unqualify).collect();
    }
    bundled_models(account)
        .into_iter()
        .map(|row| {
            previous_rows(account, previous)
                .find(|prior| prior.upstream_id == row.id)
                .map(unqualify)
                .unwrap_or(row)
        })
        .collect()
}

/// Codex keeps native fields in its per-account catalog. The common catalog
/// gets the intersection for the requesting client at read time.
fn catalog_rows(provider: SubscriptionProvider, live: &mut [Model]) -> AccountListing {
    match provider {
        SubscriptionProvider::Codex => AccountListing::Codex(
            live.iter_mut()
                .filter_map(|m| Some((m.id.clone(), m.metadata.take()?)))
                .collect(),
        ),
        SubscriptionProvider::Antigravity => {
            AccountListing::Antigravity(live.iter().map(|m| (m.id.clone(), m.clone())).collect())
        }
        _ => AccountListing::Ids(live.iter().map(|m| m.id.clone()).collect()),
    }
}
