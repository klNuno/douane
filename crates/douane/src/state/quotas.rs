use super::*;

impl AppState {
    /// window at or below it takes the account out of rotation until that
    /// window resets. Unknown quotas never trigger it. This check only reads;
    /// request routing uses [`Self::reserve_excludes`].
    pub async fn reserve_reached(&self, account: &AccountCredential) -> bool {
        let free_before = self.free_before_reset_seconds().await;
        let snapshot = self.quotas.get(&account.id).await;
        reserve_reached_in(
            account.reserve(),
            &account.reserve_windows(),
            snapshot.as_ref(),
            time::OffsetDateTime::now_utc(),
            free_before,
        )
    }

    /// Routing check for the reserve. Close to or at the reserve, a snapshot
    pub async fn reserve_excludes(&self, account: &AccountCredential) -> bool {
        let free_before = self.free_before_reset_seconds().await;
        let snapshot = self.quotas.get(&account.id).await;
        self.reserve_excludes_with(
            account,
            snapshot.as_ref(),
            time::OffsetDateTime::now_utc(),
            free_before,
        )
    }

    /// Seconds before an account's weekly reset when its reserve and client
    /// shares stop applying.
    pub async fn free_before_reset_seconds(&self) -> i64 {
        self.config
            .read()
            .await
            .settings
            .free_before_reset_seconds()
    }

    pub(super) fn reserve_excludes_with(
        &self,
        account: &AccountCredential,
        snapshot: Option<&crate::subscriptions::QuotaSnapshot>,
        now: time::OffsetDateTime,
        free_before: i64,
    ) -> bool {
        let (Some(reserve), Some(snapshot)) = (account.reserve(), snapshot) else {
            return false;
        };
        if snapshot.longest_window_ends_within(free_before, now.unix_timestamp()) {
            return false;
        }
        let windows = account.reserve_windows();
        let near = snapshot
            .reserve_window_remaining(&windows, now)
            .is_some_and(|lowest| lowest <= reserve + 10.0);
        let stale = crate::subscriptions::parse_timestamp(&snapshot.updated_at)
            .is_none_or(|at| now - at > time::Duration::minutes(1));
        if near && stale {
            self.refresh_soon(&account.id);
        }
        reserve_reached_in(Some(reserve), &windows, Some(snapshot), now, 0)
    }

    fn refresh_soon(&self, id: &str) {
        let started = std::time::Instant::now();
        let throttle = std::time::Duration::from_secs(60);
        {
            let mut last = self
                .reserve_refresh
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Expired entries go, so deleted accounts do not accumulate.
            last.retain(|_, at| started.duration_since(*at) < throttle);
            if last.contains_key(id) {
                return;
            }
            last.insert(id.to_owned(), started);
        }
        let quotas = self.quotas.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let _ = quotas.refresh_background(&id).await;
        });
    }

    /// A read asked by hand, for one account or all of them: a failure shows
    /// at once.
    pub async fn refresh_quotas(&self, id: Option<&str>) {
        self.refresh_quotas_matching(|account| id.is_none_or(|id| account.id == id))
            .await;
    }

    pub(crate) async fn refresh_quotas_matching(&self, matches: impl Fn(&AccountSummary) -> bool) {
        use futures_util::{StreamExt, stream};
        let ids: Vec<String> = self
            .accounts
            .list()
            .await
            .into_iter()
            .filter(matches)
            .map(|a| a.id)
            .collect();
        stream::iter(ids)
            .for_each_concurrent(4, |id| async move {
                let _ = self.quotas.refresh(&id).await;
                self.observe_usage(&id).await;
            })
            .await;
    }

    /// Attributes the latest rise of an account's windows to its clients.
    pub async fn observe_usage(&self, id: &str) {
        if let Some(snapshot) = self.quotas.get(id).await {
            self.clients
                .observe(&snapshot, time::OffsetDateTime::now_utc().unix_timestamp());
        }
    }

    /// The poller's pass: reads only the accounts whose snapshot is
    /// `quota_refresh_seconds` old or whose retry delay has passed.
    pub async fn refresh_due_quotas(&self) {
        use futures_util::{StreamExt, stream};
        let interval = self.config.read().await.settings.quota_refresh_seconds;
        let interval = time::Duration::seconds(interval as i64);
        let ids = self
            .quotas
            .due(interval, time::OffsetDateTime::now_utc())
            .await;
        stream::iter(ids)
            .for_each_concurrent(4, |id| async move {
                if self.quotas.refresh_background(&id).await.is_some() {
                    self.observe_usage(&id).await;
                }
            })
            .await;
        let accounts = self.accounts.list().await;
        let existing = accounts.iter().map(|a| a.id.clone()).collect();
        let clients = self.clients.clone();
        let _ = tokio::task::spawn_blocking(move || {
            clients.forget_accounts(&existing);
            clients.save_usage();
            if let Err(error) = clients.drop_unreachable_shares(&accounts) {
                tracing::warn!(%error, "unreachable client shares were not dropped");
            }
        })
        .await;
    }
}

/// A valid reserve and a snapshot whose account-wide window sits at or below
/// it, unless the weekly reset is within `free_before` seconds. Shared by
/// routing and the management listing.
pub fn reserve_reached_in(
    reserve: Option<f64>,
    windows: &[String],
    snapshot: Option<&crate::subscriptions::QuotaSnapshot>,
    now: time::OffsetDateTime,
    free_before: i64,
) -> bool {
    reserve
        .filter(|percent| crate::subscriptions::valid_reserve(*percent))
        .zip(snapshot)
        .filter(|(_, snapshot)| {
            !snapshot.longest_window_ends_within(free_before, now.unix_timestamp())
        })
        .is_some_and(|(reserve, snapshot)| snapshot.reserve_reached(reserve, windows, now))
}
