use super::*;
/// Who asked for a quota read. A read asked by hand reports its failure at
/// once; the background poller stays quiet about transient failures until
/// [`FAILURES_SHOWN`] of them in a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Manual,
    Background,
}

/// Consecutive failed reads before a transient error reaches the snapshot.
pub const FAILURES_SHOWN: u32 = 5;
const RETRY_FIRST_SECONDS: i64 = 30;
const RETRY_MAX_SECONDS: i64 = 30 * 60;

/// Wait after the `count`-th consecutive failure: 30 s, doubling, capped at
/// 30 minutes. The fifth failure lands about 7.5 minutes after the first.
pub fn retry_delay(count: u32) -> time::Duration {
    let doublings = count.saturating_sub(1).min(16);
    time::Duration::seconds((RETRY_FIRST_SECONDS << doublings).min(RETRY_MAX_SECONDS))
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Failures {
    pub(super) count: u32,
    pub(super) retry_at: time::OffsetDateTime,
    pub(super) began: time::OffsetDateTime,
}

impl Failures {
    fn after(
        previous: Option<Self>,
        observed: time::OffsetDateTime,
        began: time::OffsetDateTime,
    ) -> Self {
        let count = previous.map_or(1, |failure| failure.count.saturating_add(1));
        Self {
            count,
            retry_at: observed + retry_delay(count),
            began,
        }
    }
}

pub(super) struct BillingReading {
    pub(super) revision: u64,
    pub(super) snapshot: QuotaSnapshot,
    pub(super) failure: Option<Failures>,
}

fn quota_read_due(
    snapshot: Option<&QuotaSnapshot>,
    failure: Option<Failures>,
    interval: time::Duration,
    now: time::OffsetDateTime,
) -> bool {
    match failure {
        Some(failure) => failure.retry_at <= now,
        None => snapshot
            .and_then(|snapshot| parse_timestamp(&snapshot.updated_at))
            .is_none_or(|at| now - at >= interval),
    }
}

fn failed_read(
    account: &AccountCredential,
    previous: Option<&QuotaSnapshot>,
    error: SubscriptionError,
    failure: Failures,
    trigger: Trigger,
) -> QuotaSnapshot {
    // Keep the last observation dated as such. Anthropic can rate-limit
    // usage reads for days while inference supplies fresher windows.
    let quiet = matches!(error, SubscriptionError::Upstream(429))
        && previous.is_some_and(|value| !value.windows.is_empty());
    let shown = previous.is_some_and(|value| value.error.is_some())
        || (!quiet
            && (trigger == Trigger::Manual
                || !error.is_transient()
                || failure.count >= FAILURES_SHOWN));
    if !shown {
        tracing::debug!(
            account = account.id,
            count = failure.count,
            "quota read failed, retrying quietly"
        );
    }
    let mut snapshot = previous.cloned().unwrap_or_else(|| QuotaSnapshot {
        account_id: account.id.clone(),
        provider: account.provider,
        windows: vec![],
        updated_at: now(),
        error: None,
        subscription: account.summary().subscription,
        credits: vec![],
        verification_required: false,
        verification_available: false,
    });
    if snapshot.subscription.is_none() {
        snapshot.subscription = account.summary().subscription;
    }
    snapshot.verification_required = error.is_verification_required();
    snapshot.verification_available = error.verification_url().is_some();
    snapshot.error = shown.then(|| error.to_string());
    snapshot
}

impl QuotaService {
    /// A read asked by hand: it always reaches the provider and a failure
    /// shows at once.
    pub async fn refresh(&self, id: &str) -> Result<QuotaSnapshot, SubscriptionError> {
        self.refresh_with(id, Trigger::Manual).await
    }

    /// A read nobody asked for: skipped while the account waits for its next
    /// retry, `None` then.
    pub async fn refresh_background(
        &self,
        id: &str,
    ) -> Option<Result<QuotaSnapshot, SubscriptionError>> {
        let now = time::OffsetDateTime::now_utc();
        if self
            .failures
            .read()
            .await
            .get(id)
            .is_some_and(|failures| failures.retry_at > now)
        {
            return None;
        }
        let waiting_revision = self
            .billing
            .read()
            .await
            .get(id)
            .filter(|reading| {
                reading
                    .failure
                    .is_some_and(|failure| failure.retry_at > now)
            })
            .map(|reading| reading.revision);
        if let Some(revision) = waiting_revision
            && self
                .store
                .read_account(id)
                .await
                .is_some_and(|account| account.endpoint_revision == revision)
        {
            return None;
        }
        Some(self.refresh_with(id, Trigger::Background).await)
    }

    /// Accounts the poller should read now: a failing one once its retry
    /// delay has passed, a healthy one once its snapshot is `interval` old.
    /// Retry state of deleted accounts goes.
    pub async fn due(&self, interval: time::Duration, now: time::OffsetDateTime) -> Vec<String> {
        let accounts = self.store.list().await;
        let due_accounts: Vec<_> = accounts
            .iter()
            .filter(|account| {
                Self::supports(account)
                    && account.provider != SubscriptionProvider::Meta
                    && (account.provider != SubscriptionProvider::OpenaiCompatible
                        || account.enabled)
            })
            .collect();
        let mut failures = self.failures.write().await;
        failures.retain(|id, _| due_accounts.iter().any(|account| account.id == *id));
        let cache = self.cache.read().await;
        let mut billing = self.billing.write().await;
        billing.retain(|id, _| accounts.iter().any(|account| account.id == *id));
        due_accounts
            .into_iter()
            .filter(|account| {
                if account.provider == SubscriptionProvider::OpenaiCompatible {
                    let reading = billing
                        .get(&account.id)
                        .filter(|reading| reading.revision == account.endpoint_revision);
                    quota_read_due(
                        reading.map(|reading| &reading.snapshot),
                        reading.and_then(|reading| reading.failure),
                        interval,
                        now,
                    )
                } else {
                    quota_read_due(
                        cache.get(&account.id),
                        failures.get(&account.id).copied(),
                        interval,
                        now,
                    )
                }
            })
            .map(|account| account.id.clone())
            .collect()
    }

    pub(super) async fn refresh_with(
        &self,
        id: &str,
        trigger: Trigger,
    ) -> Result<QuotaSnapshot, SubscriptionError> {
        let mut began = time::OffsetDateTime::now_utc();
        let account = self
            .store
            .get(id)
            .await
            .ok_or(SubscriptionError::NotFound)?;
        if account.provider == SubscriptionProvider::Meta {
            let mut usage = self.oauth.take_meta_usage(id).await;
            if usage.is_none() && account.dca_token.is_some() {
                // The native key response includes subs_usage. Renew through
                // the shared credential lock and persist the key before use.
                if let Err(error) = self.oauth.force_refresh(id).await {
                    return self
                        .complete_refresh_at(id, &account, Err(error), trigger, began)
                        .await;
                }
                usage = self.oauth.take_meta_usage(id).await;
            }
            if let Some((observed, event)) = usage {
                self.observe_meta_event_at(id, &event, observed).await;
            }
            return Ok(self.get(id).await.unwrap_or(QuotaSnapshot {
                account_id: id.into(),
                provider: account.provider,
                windows: vec![],
                updated_at: now(),
                error: None,
                subscription: account.subscription,
                credits: vec![],
                verification_required: false,
                verification_available: false,
            }));
        }
        if account.provider == SubscriptionProvider::OpenaiCompatible {
            let api = BillingApi::for_base_url(account.base_url.as_deref())
                .ok_or(SubscriptionError::Unsupported)?;
            let result = self.fetch_billing(&account, api).await;
            return self
                .complete_refresh_at(id, &account, result, trigger, began)
                .await;
        }
        // Nothing to read: no request, and no error snapshot to show.
        if !account.provider.reports_quota() {
            return Err(SubscriptionError::Unsupported);
        }
        match self.oauth.credential(id).await {
            Ok(mut credential) => {
                began = time::OffsetDateTime::now_utc();
                let mut result = self.fetch(&credential).await;
                // A provider can revoke an access token before its expiry
                // (ChatGPT does for Codex). Any OAuth provider renews once.
                if matches!(result, Err(SubscriptionError::Upstream(401)))
                    && credential.refresh_token.is_some()
                {
                    match self.oauth.renew_refused(id, &credential.access_token).await {
                        Ok(fresh) => {
                            credential = fresh;
                            began = time::OffsetDateTime::now_utc();
                            result = self.fetch(&credential).await;
                        }
                        Err(error) => result = Err(error),
                    }
                }
                // Capture the active refusal identities and budget revision
                // from the credential that started the usage read.
                self.complete_refresh_at(id, &credential, result, trigger, began)
                    .await
            }
            Err(error) => {
                self.complete_refresh_at(id, &account, Err(error), trigger, began)
                    .await
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn complete_refresh(
        &self,
        id: &str,
        account: &AccountCredential,
        result: Result<Observation, SubscriptionError>,
        trigger: Trigger,
    ) -> Result<QuotaSnapshot, SubscriptionError> {
        let began = result
            .as_ref()
            .map(|obs| obs.received)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        self.complete_refresh_at(id, account, result, trigger, began)
            .await
    }

    async fn complete_refresh_at(
        &self,
        id: &str,
        account: &AccountCredential,
        result: Result<Observation, SubscriptionError>,
        trigger: Trigger,
        began: time::OffsetDateTime,
    ) -> Result<QuotaSnapshot, SubscriptionError> {
        if account.provider == SubscriptionProvider::OpenaiCompatible {
            return self.complete_billing(id, account, result, trigger).await;
        }
        match result {
            Ok(observation) => {
                self.publish_observation(account, observation, began, true)
                    .await
            }
            Err(error) => {
                let _guard = self.transitions.lock().await;
                let current = self
                    .store
                    .read_account(id)
                    .await
                    .ok_or(SubscriptionError::NotFound)?;
                if current.budget_revision != account.budget_revision {
                    return Err(SubscriptionError::NotFound);
                }
                let mut failures = self.failures.write().await;
                let mut cache = self.cache.write().await;
                let previous = super::state::snapshot_for(&current, cache.get(id).cloned());
                if let Some(previous) = previous.as_ref()
                    && newer_than(&previous.updated_at, began)
                {
                    return Ok(previous.clone());
                }
                if failures
                    .get(id)
                    .is_some_and(|failure| failure.began > began)
                {
                    return previous.ok_or(error);
                }
                let failure = Failures::after(
                    failures.get(id).copied(),
                    time::OffsetDateTime::now_utc(),
                    began,
                );
                failures.insert(id.into(), failure);
                let mut verification_urls = self.verification_urls.write().await;
                if let Some(url) = error.verification_url() {
                    verification_urls.insert(id.into(), url.to_owned());
                } else {
                    verification_urls.remove(id);
                }
                let snapshot = failed_read(account, previous.as_ref(), error, failure, trigger);
                cache.insert(id.into(), snapshot.clone());
                Ok(snapshot)
            }
        }
    }

    pub(super) async fn complete_billing(
        &self,
        id: &str,
        account: &AccountCredential,
        result: Result<Observation, SubscriptionError>,
        trigger: Trigger,
    ) -> Result<QuotaSnapshot, SubscriptionError> {
        // Hold the account read lock through publication so a change to the
        // endpoint and this read cannot update the same revision concurrently.
        let current = self
            .store
            .read_account(id)
            .await
            .ok_or(SubscriptionError::NotFound)?;
        if current.endpoint_revision != account.endpoint_revision {
            return Err(SubscriptionError::NotFound);
        }
        let mut billing = self.billing.write().await;
        let (snapshot, failure) = match result {
            Ok(observation) => (
                QuotaSnapshot {
                    account_id: id.into(),
                    provider: account.provider,
                    windows: vec![],
                    updated_at: now(),
                    error: None,
                    subscription: None,
                    credits: observation.credits,
                    verification_required: false,
                    verification_available: false,
                },
                None,
            ),
            Err(error) => {
                let previous = billing
                    .get(id)
                    .filter(|reading| reading.revision == account.endpoint_revision);
                let failure = Failures::after(
                    previous.and_then(|reading| reading.failure),
                    time::OffsetDateTime::now_utc(),
                    time::OffsetDateTime::now_utc(),
                );
                (
                    failed_read(
                        account,
                        previous.map(|reading| &reading.snapshot),
                        error,
                        failure,
                        trigger,
                    ),
                    Some(failure),
                )
            }
        };
        billing.insert(
            id.into(),
            BillingReading {
                revision: account.endpoint_revision,
                snapshot: snapshot.clone(),
                failure,
            },
        );
        Ok(snapshot)
    }
}
