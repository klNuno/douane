use super::*;

impl AppState {
    pub async fn candidates(
        &self,
        requested: &str,
        provider_hint: Option<&str>,
        session: Option<&str>,
    ) -> Result<(String, Vec<AccountCredential>), String> {
        self.candidates_for(requested, provider_hint, session, None)
            .await
    }

    /// The accounts `client` may use for `requested`, best first. A client
    /// sees only the accounts its access allows, and an account where the
    /// other holders' shares leave it nothing drops out like a reserve.
    pub async fn candidates_for(
        &self,
        requested: &str,
        provider_hint: Option<&str>,
        session: Option<&str>,
        client: Option<&crate::clients::Client>,
    ) -> Result<(String, Vec<AccountCredential>), String> {
        self.candidates_with_owner(requested, provider_hint, session, client, None, false, true)
            .await
    }

    /// The candidates for a request, `gemini` when it came through the Gemini
    /// API. Only that route reaches Antigravity accounts: Google bans an
    /// account used outside Antigravity, so another dialect goes to another
    /// provider of the model or is refused with [`ANTIGRAVITY_GEMINI_ONLY`].
    /// An `owner` (the account that signed a Gemini history) is the only
    /// candidate, even while it rests.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn candidates_with_owner(
        &self,
        requested: &str,
        provider_hint: Option<&str>,
        session: Option<&str>,
        client: Option<&crate::clients::Client>,
        owner: Option<&str>,
        gemini: bool,
        advance_rotation: bool,
    ) -> Result<(String, Vec<AccountCredential>), String> {
        let (model, accounts, _) = self
            .select_pool(
                requested,
                provider_hint,
                session,
                client,
                owner,
                gemini,
                advance_rotation,
            )
            .await?;
        Ok((
            model,
            accounts.into_iter().map(|(account, _)| account).collect(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn select_pool(
        &self,
        requested: &str,
        provider_hint: Option<&str>,
        session: Option<&str>,
        client: Option<&crate::clients::Client>,
        owner: Option<&str>,
        gemini: bool,
        advance_rotation: bool,
    ) -> Result<(String, Vec<(AccountCredential, Candidate)>, bool), String> {
        let config = self.config.read().await.clone();
        if config.disabled_models.contains(requested) {
            return Err("model is disabled".into());
        }
        let (model, pools) = self
            .request_pools(requested, provider_hint, gemini, &config)
            .await?;
        let request = Eligibility {
            state: self,
            model,
            client,
            settings: &config.settings,
            now: time::OffsetDateTime::now_utc(),
        };
        let mut held_back = None;
        for (provider, route) in pools {
            let pool = route.as_deref().unwrap_or(provider.as_str());
            if owner.is_some() && provider != SubscriptionProvider::Antigravity
                || config.disabled_models.contains(&format!("{pool}/{model}"))
            {
                continue;
            }
            let (eligible, resting) = request
                .pool(provider, route.as_deref(), owner, &mut held_back)
                .await;
            if eligible.is_empty() {
                continue;
            }
            let ordered = self.order_pool(
                provider,
                pool,
                session,
                &config.settings,
                eligible,
                request.now.unix_timestamp(),
                advance_rotation,
            );
            return Ok((model.into(), ordered, resting));
        }
        Err(match held_back {
            Some((pool, reset)) => share_refusal(&pool, reset),
            None => "no available subscription for this model; connect an account or wait for its quota reset".into(),
        })
    }

    async fn request_pools<'a>(
        &self,
        requested: &'a str,
        provider_hint: Option<&str>,
        gemini: bool,
        config: &Config,
    ) -> Result<(&'a str, Vec<(SubscriptionProvider, Option<String>)>), String> {
        // A prefix names a provider or a custom endpoint route; anything else
        // is part of the model id (OpenRouter ids contain slashes).
        let routes = self.accounts.routes().await;
        let pool_of = |name: &str| -> Option<(SubscriptionProvider, Option<String>)> {
            SubscriptionProvider::parse(name)
                .map(|provider| (provider, None))
                .or_else(|| {
                    routes.contains(name).then(|| {
                        (
                            SubscriptionProvider::OpenaiCompatible,
                            Some(name.to_owned()),
                        )
                    })
                })
        };
        let (prefix, model) = requested
            .split_once('/')
            .and_then(|(p, m)| pool_of(p).map(|pool| (Some(pool), m)))
            .unwrap_or((None, requested));
        let explicit = match prefix {
            Some(pool) => Some(pool),
            // A saved Antigravity default names no pool outside the Gemini
            // API: a bare id there goes to whichever other pool serves it.
            None => {
                match provider_hint.or(config.settings.default_provider.as_deref().filter(|name| {
                    gemini
                        || SubscriptionProvider::parse(name)
                            != Some(SubscriptionProvider::Antigravity)
                })) {
                    Some(name) => Some(pool_of(name).ok_or("unknown provider")?),
                    None => None,
                }
            }
        };
        let pools: Vec<(SubscriptionProvider, Option<String>)> = if let Some(pool) = explicit {
            vec![pool]
        } else {
            let mut pools = vec![];
            for m in self
                .models
                .read()
                .await
                .iter()
                .filter(|m| m.upstream_id == model && m.enabled)
            {
                let pool = (m.provider, m.route.clone());
                if !pools.contains(&pool) {
                    pools.push(pool);
                }
            }
            // Native names select the matching native provider ahead of aggregators.
            pools.sort_by_key(|(p, _)| match (model, *p) {
                (m, SubscriptionProvider::Codex) if m.starts_with("gpt-") => 0,
                (m, SubscriptionProvider::Claude) if m.starts_with("claude-") => 0,
                (m, SubscriptionProvider::Xai) if m.starts_with("grok-") => 0,
                _ => 1,
            });
            pools
        };
        let pools = if gemini {
            pools
        } else {
            let served = !pools.is_empty();
            let pools: Vec<_> = pools
                .into_iter()
                .filter(|(provider, _)| *provider != SubscriptionProvider::Antigravity)
                .collect();
            if served && pools.is_empty() {
                return Err(ANTIGRAVITY_GEMINI_ONLY.into());
            }
            pools
        };
        Ok((model, pools))
    }

    #[allow(clippy::too_many_arguments)]
    fn order_pool(
        &self,
        provider: SubscriptionProvider,
        pool: &str,
        session: Option<&str>,
        settings: &crate::settings::Settings,
        mut eligible: Vec<(AccountCredential, Candidate)>,
        now: i64,
        advance_rotation: bool,
    ) -> Vec<(AccountCredential, Candidate)> {
        let strategy = settings
            .provider_strategies
            .get(pool)
            .or_else(|| settings.provider_strategies.get(provider.as_str()))
            .copied()
            .unwrap_or(settings.strategy);
        let candidates: Vec<_> = eligible
            .iter()
            .map(|(_, candidate)| candidate.clone())
            .collect();
        let pinned = session.and_then(|key| {
            self.affinity
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(pool, key)
        });
        if let Some(index) = pinned.and_then(|id| {
            candidates.iter().position(|c| {
                c.id == id
                    && (!c.credits_only || candidates.iter().all(|c| c.credits_only))
                    && Scheduler::pin_holds(strategy, &candidates, &id, now)
            })
        }) {
            eligible.swap(0, index);
            return eligible;
        }
        let sorted = if advance_rotation {
            self.scheduler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .order(pool, strategy, &candidates, now)
        } else {
            Scheduler::default().order(pool, Strategy::FillFirst, &candidates, now)
        };
        let mut by_id: std::collections::HashMap<_, _> = eligible
            .into_iter()
            .map(|(account, candidate)| (account.id.clone(), (account, candidate)))
            .collect();
        sorted
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect()
    }

    /// Socket reuse uses the same offered pool and client rules as a new request.
    pub(crate) async fn can_reuse_socket(
        &self,
        id: &str,
        provider: SubscriptionProvider,
        model: &str,
        session: Option<&str>,
        client: Option<&crate::clients::Client>,
    ) -> bool {
        let Ok((_, candidates, resting)) = self
            .select_pool(
                model,
                Some(provider.as_str()),
                session,
                client,
                None,
                false,
                false,
            )
            .await
        else {
            return false;
        };
        let Some((_, candidate)) = candidates.iter().find(|(account, _)| account.id == id) else {
            return false;
        };
        if resting && !candidates.first().is_some_and(|(first, _)| first.id == id) {
            return false;
        }
        !candidate.credits_only
            || candidates
                .iter()
                .all(|(_, candidate)| candidate.credits_only)
    }

    /// The client may not use this account: its access refuses it, or the
    /// other holders' shares leave it nothing there.
    pub async fn client_excludes(
        &self,
        client: Option<&crate::clients::Client>,
        account: &AccountCredential,
        model: &str,
    ) -> bool {
        if client.is_some_and(|c| !c.may_use(account)) {
            return true;
        }
        let release = self.config.read().await.settings.release();
        if let Some(c) = client {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            if !self.borrow_open(c, account, model, now, release).await {
                return true;
            }
        }
        let snapshot = self.quotas.get(&account.id).await;
        self.clients
            .share_blocks_for(
                client,
                account,
                snapshot.as_ref(),
                time::OffsetDateTime::now_utc().unix_timestamp(),
                release,
                model,
            )
            .is_some()
    }

    /// Whether `client` may call `model` on `account` as far as borrowing
    /// goes: always on an account its access allows; on a borrowed one only
    /// for a model family whose week its home account holds back, while the
    /// home account's own week still leaves it room.
    async fn borrow_open(
        &self,
        client: &crate::clients::Client,
        account: &AccountCredential,
        model: &str,
        now: i64,
        release: crate::clients::Release,
    ) -> bool {
        let Some(home) = client.borrowed_home(account) else {
            return true;
        };
        let Some(family) = crate::subscriptions::cooldown::claude_family(model)
            .or_else(|| matches!(model, "fable" | "opus" | "sonnet").then_some(model))
        else {
            return false;
        };
        let Some(home) = self.accounts.get(home).await else {
            return false;
        };
        if !client.access.allows(&home) {
            return false;
        }
        let Some(quota) = self.quotas.get(&home.id).await else {
            return false;
        };
        if crate::clients::account_week_spent(&quota, now)
            || self
                .clients
                .share_blocks_for(Some(client), &home, Some(&quota), now, release, "")
                .is_some()
        {
            return false;
        }
        quota.family_week_spent(family, now)
            || self
                .clients
                .share_blocks_for(Some(client), &home, Some(&quota), now, release, model)
                .is_some()
    }
}

fn share_refusal(pool: &str, reset: i64) -> String {
    let until = time::OffsetDateTime::from_unix_timestamp(reset)
        .ok()
        .map(crate::subscriptions::timestamp)
        .unwrap_or_default();
    format!(
        "{} {pool} is used up until {until}; its overbooked share cap is reached or quota is reserved for other clients",
        crate::clients::SHARE_USED_UP
    )
}

struct Eligibility<'a> {
    state: &'a AppState,
    model: &'a str,
    client: Option<&'a crate::clients::Client>,
    settings: &'a crate::settings::Settings,
    now: time::OffsetDateTime,
}

enum Exclusion {
    Unavailable,
    ClientShare(i64),
}

impl Eligibility<'_> {
    async fn pool(
        &self,
        provider: SubscriptionProvider,
        route: Option<&str>,
        owner: Option<&str>,
        held_back: &mut Option<(String, i64)>,
    ) -> (Vec<(AccountCredential, Candidate)>, bool) {
        let accounts = self.state.accounts.accounts_for_pool(provider, route).await;
        let mut available = Vec::new();
        let mut resting = Vec::new();
        for account in accounts {
            if !self.offers(&account, owner) {
                continue;
            }
            match self.evaluate(&account, owner.is_some()).await {
                Ok(candidate) => {
                    let pool = if owner.is_none() && account.resting_at(self.now) {
                        &mut resting
                    } else {
                        &mut available
                    };
                    pool.push((account, candidate));
                }
                Err(Exclusion::ClientShare(reset))
                    if held_back.as_ref().is_none_or(|(_, old)| reset < *old) =>
                {
                    *held_back = Some((account.pool().into(), reset));
                }
                _ => {}
            }
        }
        if available.is_empty() {
            resting.sort_by_key(|(account, _)| account.retry_at(self.model));
            (resting, true)
        } else {
            (available, false)
        }
    }

    fn offers(&self, account: &AccountCredential, owner: Option<&str>) -> bool {
        let status = account.status_at(self.now);
        if let Some(owner) = owner {
            return account.id == owner
                && account.offers_model(self.model)
                && matches!(status, "ready" | "error" | "cooldown" | "exhausted");
        }
        account.serves_at(self.model, self.now)
            && (matches!(status, "ready" | "error") || account.resting_at(self.now))
    }

    async fn evaluate(
        &self,
        account: &AccountCredential,
        owner: bool,
    ) -> Result<Candidate, Exclusion> {
        use Exclusion::*;
        if self.client.is_some_and(|c| !c.may_use(account))
            || !self.state.serves(&account.id, self.model, account.provider)
        {
            return Err(Unavailable);
        }
        let now = self.now.unix_timestamp();
        let release = self.settings.release();
        if let Some(client) = self.client
            && !self
                .state
                .borrow_open(client, account, self.model, now, release)
                .await
        {
            return Err(Unavailable);
        }
        let quota = self.state.quotas.for_account(account).await;
        if !owner
            && quota
                .as_ref()
                .is_some_and(|q| q.window_exhausted(self.now) && !q.usable_credits())
            || self.state.reserve_excludes_with(
                account,
                quota.as_ref(),
                self.now,
                self.settings.free_before_reset_seconds(),
            )
        {
            return Err(Unavailable);
        }
        if let Some(reset) = self.state.clients.share_blocks_for(
            self.client,
            account,
            quota.as_ref(),
            now,
            release,
            self.model,
        ) {
            return Err(ClientShare(reset));
        }
        Ok(self.rank(account, quota.as_ref()))
    }

    fn rank(
        &self,
        account: &AccountCredential,
        quota: Option<&crate::subscriptions::QuotaSnapshot>,
    ) -> Candidate {
        let now = self.now.unix_timestamp();
        let configured_order = self
            .settings
            .account_order
            .get(account.pool())
            .or_else(|| self.settings.account_order.get(account.provider.as_str()));
        let preferred = self.settings.keep_fable
            && account.provider == SubscriptionProvider::Claude
            && crate::subscriptions::cooldown::claude_family(self.model) != Some("fable")
            && quota.is_some_and(|q| {
                q.family_week_spent("fable", now)
                    || self
                        .state
                        .clients
                        .share_blocks_for(
                            self.client,
                            account,
                            Some(q),
                            now,
                            self.settings.release(),
                            "fable",
                        )
                        .is_some()
            });
        Candidate {
            id: account.id.clone(),
            remaining_percent: quota.and_then(|q| {
                q.weekly_remaining_percent()
                    .or_else(|| q.remaining_percent())
            }),
            weekly_reset: quota
                .and_then(|q| q.weekly_reset())
                .and_then(crate::subscriptions::parse_timestamp)
                .map(|t| t.unix_timestamp()),
            credits_only: quota.is_some_and(|q| q.credits_only(self.now)),
            rank: configured_order.and_then(|ids| ids.iter().position(|id| id == &account.id)),
            preferred,
        }
    }
}
