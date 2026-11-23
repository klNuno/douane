use super::cooldown::{QuotaScope, Refusal, RefusalKind};
use super::{SubscriptionError, now, parse_timestamp, random_id, timestamp};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, RwLock};

mod endpoint;
mod import;
mod record;
mod storage;
#[cfg(test)]
mod tests;

pub use endpoint::*;
#[cfg(test)]
use import::parse_import;
pub(crate) use import::{apply_identity, subscription_from_value, text};
pub use record::*;
use storage::valid_id;

pub struct AccountStore {
    directory: PathBuf,
    accounts: Arc<RwLock<BTreeMap<String, AccountCredential>>>,
    pub(crate) refresh_locks: Mutex<BTreeMap<String, std::sync::Arc<Mutex<()>>>>,
}

impl AccountStore {
    pub async fn list(&self) -> Vec<AccountSummary> {
        self.accounts
            .read()
            .await
            .values()
            .map(AccountCredential::summary)
            .collect()
    }

    pub async fn get(&self, id: &str) -> Option<AccountCredential> {
        let mut account = self.accounts.read().await.get(id).cloned()?;
        account.project_quota();
        Some(account)
    }

    pub(super) async fn read_account(
        &self,
        id: &str,
    ) -> Option<tokio::sync::RwLockReadGuard<'_, AccountCredential>> {
        tokio::sync::RwLockReadGuard::try_map(self.accounts.read().await, |accounts| {
            accounts.get(id)
        })
        .ok()
    }

    pub async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.modify(id, |account| account.enabled = enabled).await
    }

    /// `reserve` is `Some(None)` to remove the reserve, `None` to keep it;
    /// `windows` is `None` to keep the watched kinds.
    pub async fn update(
        &self,
        id: &str,
        name: Option<String>,
        enabled: Option<bool>,
        hidden: Option<bool>,
        reserve: Option<Option<f64>>,
        windows: Option<Vec<String>>,
    ) -> Result<AccountSummary, SubscriptionError> {
        if reserve
            .flatten()
            .is_some_and(|percent| !valid_reserve(percent))
        {
            return Err(SubscriptionError::InvalidReserve);
        }
        let windows = match windows {
            Some(kinds) => {
                Some(reserve_windows(&kinds).ok_or(SubscriptionError::InvalidReserveWindows)?)
            }
            None => None,
        };
        self.modify(id, |account| {
            if let Some(name) = name {
                account.name = name;
            }
            if let Some(enabled) = enabled {
                account.enabled = enabled;
            }
            if let Some(hidden) = hidden {
                account.hidden = hidden;
            }
            if let Some(reserve) = reserve {
                account.reserve_percent = reserve;
            }
            if let Some(windows) = windows {
                account.reserve_windows = windows;
            }
        })
        .await
    }

    /// Routes of the custom endpoints currently configured.
    pub async fn routes(&self) -> std::collections::BTreeSet<String> {
        self.accounts
            .read()
            .await
            .values()
            .filter_map(|account| account.route.clone())
            .collect()
    }

    pub async fn eligible(
        &self,
        provider: SubscriptionProvider,
        model: Option<&str>,
    ) -> Vec<AccountCredential> {
        self.eligible_in(provider, None, model).await
    }

    /// Accounts of one pool: a provider's own accounts, or the keys of one
    /// custom endpoint route. Keys on different routes never serve each other.
    pub async fn eligible_in(
        &self,
        provider: SubscriptionProvider,
        route: Option<&str>,
        model: Option<&str>,
    ) -> Vec<AccountCredential> {
        let now = time::OffsetDateTime::now_utc();
        self.accounts_for_pool(provider, route)
            .await
            .into_iter()
            .filter(|account| {
                matches!(account.status_at(now), "ready" | "error")
                    && model.is_none_or(|model| account.serves_at(model, now))
            })
            .collect()
    }

    /// Catalog access includes enabled accounts during temporary cooldowns.
    pub(crate) async fn enabled_subscriptions(
        &self,
        provider: SubscriptionProvider,
    ) -> Vec<AccountCredential> {
        self.accounts_for_pool(provider, None)
            .await
            .into_iter()
            .filter(|account| account.enabled)
            .collect()
    }

    pub(crate) async fn accounts_for_pool(
        &self,
        provider: SubscriptionProvider,
        route: Option<&str>,
    ) -> Vec<AccountCredential> {
        self.accounts
            .read()
            .await
            .values()
            .filter(|account| account.provider == provider && account.route.as_deref() == route)
            .cloned()
            .collect()
    }

    pub(crate) async fn update_credential(
        &self,
        started: &AccountCredential,
        renewed: AccountCredential,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.modify(&renewed.id, |current| {
            if current.budget_revision != started.budget_revision
                || current.access_token != started.access_token
            {
                return;
            }
            current.access_token = renewed.access_token;
            current.refresh_token = renewed.refresh_token;
            current.id_token = renewed.id_token;
            current.expires_at = renewed.expires_at;
            current.account_id = current.account_id.take().or(renewed.account_id);
            current.organization_id = current.organization_id.take().or(renewed.organization_id);
            if current.provider == SubscriptionProvider::Meta {
                current.base_url = renewed.base_url;
                current.email = renewed.email.or_else(|| current.email.take());
                current.plan = renewed.plan;
                current.subscription = renewed.subscription;
            } else {
                current.email = current.email.take().or(renewed.email);
                current.plan = current.plan.take().or(renewed.plan);
                current.subscription = current.subscription.take().or(renewed.subscription);
            }
            current.quota.clear_error_observed_by(&started.quota);
            current.quota.refusals.retain(|refusal| {
                refusal.kind != RefusalKind::Auth
                    || !started
                        .quota
                        .refusals
                        .iter()
                        .any(|old| old.id == refusal.id)
            });
        })
        .await
    }

    pub async fn mark_cooldown(
        &self,
        id: &str,
        seconds: u64,
        error: &'static str,
    ) -> Result<AccountSummary, SubscriptionError> {
        let refusal = Refusal::timed(
            time::OffsetDateTime::now_utc(),
            seconds,
            error,
            QuotaScope::Global,
            false,
        );
        self.record_refusal(id, refusal).await
    }

    #[cfg(test)]
    pub(crate) async fn cooldown_with_retry_after(
        &self,
        id: &str,
        seconds: u64,
        error: &'static str,
        retry_after: bool,
    ) -> Result<AccountSummary, SubscriptionError> {
        let mut refusal = Refusal::timed(
            time::OffsetDateTime::now_utc(),
            seconds,
            error,
            QuotaScope::Global,
            retry_after,
        );
        refusal.legacy = true;
        self.record_refusal(id, refusal).await
    }

    #[cfg(test)]
    pub(crate) async fn scoped_cooldown(
        &self,
        id: &str,
        scope: QuotaScope,
        seconds: u64,
        retry_after: bool,
    ) -> Result<AccountSummary, SubscriptionError> {
        let mut refusal = Refusal::timed(
            time::OffsetDateTime::now_utc(),
            seconds,
            "upstream quota exceeded",
            scope,
            retry_after,
        );
        refusal.legacy = true;
        self.record_refusal(id, refusal).await
    }

    pub(crate) async fn record_refusal(
        &self,
        id: &str,
        refusal: Refusal,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.modify(id, |account| {
            if refusal
                .budget_revision
                .is_none_or(|revision| revision == account.budget_revision)
                && !super::quota::resolved_before_recording(account, &refusal)
            {
                account
                    .quota
                    .record(refusal, time::OffsetDateTime::now_utc());
            }
        })
        .await
    }

    pub async fn cooldown(
        &self,
        id: &str,
        seconds: u64,
        reason: &'static str,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.mark_cooldown(id, seconds, reason).await
    }

    pub(crate) async fn clear_error_for(&self, started: &AccountCredential) {
        if started.quota.error.is_none() {
            return;
        }
        let _ = self
            .modify(&started.id, |account| {
                if account.budget_revision == started.budget_revision {
                    account.quota.clear_error_observed_by(&started.quota);
                }
            })
            .await;
    }

    pub async fn mark_error(
        &self,
        id: &str,
        error: Option<String>,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.set_account_error(
            id,
            error.as_ref().map(|_| "provider request failed"),
            |_| true,
        )
        .await
    }

    pub(crate) async fn mark_error_for(&self, started: &AccountCredential, reason: &'static str) {
        let _ = self
            .set_account_error(&started.id, Some(reason), |current| {
                current.budget_revision == started.budget_revision
                    && current.access_token == started.access_token
            })
            .await;
    }

    async fn set_account_error(
        &self,
        id: &str,
        reason: Option<&str>,
        current_credential: impl FnOnce(&AccountCredential) -> bool,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.modify(id, |account| {
            let can_report = reason.is_none()
                || account
                    .quota
                    .active_refusals(time::OffsetDateTime::now_utc())
                    .next()
                    .is_none();
            if current_credential(account) && can_report {
                account.quota.set_error(reason);
            }
        })
        .await
    }

    pub(crate) async fn modify(
        &self,
        id: &str,
        edit: impl FnOnce(&mut AccountCredential),
    ) -> Result<AccountSummary, SubscriptionError> {
        self.modify_with(id, |_, account| {
            edit(account);
            true
        })
        .await
    }

    pub(crate) async fn modify_with(
        &self,
        id: &str,
        edit: impl FnOnce(&AccountCredential, &mut AccountCredential) -> bool,
    ) -> Result<AccountSummary, SubscriptionError> {
        let mut accounts = self.accounts.clone().write_owned().await;
        let previous = accounts.get(id).ok_or(SubscriptionError::NotFound)?;
        let mut account = previous.clone();
        let durable = edit(previous, &mut account);
        account.project_quota();
        if !durable {
            let summary = account.summary();
            accounts.insert(account.id.clone(), account);
            return Ok(summary);
        }
        account.updated_at = now();
        let directory = self.directory.clone();
        // Keep the write guard in the task so cancellation cannot publish a
        // newer account while this write is still committing to disk.
        tokio::task::spawn_blocking(move || {
            Self::persist_in(&directory, &account)?;
            let summary = account.summary();
            accounts.insert(account.id.clone(), account);
            Ok(summary)
        })
        .await
        .map_err(|_| SubscriptionError::Storage)?
    }
}
