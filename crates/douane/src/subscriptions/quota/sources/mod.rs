use super::*;
impl QuotaService {
    pub(super) async fn fetch_billing(
        &self,
        account: &AccountCredential,
        api: BillingApi,
    ) -> Result<Observation, SubscriptionError> {
        let url = api.url();
        #[cfg(test)]
        let url = {
            let origin = self
                .billing_test_origin
                .as_deref()
                .ok_or(SubscriptionError::Transport)?;
            format!("{origin}{}", reqwest::Url::parse(url).unwrap().path())
        };
        let result = send_json(
            self.billing_client
                .get(url)
                .bearer_auth(&account.access_token),
        )
        .await;
        let credits = match (api, result) {
            (BillingApi::MistralVibe, Err(SubscriptionError::Upstream(403))) => vec![],
            (_, result) => api.parse(&result?, time::OffsetDateTime::now_utc())?,
        };
        Ok(Observation {
            windows: vec![],
            plan: None,
            subscription: None,
            credits,
            organization_id: None,
            received: time::OffsetDateTime::now_utc(),
        })
    }

    pub(super) async fn fetch(
        &self,
        account: &AccountCredential,
    ) -> Result<Observation, SubscriptionError> {
        let (value, windows, observed) = match account.provider {
            SubscriptionProvider::Codex => {
                let mut request = self
                    .client
                    .get(&self.codex_usage_url)
                    .bearer_auth(&account.access_token)
                    .header("User-Agent", crate::relay::CODEX_USER_AGENT);
                if let Some(id) = &account.account_id {
                    request = request.header("ChatGPT-Account-Id", id);
                }
                let (value, observed) = send_json_received(request).await?;
                let windows = parse_codex(&value, observed);
                (value, windows, observed)
            }
            SubscriptionProvider::Claude => {
                let (value, observed) = send_json_received(
                    self.client
                        .get(format!("{}/api/oauth/usage", self.claude_api))
                        .bearer_auth(&account.access_token)
                        .header("anthropic-beta", "oauth-2025-04-20")
                        .header("User-Agent", crate::relay::CLAUDE_USER_AGENT),
                )
                .await?;
                let windows = parse_claude(&value, observed);
                (value, windows, observed)
            }
            SubscriptionProvider::Xai => {
                let mut request = self
                    .client
                    .get("https://cli-chat-proxy.grok.com/v1/billing?format=credits")
                    .bearer_auth(&account.access_token)
                    .header("x-xai-token-auth", "xai-grok-cli")
                    .header("x-grok-client-version", "1.0.44")
                    .header(
                        "User-Agent",
                        "grok-pager/1.0.44 grok-shell/1.0.44 (macos; aarch64)",
                    );
                if let Some(id) = &account.account_id {
                    request = request.header("x-userid", id);
                }
                let (value, observed) = send_json_received(request).await?;
                let windows = parse_xai(&value, observed);
                (value, windows, observed)
            }
            SubscriptionProvider::OpencodeGo => {
                let (value, observed) = send_json_received(
                    self.client
                        .get("https://opencode.ai/zen/go/v1/usage")
                        .bearer_auth(&account.access_token)
                        .header("User-Agent", concat!("douane/", env!("CARGO_PKG_VERSION"))),
                )
                .await?;
                let windows = parse_opencode_go(&value, observed);
                (value, windows, observed)
            }
            SubscriptionProvider::Antigravity => {
                let project =
                    account
                        .project_id
                        .as_deref()
                        .ok_or(SubscriptionError::InvalidImport(
                            "missing Google project ID",
                        ))?;
                let (value, observed) = send_json_received(self.client.post("https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary")
                    .bearer_auth(&account.access_token).header("User-Agent", antigravity_user_agent())
                    .json(&json!({"project":project}))).await?;
                let windows = parse_antigravity(&value, observed);
                (value, windows, observed)
            }
            SubscriptionProvider::Gemini => {
                let project =
                    account
                        .project_id
                        .as_deref()
                        .ok_or(SubscriptionError::InvalidImport(
                            "missing Google project ID",
                        ))?;
                let (value, observed) = send_json_received(
                    self.client
                        .post("https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota")
                        .bearer_auth(&account.access_token)
                        .header("User-Agent", crate::relay::GEMINI_USER_AGENT)
                        .json(&json!({"project":project})),
                )
                .await?;
                let windows = parse_gemini(&value, observed);
                (value, windows, observed)
            }
            SubscriptionProvider::Kimi => {
                let (value, observed) = send_json_received(
                    self.client
                        .get("https://api.kimi.com/coding/v1/usages")
                        .bearer_auth(&account.access_token),
                )
                .await?;
                let windows = parse_kimi(&value, observed);
                (value, windows, observed)
            }
            SubscriptionProvider::Copilot => {
                let (value, observed) = send_json_received(
                    self.client
                        .get(&self.copilot_user_url)
                        .header("Authorization", format!("token {}", account.access_token))
                        .header("User-Agent", crate::relay::COPILOT_USER_AGENT)
                        .header("X-GitHub-Api-Version", crate::relay::COPILOT_API_VERSION),
                )
                .await?;
                let windows = parse_copilot(&value, observed);
                (value, windows, observed)
            }
            SubscriptionProvider::OpenaiCompatible
            | SubscriptionProvider::Qwen
            | SubscriptionProvider::Meta => {
                return Err(SubscriptionError::Unsupported);
            }
        };
        let credits = parse_credits(account.provider, &value, observed);
        if windows.is_empty() && credits.is_empty() {
            return Err(SubscriptionError::InvalidResponse);
        }
        let plan = text(&value, &["plan_type", "planType", "subscription_type"]);
        let mut subscription = if account.provider == SubscriptionProvider::Copilot {
            // The SKU names education and free plans; the plan alone does not.
            text(&value, &["access_type_sku"])
                .and_then(|sku| {
                    SubscriptionTier::detect(account.provider, Some(&sku), None, "provider_usage")
                })
                .or_else(|| {
                    SubscriptionTier::detect(
                        account.provider,
                        text(&value, &["copilot_plan"]).as_deref(),
                        None,
                        "provider_usage",
                    )
                })
        } else {
            subscription_from_value(account.provider, &value, "provider_usage")
        };
        let mut organization_id = None;
        // Usage and account metadata are different provider resources. A
        // metadata outage must not discard a successful quota observation.
        match account.provider {
            SubscriptionProvider::Claude => {
                if let Ok(profile) = send_json(
                    self.client
                        .get(format!("{}/api/oauth/profile", self.claude_api))
                        .bearer_auth(&account.access_token)
                        .header("User-Agent", crate::relay::CLAUDE_USER_AGENT)
                        .header("anthropic-beta", "oauth-2025-04-20")
                        .timeout(std::time::Duration::from_secs(10)),
                )
                .await
                {
                    subscription =
                        subscription_from_value(account.provider, &profile, "provider_profile")
                            .or(subscription);
                    organization_id = profile
                        .get("organization")
                        .and_then(|organization| text(organization, &["uuid", "id"]));
                }
            }
            SubscriptionProvider::Xai => {
                let mut request = self
                    .client
                    .get("https://cli-chat-proxy.grok.com/v1/settings")
                    .bearer_auth(&account.access_token)
                    .header("x-xai-token-auth", "xai-grok-cli")
                    .header("x-grok-client-version", "1.0.44")
                    .timeout(std::time::Duration::from_secs(10));
                if let Some(id) = &account.account_id {
                    request = request.header("x-userid", id);
                }
                if let Ok(settings) = send_json(request).await {
                    subscription =
                        subscription_from_value(account.provider, &settings, "provider_settings")
                            .or(subscription);
                }
            }
            SubscriptionProvider::OpencodeGo if !windows.is_empty() => {
                // This endpoint rejects accounts without the Go entitlement.
                subscription =
                    SubscriptionTier::detect(account.provider, Some("go"), None, "provider_usage");
            }
            _ => {}
        }
        Ok(Observation {
            windows,
            plan,
            subscription,
            credits,
            organization_id,
            received: observed,
        })
    }
}
pub(super) fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|v| v.is_finite())
}

pub(super) fn field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key))
}
pub(super) fn remaining(used_percent: Option<f64>) -> Option<f64> {
    used_percent
        .filter(|used| used.is_finite() && *used >= 0.0)
        .map(|used| (100.0 - used).max(0.0))
}

pub(super) fn reset(
    value: Option<&Value>,
    relative: Option<&Value>,
    observed: time::OffsetDateTime,
) -> Option<String> {
    value
        .and_then(|value| {
            if let Some(raw) = value.as_str() {
                if let Some(timestamp) = parse_timestamp(raw) {
                    return Some(timestamp);
                }
            }
            let seconds = number(Some(value))?;
            if seconds <= 0.0 {
                return None;
            }
            // Contemporary millisecond epochs cannot be confused with seconds.
            let seconds = if seconds > 100_000_000_000.0 {
                seconds / 1000.0
            } else {
                seconds
            };
            time::OffsetDateTime::from_unix_timestamp(seconds as i64).ok()
        })
        .or_else(|| {
            number(relative)
                .filter(|seconds| *seconds >= 0.0 && *seconds <= 31_536_000.0)
                .map(|seconds| observed + time::Duration::seconds(seconds as i64))
        })
        .map(timestamp)
}

pub(super) fn cent(value: Option<&Value>) -> Option<f64> {
    let value = value?.as_object()?;
    // The provider's proto3 Cent message serializes zero as an empty object.
    value
        .get("val")
        .map(|value| number(Some(value)))
        .unwrap_or(Some(0.0))
        .filter(|amount| *amount >= 0.0)
        .map(|amount| amount / 100.0)
}

pub(super) fn credit_remaining(used: Option<f64>, limit: Option<f64>) -> Option<f64> {
    Some((limit? - used?).max(0.0))
}

pub(super) fn parse_credits(
    provider: SubscriptionProvider,
    value: &Value,
    observed: time::OffsetDateTime,
) -> Vec<QuotaCredits> {
    match provider {
        SubscriptionProvider::Codex => value
            .get("credits")
            .filter(|credits| credits.is_object())
            .map(|credits| {
                vec![QuotaCredits {
                    id: "codex-credits".into(),
                    label: "Credits".into(),
                    used: None,
                    limit: None,
                    remaining: number(credits.get("balance")).filter(|balance| *balance >= 0.0),
                    unit: "credits".into(),
                    reset_at: None,
                    unlimited: credits.get("unlimited").and_then(Value::as_bool),
                    enabled: field(credits, &["has_credits", "hasCredits"])
                        .and_then(Value::as_bool),
                }]
            })
            .unwrap_or_default(),
        SubscriptionProvider::Claude => {
            let Some(extra) = value.get("extra_usage").filter(|extra| extra.is_object()) else {
                return Vec::new();
            };
            let currency = text(extra, &["currency"])
                .filter(|currency| {
                    currency.len() == 3 && currency.bytes().all(|byte| byte.is_ascii_alphabetic())
                })
                .unwrap_or_else(|| "USD".into())
                .to_ascii_uppercase();
            // USD values are cents. Other currencies retain explicit minor
            // units rather than guessing a currency's decimal exponent.
            let (unit, divisor) = if currency == "USD" {
                (currency, 100.0)
            } else {
                (format!("{currency}_minor"), 1.0)
            };
            let used = number(extra.get("used_credits"))
                .filter(|value| *value >= 0.0)
                .map(|value| value / divisor);
            let limit = number(extra.get("monthly_limit"))
                .filter(|value| *value >= 0.0)
                .map(|value| value / divisor);
            vec![QuotaCredits {
                id: "claude-extra-usage".into(),
                label: "Extra usage".into(),
                used,
                limit,
                remaining: credit_remaining(used, limit),
                unit,
                reset_at: reset(field(extra, &["resets_at", "reset_at"]), None, observed),
                unlimited: None,
                enabled: extra.get("is_enabled").and_then(Value::as_bool),
            }]
        }
        SubscriptionProvider::Xai => {
            let Some(config) = value.get("config") else {
                return Vec::new();
            };
            let period = field(config, &["currentPeriod", "current_period"]);
            let reset_at = reset(
                period
                    .and_then(|period| period.get("end"))
                    .or_else(|| field(config, &["billingPeriodEnd", "billing_period_end"])),
                None,
                observed,
            );
            let mut credits = Vec::new();
            for (used_fields, limit_fields, id, label) in [
                (
                    &["used"][..],
                    &["monthlyLimit", "monthly_limit"][..],
                    "grok-included",
                    "Included credits",
                ),
                (
                    &["onDemandUsed", "on_demand_used"][..],
                    &["onDemandCap", "on_demand_cap"][..],
                    "grok-on-demand",
                    "On-demand credits",
                ),
            ] {
                let used = cent(field(config, used_fields));
                let limit = cent(field(config, limit_fields));
                if used.is_some() || limit.is_some() {
                    credits.push(QuotaCredits {
                        id: id.into(),
                        label: label.into(),
                        used,
                        limit,
                        remaining: credit_remaining(used, limit),
                        unit: "USD".into(),
                        reset_at: reset_at.clone(),
                        unlimited: None,
                        enabled: None,
                    });
                }
            }
            if let Some(balance) = cent(field(config, &["prepaidBalance", "prepaid_balance"])) {
                credits.push(QuotaCredits {
                    id: "grok-prepaid".into(),
                    label: "Prepaid credits".into(),
                    used: None,
                    limit: None,
                    remaining: Some(balance),
                    unit: "USD".into(),
                    reset_at: None,
                    unlimited: None,
                    enabled: None,
                });
            }
            credits
        }
        _ => Vec::new(),
    }
}

pub(super) fn parse_codex(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    for (keys, prefix, label) in [
        (&["rate_limit", "rateLimit"][..], "", "Codex"),
        (
            &["code_review_rate_limit", "codeReviewRateLimit"][..],
            "code-review-",
            "Code review",
        ),
    ] {
        let Some(limits) = field(value, keys) else {
            continue;
        };
        for (keys, default_id) in [
            (&["primary_window", "primaryWindow"][..], "five-hour"),
            (&["secondary_window", "secondaryWindow"][..], "weekly"),
        ] {
            let Some(window) = field(limits, keys).filter(|v| v.is_object()) else {
                continue;
            };
            let duration = number(field(
                window,
                &["limit_window_seconds", "limitWindowSeconds"],
            ));
            // A plan can report its weekly allowance as the primary window.
            // Position in the payload is not the window's duration.
            let id = duration
                .filter(|seconds| *seconds > 0.0)
                .map(crate::subscriptions::cooldown::duration_window)
                .unwrap_or_else(|| default_id.into());
            let used = number(field(window, &["used_percent", "usedPercent"]));
            windows.push(QuotaWindow {
                id: format!("{prefix}{id}"),
                label: format!("{label} {}", codex_window_label(&id)),
                remaining_percent: remaining(used),
                reset_at: reset(
                    field(window, &["reset_at", "resetAt"]),
                    field(window, &["reset_after_seconds", "resetAfterSeconds"]),
                    observed,
                ),
                used,
                limit: Some(100.0),
            });
        }
    }
    windows
}

pub(super) fn parse_claude(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    for (key, id, label) in [
        ("five_hour", "five-hour", "5 hours"),
        ("seven_day", "seven-day", "Weekly"),
        (
            "seven_day_oauth_apps",
            "seven-day-oauth-apps",
            "OAuth apps weekly",
        ),
        ("seven_day_opus", "seven-day-opus", "Opus weekly"),
        ("seven_day_sonnet", "seven-day-sonnet", "Sonnet weekly"),
        ("seven_day_cowork", "seven-day-cowork", "Cowork weekly"),
        ("iguana_necktie", "seven-day-fable", "Fable weekly"),
    ] {
        let Some(window) = value.get(key).filter(|v| v.is_object()) else {
            continue;
        };
        let used = number(window.get("utilization"));
        windows.push(QuotaWindow {
            id: id.into(),
            label: label.into(),
            remaining_percent: remaining(used),
            reset_at: reset(window.get("resets_at"), None, observed),
            used,
            limit: Some(100.0),
        });
    }
    if let Some(limits) = value.get("limits").and_then(Value::as_array) {
        for (index, window) in limits.iter().enumerate() {
            // `is_active` marks the limit that binds right now, not the only
            // one that applies: the Fable weekly limit stays false until it binds.
            let used = number(window.get("percent"));
            // A scoped limit names its model by id, or only by display name
            // ("Fable"): both become the same `seven-day-<model>` window.
            let model = window
                .pointer("/scope/model/id")
                .and_then(Value::as_str)
                .filter(|m| !m.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    window
                        .pointer("/scope/model/display_name")
                        .and_then(Value::as_str)
                        .map(|name| name.trim().to_lowercase().replace(char::is_whitespace, "-"))
                        .filter(|m| !m.is_empty())
                });
            let model = model
                .as_deref()
                .map(|id| crate::subscriptions::cooldown::claude_family(id).unwrap_or(id));
            let kind = text(window, &["kind", "group"]).unwrap_or_else(|| format!("limit-{index}"));
            // Claude names its five-hour window "session" in this list.
            let id = if kind.contains("5") || kind.contains("five") || kind.contains("session") {
                "five-hour".into()
            } else if kind.contains("7") || kind.contains("week") {
                model
                    .map(|m| format!("seven-day-{m}"))
                    .unwrap_or_else(|| "seven-day".into())
            } else {
                format!("{kind}-{}", model.unwrap_or("all"))
            };
            let reset_at = reset(window.get("resets_at"), None, observed);
            if let Some(existing) = windows.iter_mut().find(|existing| existing.id == id) {
                existing.reset_at = existing.reset_at.take().or(reset_at);
                existing.used = existing.used.or(used);
                existing.remaining_percent = existing.remaining_percent.or(remaining(used));
                continue;
            }
            let label = match (model, id.starts_with("seven-day-")) {
                (Some(m), true) => {
                    let mut name = m.to_owned();
                    if let Some(first) = name.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    format!("{name} weekly")
                }
                (Some(m), false) => format!("{m} {kind}"),
                (None, _) => kind,
            };
            windows.push(QuotaWindow {
                id,
                label,
                remaining_percent: remaining(used),
                reset_at,
                used,
                limit: Some(100.0),
            });
        }
    }
    windows
}

/// Snapshots a previous run saved; none when the file is missing or unreadable.
/// The five-hour and weekly windows of Claude's unified rate-limit headers:
/// utilization is a fraction, resets are Unix seconds.
pub(super) fn claude_header_windows(headers: &reqwest::header::HeaderMap) -> Vec<QuotaWindow> {
    let number = |name: String| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    [
        ("5h", "five-hour", "5 hours"),
        ("7d", "seven-day", "Weekly"),
    ]
    .into_iter()
    .filter_map(|(key, id, label)| {
        let used = number(format!("anthropic-ratelimit-unified-{key}-utilization"))
            .map(|fraction| fraction * 100.0);
        let reset_at = number(format!("anthropic-ratelimit-unified-{key}-reset"))
            .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds as i64).ok())
            .map(timestamp);
        if used.is_none() && reset_at.is_none() {
            return None;
        }
        Some(QuotaWindow {
            id: id.into(),
            label: label.into(),
            remaining_percent: remaining(used),
            reset_at,
            used,
            limit: Some(100.0),
        })
    })
    .collect()
}

pub(super) fn codex_header_windows(
    headers: &reqwest::header::HeaderMap,
    received: time::OffsetDateTime,
) -> Vec<QuotaWindow> {
    crate::subscriptions::cooldown::codex_header_budgets(headers, received)
        .map(|(id, used, reset)| QuotaWindow {
            label: format!("Codex {}", codex_window_label(&id)),
            id,
            remaining_percent: remaining(Some(used)),
            reset_at: reset.map(timestamp),
            used: Some(used),
            limit: Some(100.0),
        })
        .collect()
}

fn codex_window_label(id: &str) -> std::borrow::Cow<'_, str> {
    match id {
        "five-hour" => "5 hours".into(),
        "daily" => "Daily".into(),
        "weekly" => "Weekly".into(),
        "monthly" => "Monthly".into(),
        _ => format!(
            "{} seconds",
            id.trim_start_matches("window-").trim_end_matches('s')
        )
        .into(),
    }
}

pub(super) fn parse_xai(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let Some(config) = value.get("config") else {
        return Vec::new();
    };
    let explicit_used = number(field(
        config,
        &["creditUsagePercent", "credit_usage_percent"],
    ));
    let period = field(config, &["currentPeriod", "current_period"]);
    let period_reset = reset(period.and_then(|p| p.get("end")), None, observed);
    let legacy_limit = cent(field(config, &["monthlyLimit", "monthly_limit"]));
    let legacy_used = cent(config.get("used"));
    let used = explicit_used
        .or_else(|| Some(legacy_used? / legacy_limit.filter(|limit| *limit > 0.0)? * 100.0))
        .or_else(|| {
            (period_reset.is_some() && legacy_used.is_none() && legacy_limit.is_none())
                .then_some(0.0)
        });
    let kind = period
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let monthly = kind.contains("month")
        || (period.is_none()
            && (legacy_limit.is_some()
                || field(config, &["billingPeriodEnd", "billing_period_end"]).is_some()));
    let weekly = kind.contains("week");
    let reset_at = period_reset.or_else(|| {
        monthly
            .then(|| {
                reset(
                    field(config, &["billingPeriodEnd", "billing_period_end"]),
                    None,
                    observed,
                )
            })
            .flatten()
    });
    let mut windows = Vec::new();
    // Monthly billing periodEnd must never masquerade as a weekly capacity reset.
    if used.is_some() || reset_at.is_some() {
        windows.push(QuotaWindow {
            id: if monthly {
                "monthly"
            } else if weekly {
                "weekly"
            } else {
                "window-period"
            }
            .into(),
            label: if monthly {
                "Grok monthly"
            } else if weekly {
                "Grok weekly"
            } else {
                "Grok usage"
            }
            .into(),
            remaining_percent: remaining(used),
            reset_at: reset_at.clone(),
            used,
            limit: Some(100.0),
        });
    }
    if let Some(products) =
        field(config, &["productUsage", "product_usage"]).and_then(Value::as_array)
    {
        for (index, product) in products.iter().enumerate() {
            let used = number(field(product, &["usagePercent", "usage_percent"]));
            windows.push(QuotaWindow {
                id: format!("product-{index}"),
                label: text(product, &["product"])
                    .unwrap_or_else(|| format!("Product {}", index + 1)),
                remaining_percent: remaining(used),
                reset_at: reset_at.clone(),
                used,
                limit: Some(100.0),
            });
        }
    }
    windows
}

pub(super) fn parse_opencode_go(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    for (key, id, label) in [
        ("rolling", "five-hour", "5 hours"),
        ("weekly", "weekly", "Weekly"),
        ("monthly", "monthly", "Monthly"),
    ] {
        let Some(window) = value
            .get("usage")
            .and_then(|usage| usage.get(key))
            .filter(|v| v.is_object())
        else {
            continue;
        };
        let used = number(window.get("percent"));
        windows.push(QuotaWindow {
            id: id.into(),
            label: label.into(),
            remaining_percent: remaining(used),
            reset_at: reset(
                field(window, &["resetsAt", "resetAt"]),
                field(window, &["resetInSec", "reset_in_sec"]),
                observed,
            ),
            used,
            limit: Some(100.0),
        });
    }
    windows
}

/// Copilot's monthly premium requests. Chat and completions are unlimited
/// on paid plans and need no bar; a limited one still gets its window.
/// None is account-wide: included models keep answering once premium
/// requests run out, so an empty bar must not take the account out.
pub(super) fn parse_copilot(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let reset_at = reset(
        field(value, &["quota_reset_date_utc", "quota_reset_date"]),
        None,
        observed,
    )
    .or_else(|| {
        // A bare date resets at midnight UTC.
        value["quota_reset_date"]
            .as_str()
            .and_then(|date| parse_timestamp(&format!("{date}T00:00:00Z")))
            .map(timestamp)
    });
    let mut windows = Vec::new();
    for (key, id, label) in [
        ("premium_interactions", "premium", "Premium requests"),
        ("chat", "chat", "Chat"),
        ("completions", "completions", "Completions"),
    ] {
        let Some(quota) = value
            .get("quota_snapshots")
            .and_then(|snapshots| snapshots.get(key))
            .filter(|quota| quota.is_object())
        else {
            continue;
        };
        if quota["unlimited"].as_bool() == Some(true) {
            continue;
        }
        let limit = number(quota.get("entitlement")).filter(|limit| *limit > 0.0);
        let left = number(field(quota, &["remaining", "quota_remaining"]));
        let remaining_percent = number(quota.get("percent_remaining"))
            .or_else(|| Some(left? / limit? * 100.0))
            .map(|percent| percent.clamp(0.0, 100.0));
        windows.push(QuotaWindow {
            id: id.into(),
            label: label.into(),
            remaining_percent,
            reset_at: reset_at.clone(),
            used: limit.zip(left).map(|(limit, left)| (limit - left).max(0.0)),
            limit,
        });
    }
    windows
}

pub(super) fn parse_antigravity(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    if let Some(groups) = value.get("groups").and_then(Value::as_array) {
        for (index, group) in groups.iter().enumerate() {
            let label = text(group, &["displayName", "display_name"])
                .unwrap_or_else(|| format!("Group {}", index + 1));
            if let Some(buckets) = group.get("buckets").and_then(Value::as_array) {
                for (bucket_index, bucket) in buckets.iter().enumerate() {
                    let fraction =
                        number(field(bucket, &["remainingFraction", "remaining_fraction"]));
                    windows.push(QuotaWindow {
                        id: text(bucket, &["bucketId", "bucket_id"])
                            .unwrap_or_else(|| format!("group-{index}-bucket-{bucket_index}")),
                        label: format!(
                            "{label} {}",
                            text(bucket, &["displayName", "display_name", "window"])
                                .unwrap_or_default()
                        ),
                        remaining_percent: fraction.map(|v| (v * 100.0).clamp(0.0, 100.0)),
                        reset_at: reset(
                            field(bucket, &["resetTime", "reset_time"]),
                            None,
                            observed,
                        ),
                        used: None,
                        limit: None,
                    });
                }
            }
        }
    }
    windows
}

pub(super) fn parse_gemini(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    value
        .get("buckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, bucket)| {
            let model = text(bucket, &["modelId", "model_id"]);
            let kind = text(bucket, &["tokenType", "token_type"])
                .unwrap_or_else(|| format!("bucket-{index}"));
            QuotaWindow {
                id: model
                    .as_ref()
                    .map(|model| format!("model:{model}:{kind}"))
                    .unwrap_or_else(|| format!("bucket-{index}")),
                label: model.map(|model| format!("{model} {kind}")).unwrap_or(kind),
                remaining_percent: number(field(
                    bucket,
                    &["remainingFraction", "remaining_fraction"],
                ))
                .map(|fraction| (fraction * 100.0).clamp(0.0, 100.0)),
                reset_at: reset(field(bucket, &["resetTime", "reset_time"]), None, observed),
                used: None,
                limit: None,
            }
        })
        .collect()
}

pub(super) fn parse_kimi(value: &Value, observed: time::OffsetDateTime) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    if let Some(limits) = value.get("limits").and_then(Value::as_array) {
        for (index, limit) in limits.iter().enumerate() {
            let detail = limit.get("detail").unwrap_or(limit);
            let used = number(detail.get("used"));
            let max = number(detail.get("limit"));
            let percent = used
                .zip(max)
                .filter(|(_, max)| *max > 0.0)
                .map(|(used, max)| (100.0 - used / max * 100.0).clamp(0.0, 100.0));
            windows.push(QuotaWindow {
                id: format!("limit-{index}"),
                label: text(limit, &["name", "title", "scope"])
                    .unwrap_or_else(|| format!("Limit {}", index + 1)),
                remaining_percent: percent,
                reset_at: reset(
                    field(detail, &["reset_at", "resetAt", "reset_time", "resetTime"]),
                    field(detail, &["reset_in", "resetIn", "ttl"]),
                    observed,
                ),
                used,
                limit: max,
            });
        }
    }
    windows
}
