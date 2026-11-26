use super::endpoint::{DIALECT_RULE, IMAGE_LIMIT_RULE, ROUTE_RULE};
use super::*;
impl AccountStore {
    pub async fn import(
        &self,
        value: Value,
        name: Option<String>,
    ) -> Result<AccountSummary, SubscriptionError> {
        let explicit_name = name.as_deref().is_some_and(|name| !name.trim().is_empty())
            || text(&value, &["name", "label"]).is_some();
        let mut account = parse_import(value, name)?;
        let mut accounts = self.accounts.write().await;
        // Reimporting the same identity rotates its tokens without duplicating it
        // or silently re-enabling a credential the user disabled.
        if let Some(previous) = accounts
            .values()
            .find(|existing| same_identity(existing, &account))
        {
            account.id = previous.id.clone();
            account.created_at = previous.created_at.clone();
            account.enabled = previous.enabled;
            account.reserve_percent = previous.reserve_percent;
            account.reserve_windows = previous.reserve_windows.clone();
            account.quota = previous.quota.clone();
            account.quota.refusals.clear();
            account.quota.set_error(None);
            // An explicit reimport reconnects the account. Refusals from
            // already-sent requests still belong to the previous connection.
            account.budget_revision = rand::random();
            account.project_quota();
            if !explicit_name {
                account.name = previous.name.clone();
            }
            if account.subscription.is_none() && account.plan.is_none() {
                account.subscription = previous.subscription.clone();
                account.plan = previous.plan.clone();
            }
            account.organization_id = account
                .organization_id
                .or_else(|| previous.organization_id.clone());
        }
        self.persist(&account)?;
        let summary = account.summary();
        accounts.insert(account.id.clone(), account);
        Ok(summary)
    }

    /// Every stored record, tokens included, for a configuration export.
    /// This is the one deliberate exit of credentials from the broker: the
    /// export route sends them to the operator who asked for a backup.
    pub async fn export(&self) -> Vec<AccountCredential> {
        self.accounts
            .read()
            .await
            .values()
            .cloned()
            .map(|mut account| {
                account.project_quota();
                account
            })
            .collect()
    }

    /// Restores one record of an export. A record naming a stored account by
    /// id, or by identity as `import` matches it, replaces that account in
    /// place under its local id; any other record is added. Cooldowns and
    /// errors belong to the instance that recorded them and are cleared.
    pub async fn restore(&self, record: Value) -> Result<AccountSummary, SubscriptionError> {
        let mut accounts = self.accounts.write().await;
        let account = prepare_restore(record, &accounts)?;
        self.save_restored_account(account, &mut accounts)
    }

    pub(crate) async fn restore_batch(
        &self,
        records: &[Value],
        validate: impl FnOnce(
            &BTreeMap<String, String>,
            &std::collections::BTreeSet<String>,
        ) -> Result<(), String>,
    ) -> Result<
        (
            Vec<Result<AccountSummary, SubscriptionError>>,
            BTreeMap<String, String>,
            std::collections::BTreeSet<String>,
        ),
        String,
    > {
        let mut accounts = self.accounts.write().await;
        let mut proposed = accounts.clone();
        let mut remapped = BTreeMap::new();
        let mut prepare_missing = std::collections::BTreeSet::new();
        let pending: Vec<_> = records
            .iter()
            .map(|record| {
                let old = record.get("id").and_then(Value::as_str).map(str::to_owned);
                let account = match prepare_restore(record.clone(), &proposed) {
                    Ok(account) => {
                        if let Some(old) = &old {
                            remapped.insert(old.clone(), account.id.clone());
                        }
                        proposed.insert(account.id.clone(), account.clone());
                        Ok(account)
                    }
                    Err(error) => {
                        if let Some(old) = old.as_ref().filter(|id| !accounts.contains_key(*id)) {
                            prepare_missing.insert(old.clone());
                        }
                        Err(error)
                    }
                };
                (old, account)
            })
            .collect();
        for (old, target) in &remapped {
            prepare_missing.remove(old);
            prepare_missing.remove(target);
        }
        validate(&remapped, &prepare_missing)?;
        let mut applied = BTreeMap::new();
        let mut missing = std::collections::BTreeSet::new();
        let results = pending
            .into_iter()
            .map(|(old, entry)| match entry {
                Ok(account) => {
                    let target = account.id.clone();
                    match self.save_restored_account(account, &mut accounts) {
                        Ok(summary) => {
                            if let Some(old) = old {
                                applied.insert(old, summary.id.clone());
                            }
                            Ok(summary)
                        }
                        Err(error) => {
                            if !accounts.contains_key(&target) {
                                missing.insert(target);
                            }
                            if let Some(old) = old.filter(|id| !accounts.contains_key(id)) {
                                missing.insert(old);
                            }
                            Err(error)
                        }
                    }
                }
                Err(error) => {
                    if let Some(old) = old.filter(|id| !accounts.contains_key(id)) {
                        missing.insert(old);
                    }
                    Err(error)
                }
            })
            .collect();
        for (old, target) in &applied {
            missing.remove(old);
            missing.remove(target);
        }
        // A failed remap must not fall back to an unrelated local account
        // with the same id as the exported record.
        missing.extend(remapped.into_iter().filter_map(|(old, target)| {
            (old != target && applied.get(&old) != Some(&target)).then_some(old)
        }));
        Ok((results, applied, missing))
    }

    fn save_restored_account(
        &self,
        account: AccountCredential,
        accounts: &mut BTreeMap<String, AccountCredential>,
    ) -> Result<AccountSummary, SubscriptionError> {
        self.persist(&account)?;
        let summary = account.summary();
        accounts.insert(account.id.clone(), account);
        Ok(summary)
    }
}

fn prepare_restore(
    record: Value,
    accounts: &BTreeMap<String, AccountCredential>,
) -> Result<AccountCredential, SubscriptionError> {
    let mut account: AccountCredential = serde_json::from_value(record)
        .map_err(|_| SubscriptionError::InvalidImport("invalid account record"))?;
    check_record(&account)?;
    account.quota = AccountQuota::new();
    account.quota.cleared_at = Some(now());
    account.budget_revision = rand::random();
    account.project_quota();
    let previous = accounts
        .get(&account.id)
        .filter(|existing| existing.provider == account.provider)
        .or_else(|| {
            accounts
                .values()
                .find(|existing| same_identity(existing, &account))
        });
    if let Some(previous) = previous {
        account.id = previous.id.clone();
        account.created_at = previous.created_at.clone();
        account.quota.serial = previous.quota.serial;
    } else if !valid_id(&account.id) || accounts.contains_key(&account.id) {
        account.id = random_id();
    }
    if parse_timestamp(&account.created_at).is_none() {
        account.created_at = now();
    }
    account.updated_at = now();
    if account.provider == SubscriptionProvider::OpenaiCompatible {
        account.endpoint_revision = rand::random();
    }
    Ok(account)
}
/// Two records name the same sign-in: same provider and same account id or
/// email. Custom endpoints carry neither and never match this way.
pub(super) fn same_identity(existing: &AccountCredential, account: &AccountCredential) -> bool {
    existing.provider == account.provider
        && ((account.account_id.is_some() && existing.account_id == account.account_id)
            || (account.email.is_some() && existing.email == account.email))
}

/// The checks `parse_import` applies, for a stored record coming back from
/// an export.
pub(super) fn check_record(account: &AccountCredential) -> Result<(), SubscriptionError> {
    if account.provider == SubscriptionProvider::Meta
        && account
            .base_url
            .as_deref()
            .is_some_and(|url| !crate::subscriptions::meta::valid_api_base(url))
    {
        return Err(SubscriptionError::InvalidImport("untrusted Meta base URL"));
    }
    let token = account.access_token.trim();
    if token.is_empty() || token.len() > 65_536 || token.chars().any(char::is_control) {
        return Err(SubscriptionError::InvalidImport("missing access token"));
    }
    if account.name.trim().is_empty()
        || account.name.len() > 200
        || account.email.as_ref().is_some_and(|v| v.len() > 320)
    {
        return Err(SubscriptionError::InvalidImport(
            "account label is missing or too long",
        ));
    }
    if account
        .expires_at
        .as_deref()
        .is_some_and(|t| parse_timestamp(t).is_none())
    {
        return Err(SubscriptionError::InvalidImport("invalid expiry timestamp"));
    }
    if account
        .base_url
        .as_deref()
        .is_some_and(|url| !valid_base_url(url))
    {
        return Err(SubscriptionError::InvalidImport("invalid base URL"));
    }
    if !account.metadata.is_empty()
        && (account.provider != SubscriptionProvider::OpenaiCompatible
            || !crate::endpoint_metadata::valid(&account.metadata))
    {
        return Err(SubscriptionError::InvalidEndpoint(
            crate::endpoint_metadata::RULE,
        ));
    }
    if account.route.is_some() || account.dialect.is_some() || account.image_limit.is_some() {
        if account.provider != SubscriptionProvider::OpenaiCompatible {
            return Err(SubscriptionError::InvalidEndpoint(
                "route, dialect and image_limit apply only to openai_compatible endpoints",
            ));
        }
        if account
            .image_limit
            .is_some_and(|limit| !valid_image_limit(limit))
        {
            return Err(SubscriptionError::InvalidEndpoint(IMAGE_LIMIT_RULE));
        }
        if account.route.as_deref().is_some_and(|r| !valid_route(r)) {
            return Err(SubscriptionError::InvalidEndpoint(ROUTE_RULE));
        }
        if account
            .dialect
            .as_deref()
            .is_some_and(|d| !valid_dialect(d))
        {
            return Err(SubscriptionError::InvalidEndpoint(DIALECT_RULE));
        }
        if account.route.is_some() && account.base_url.is_none() {
            return Err(SubscriptionError::InvalidEndpoint(
                "an endpoint with a route needs its base_url",
            ));
        }
    }
    Ok(())
}

pub(crate) fn text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned)
    })
}

pub(crate) fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn apply_identity(account: &mut AccountCredential) {
    let claims = account
        .id_token
        .as_deref()
        .and_then(jwt_claims)
        .or_else(|| jwt_claims(&account.access_token));
    if let Some(claims) = claims {
        // JWT claims are identity hints only. They do not authorize a request.
        account.email = account.email.take().or_else(|| text(&claims, &["email"]));
        let auth = claims.get("https://api.openai.com/auth").unwrap_or(&claims);
        account.account_id = account
            .account_id
            .take()
            .or_else(|| text(auth, &["chatgpt_account_id", "account_id"]));
        if account.provider == SubscriptionProvider::Xai {
            account.account_id = account
                .account_id
                .take()
                .or_else(|| text(&claims, &["sub"]));
        }
        account.plan = account
            .plan
            .take()
            .or_else(|| text(auth, &["chatgpt_plan_type", "plan_type"]));
        if account.subscription.is_none() {
            account.subscription =
                subscription_from_value(account.provider, auth, "identity_claim");
        }
    }
    // Identity tokens and bearer tokens can have different lifetimes. Only
    // the bearer token's expiration governs when an inference call refreshes.
    if account.expires_at.is_none() {
        account.expires_at = jwt_claims(&account.access_token)
            .and_then(|claims| claims.get("exp").and_then(Value::as_i64))
            .and_then(|exp| time::OffsetDateTime::from_unix_timestamp(exp).ok())
            .map(timestamp);
    }
}

pub(super) fn parse_import(
    mut value: Value,
    name: Option<String>,
) -> Result<AccountCredential, SubscriptionError> {
    if !value.is_object() {
        return Err(SubscriptionError::InvalidImport("expected a JSON object"));
    }
    // Grok CLI stores OAuth under an issuer/client compound key. Import the
    // selected record rather than treating arbitrary siblings as credentials.
    if let Some(native) = value
        .get("https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828")
        .cloned()
    {
        value = native;
        let object = value
            .as_object_mut()
            .ok_or(SubscriptionError::InvalidImport(
                "invalid Grok credential record",
            ))?;
        object.insert("type".into(), serde_json::json!("xai"));
    }
    // OpenCode keeps its GitHub Copilot login under the provider's id; the
    // GitHub OAuth token serves the Copilot API directly and does not expire.
    if let Some(native) = value
        .get("github-copilot")
        .filter(|record| record.is_object())
        .cloned()
    {
        let token = text(&native, &["refresh", "access", "access_token"]).ok_or(
            SubscriptionError::InvalidImport("missing GitHub Copilot token"),
        )?;
        let mut record = serde_json::json!({"type":"copilot","access_token":token});
        if let Some(domain) = text(&native, &["enterpriseUrl"]) {
            record["base_url"] = serde_json::json!(format!(
                "https://copilot-api.{}",
                domain
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
            ));
        }
        value = record;
    }
    let (provider, token) = if let Some(oauth) = value.get("claudeAiOauth") {
        (SubscriptionProvider::Claude, oauth)
    } else if let Some(tokens) = value.get("tokens") {
        (SubscriptionProvider::Codex, tokens)
    } else {
        let kind = text(&value, &["type", "provider"])
            .ok_or(SubscriptionError::InvalidImport("missing provider type"))?;
        let provider = SubscriptionProvider::parse(&kind).ok_or(
            SubscriptionError::InvalidImport("unsupported provider type"),
        )?;
        (
            provider,
            value
                .get("token")
                .filter(|t| t.is_object())
                .unwrap_or(&value),
        )
    };
    if provider == SubscriptionProvider::Meta
        && [&value, token].iter().any(|record| {
            record.get("is_subs_active").is_some_and(|v| v == false)
                || record["require_payment"] == true
        })
    {
        return Err(SubscriptionError::InvalidImport(
            "Muse Code requires an active subscription",
        ));
    }
    let access_token = text(
        token,
        if provider == SubscriptionProvider::Meta {
            &["api_key", "access_token", "dca_token"]
        } else {
            &["access_token", "accessToken", "api_key", "key"]
        },
    )
    .ok_or(SubscriptionError::InvalidImport("missing access token"))?;
    let expires_at = text(token, &["expired", "expiry", "expires_at", "expiration"])
        .or_else(|| {
            token
                .get("expiresAt")
                .or_else(|| token.get("expiry_date"))
                .and_then(Value::as_i64)
                .and_then(|milliseconds| {
                    time::OffsetDateTime::from_unix_timestamp(milliseconds / 1000)
                        .ok()
                        .map(timestamp)
                })
        })
        .or_else(|| {
            token
                .get("expires_at")
                .and_then(Value::as_i64)
                .and_then(|epoch| {
                    time::OffsetDateTime::from_unix_timestamp(if epoch > 100_000_000_000 {
                        epoch / 1000
                    } else {
                        epoch
                    })
                    .ok()
                    .map(timestamp)
                })
        });
    if expires_at
        .as_deref()
        .is_some_and(|t| parse_timestamp(t).is_none())
    {
        return Err(SubscriptionError::InvalidImport("invalid expiry timestamp"));
    }
    let base_url = text(&value, &["base_url", "resource_url"]).or_else(|| {
        (provider == SubscriptionProvider::Xai).then(|| "https://cli-chat-proxy.grok.com/v1".into())
    });
    if base_url.as_deref().is_some_and(|url| !valid_base_url(url)) {
        return Err(SubscriptionError::InvalidImport("invalid base URL"));
    }
    if provider == SubscriptionProvider::Meta
        && base_url
            .as_deref()
            .is_some_and(|url| !crate::subscriptions::meta::valid_api_base(url))
    {
        return Err(SubscriptionError::InvalidImport("untrusted Meta base URL"));
    }
    let route = text(&value, &["route", "prefix"]).map(|r| r.trim().to_ascii_lowercase());
    let dialect = text(&value, &["dialect", "format"]).map(|d| d.trim().to_ascii_lowercase());
    let image_limit = match value.get("image_limit") {
        None | Some(Value::Null) => None,
        Some(limit) => Some(
            limit
                .as_u64()
                .and_then(|limit| u32::try_from(limit).ok())
                .filter(|limit| valid_image_limit(*limit))
                .ok_or(SubscriptionError::InvalidEndpoint(IMAGE_LIMIT_RULE))?,
        ),
    };
    if route.is_some() || dialect.is_some() || image_limit.is_some() {
        if provider != SubscriptionProvider::OpenaiCompatible {
            return Err(SubscriptionError::InvalidEndpoint(
                "route, dialect and image_limit apply only to openai_compatible endpoints",
            ));
        }
        if route.as_deref().is_some_and(|r| !valid_route(r)) {
            return Err(SubscriptionError::InvalidEndpoint(ROUTE_RULE));
        }
        if dialect.as_deref().is_some_and(|d| !valid_dialect(d)) {
            return Err(SubscriptionError::InvalidEndpoint(DIALECT_RULE));
        }
        if route.is_some() && base_url.is_none() {
            return Err(SubscriptionError::InvalidEndpoint(
                "an endpoint with a route needs its base_url",
            ));
        }
    }
    let created = now();
    let subscription = subscription_from_value(provider, token, "credential")
        .or_else(|| subscription_from_value(provider, &value, "credential"));
    let mut account = AccountCredential {
        id: random_id(),
        name: String::new(),
        provider,
        quota: AccountQuota::new(),
        budget_revision: rand::random(),
        endpoint_revision: rand::random(),
        access_token,
        refresh_token: text(token, &["refresh_token", "refreshToken"]),
        dca_token: if provider == SubscriptionProvider::Meta {
            text(token, &["dca_token"])
                .or_else(|| text(token, &["access_token"]).filter(|t| t.starts_with("dca:")))
        } else {
            None
        },
        dca_expires_at: text(token, &["dca_expired", "dca_expires_at"]).or_else(|| {
            token["dca_expires_at"]
                .as_i64()
                .and_then(|n| time::OffsetDateTime::from_unix_timestamp(n).ok())
                .map(timestamp)
        }),
        id_token: text(token, &["id_token", "idToken"]),
        account_id: text(token, &["account_id", "account_uuid", "sub", "user_id"])
            .or_else(|| text(&value, &["account_id", "account_uuid", "sub", "user_id"])),
        organization_id: text(&value, &["organization_id", "organizationUuid"])
            .or_else(|| {
                value
                    .get("oauthAccount")
                    .and_then(|account| text(account, &["organizationUuid"]))
            })
            .or_else(|| {
                value
                    .get("organization")
                    .and_then(|organization| text(organization, &["uuid", "id"]))
            }),
        base_url,
        models: value
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| m.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        project_id: text(&value, &["project_id", "projectId"]),
        email: text(&value, &["email", "user_email"]).or_else(|| text(token, &["email"])),
        plan: text(
            &value,
            &["plan_type", "subscription_type", "plan", "subs_tier_name"],
        )
        .or_else(|| text(token, &["subscriptionType", "plan_type"])),
        subscription,
        expires_at: if provider == SubscriptionProvider::Meta {
            None
        } else {
            expires_at
        },
        enabled: !value
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        created_at: created.clone(),
        updated_at: created,
        last_error: None,
        cooldown_until: None,
        cooldown_retry_after: false,
        scoped_cooldowns: Vec::new(),
        exhausted_until: None,
        exhausted_without_reset: false,
        client_id: text(&value, &["client_id"]).or_else(|| text(token, &["client_id"])),
        client_secret: text(&value, &["client_secret"]).or_else(|| text(token, &["client_secret"])),
        token_endpoint: text(&value, &["token_endpoint"]),
        model_exhaustion: BTreeMap::new(),
        reserve_percent: None,
        reserve_windows: vec![],
        route,
        dialect,
        image_limit,
        metadata: BTreeMap::new(),
    };
    apply_identity(&mut account);
    account.name = name
        .filter(|v| !v.trim().is_empty())
        .or_else(|| text(&value, &["name", "label"]))
        .or_else(|| account.email.clone())
        .or_else(|| account.route.clone())
        .unwrap_or_else(|| format!("{} account", provider.as_str()));
    if account.name.len() > 200 || account.email.as_ref().is_some_and(|v| v.len() > 320) {
        return Err(SubscriptionError::InvalidImport(
            "account label is too long",
        ));
    }
    Ok(account)
}

pub(crate) fn subscription_from_value(
    provider: SubscriptionProvider,
    value: &Value,
    source: &'static str,
) -> Option<SubscriptionTier> {
    if provider == SubscriptionProvider::Meta {
        return Some(SubscriptionTier {
            id: text(value, &["subs_tier_id"])?,
            label: text(value, &["subs_tier_name"])?,
            multiplier: None,
            default_weight: None,
            source: source.into(),
        });
    }
    let organization = value.get("organization").unwrap_or(value);
    let plan = text(
        value,
        &[
            "plan_type",
            "planType",
            "chatgpt_plan_type",
            "subscriptionType",
            "subscription_type",
            "subscription_tier_display",
            "subscriptionTierDisplay",
            "subscription_tier",
            "subscriptionTier",
            "plan",
        ],
    )
    .or_else(|| text(organization, &["organization_type"]));
    let rate_tier = text(organization, &["rate_limit_tier", "rateLimitTier"]);
    let mut tier =
        SubscriptionTier::detect(provider, plan.as_deref(), rate_tier.as_deref(), source)?;
    // Anthropic now names its plans (the Max x20 tier is "Ultra"): the
    // display name wins over the tier-derived label, the multiplier stays.
    if provider == SubscriptionProvider::Claude {
        if let Some(name) = text(organization, &["plan_display_name", "planDisplayName"])
            .map(|name| name.trim().trim_start_matches("Claude ").trim().to_owned())
            .filter(|name| !name.is_empty() && name.len() <= 40)
        {
            tier.id = name.to_ascii_lowercase().replace(' ', "_");
            tier.label = format!("Claude {name}");
        }
    }
    // The generic Pro identifier does not prove an allowance multiplier.
    // Only an explicit provider field can add it.
    if provider == SubscriptionProvider::Codex {
        let multiple = value
            .get("usage_multiplier")
            .or_else(|| value.get("rate_limit_multiplier"))
            .and_then(Value::as_u64)
            .filter(|multiple| (1..=100).contains(multiple));
        if let Some(multiple) = multiple {
            tier.multiplier = Some(multiple as u32);
            tier.label = format!("{} x{multiple}", tier.label);
        }
    }
    Some(tier)
}
