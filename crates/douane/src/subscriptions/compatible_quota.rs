use super::{QuotaCredits, SubscriptionError, parse_timestamp, timestamp};
use serde_json::Value;
use time::OffsetDateTime;

#[derive(Clone, Copy)]
pub(super) enum BillingApi {
    OpenRouter,
    Refuseless,
    MistralVibe,
}

impl BillingApi {
    pub(super) fn for_base_url(base: Option<&str>) -> Option<Self> {
        let url = reqwest::Url::parse(base?).ok()?;
        if url.scheme() != "https"
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        match (url.host_str()?, url.path().trim_end_matches('/')) {
            ("openrouter.ai", "/api/v1") => Some(Self::OpenRouter),
            ("api.refuseless.com", "/v1") => Some(Self::Refuseless),
            ("api.mistral.ai", "/v1") => Some(Self::MistralVibe),
            _ => None,
        }
    }

    pub(super) fn url(self) -> &'static str {
        match self {
            Self::OpenRouter => "https://openrouter.ai/api/v1/key",
            Self::Refuseless => "https://api.refuseless.com/v1/credits",
            Self::MistralVibe => "https://api.mistral.ai/v1/billing/budget",
        }
    }

    pub(super) fn parse(
        self,
        response: &Value,
        observed: OffsetDateTime,
    ) -> Result<Vec<QuotaCredits>, SubscriptionError> {
        match self {
            Self::OpenRouter => openrouter(&response["data"], observed),
            Self::Refuseless => refuseless(response),
            Self::MistralVibe => mistral_vibe(response),
        }
    }
}

fn credit(id: &str, label: &str, unit: &str, remaining: Option<f64>) -> QuotaCredits {
    QuotaCredits {
        id: id.into(),
        label: label.into(),
        used: None,
        limit: None,
        remaining,
        unit: unit.into(),
        reset_at: None,
        unlimited: None,
        enabled: None,
    }
}

fn openrouter(
    data: &Value,
    observed: OffsetDateTime,
) -> Result<Vec<QuotaCredits>, SubscriptionError> {
    if data["is_management_key"].as_bool() == Some(true)
        || data["is_provisioning_key"].as_bool() == Some(true)
    {
        return Err(SubscriptionError::Unsupported);
    }
    let budget = &data["effective_budget"];
    let mut credits = Vec::new();
    if let Some(remaining) = budget["remaining_usd"].as_f64() {
        let mut entry = credit(
            "openrouter-budget",
            "Effective budget",
            "USD",
            Some(remaining),
        );
        entry.limit = budget["limit_usd"].as_f64();
        entry.used = budget["spend_usd"].as_f64();
        entry.reset_at = budget["resets_at"]
            .as_str()
            .and_then(parse_timestamp)
            .map(timestamp);
        credits.push(entry);
    }
    let limit = data["limit"].as_f64();
    let uncapped = data.get("limit").is_some_and(Value::is_null)
        && data.get("limit_remaining").is_some_and(Value::is_null);
    let remaining = data["limit_remaining"].as_f64();
    // The effective budget may already be the key cap. Its exact reset is
    // more informative than the legacy key fields in that case.
    if (remaining.is_some() || uncapped)
        && (credits.is_empty() || budget["scope"].as_str() != Some("key_limit"))
    {
        let mut entry = credit("openrouter-key", "Key spending cap", "USD", remaining);
        entry.limit = limit;
        entry.unlimited = uncapped.then_some(true);
        let key_budget = data["budgets"].as_array().and_then(|budgets| {
            budgets
                .iter()
                .find(|budget| budget["scope"].as_str() == Some("key_limit"))
        });
        entry.used = key_budget
            .and_then(|budget| budget["spend_usd"].as_f64())
            .or_else(|| openrouter_key_spend(data));
        credits.push(entry);
    }
    let daily = &data["free_model_daily_requests"];
    if let Some(remaining) = daily["remaining"].as_u64() {
        let mut entry = credit(
            "openrouter-free-daily",
            "Free models daily",
            "requests",
            Some(remaining as f64),
        );
        entry.limit = daily["limit"].as_u64().map(|n| n as f64);
        entry.used = daily["used"].as_u64().map(|n| n as f64);
        entry.reset_at = observed
            .date()
            .next_day()
            .map(|date| timestamp(date.midnight().assume_utc()));
        credits.push(entry);
    }
    if credits.is_empty() {
        return Err(SubscriptionError::InvalidResponse);
    }
    Ok(credits)
}

fn openrouter_key_spend(data: &Value) -> Option<f64> {
    let (usage, byok_usage) = match data["limit_reset"].as_str() {
        Some("daily") => ("usage_daily", "byok_usage_daily"),
        Some("weekly") => ("usage_weekly", "byok_usage_weekly"),
        Some("monthly") => ("usage_monthly", "byok_usage_monthly"),
        None => ("usage", "byok_usage"),
        _ => return None,
    };
    let spend = data[usage].as_f64()?;
    if data["include_byok_in_limit"].as_bool() == Some(true) {
        Some(spend + data[byok_usage].as_f64()?)
    } else {
        Some(spend)
    }
}

fn refuseless(response: &Value) -> Result<Vec<QuotaCredits>, SubscriptionError> {
    let available = response["available"]
        .as_f64()
        .ok_or(SubscriptionError::InvalidResponse)?;
    if response["unit"].as_str() != Some("usd") {
        return Err(SubscriptionError::InvalidResponse);
    }
    let mut credits = vec![credit(
        "refuseless-available",
        "Available paid credits",
        "USD",
        Some(available),
    )];
    let budget = response["key_limit"].as_f64();
    let spent = response["key_spent"].as_f64();
    if budget.is_some() || spent.is_some() {
        let mut entry = credit(
            "refuseless-key",
            "Applicable spending budget",
            "USD",
            budget,
        );
        entry.unlimited = response
            .get("key_limit")
            .is_some_and(Value::is_null)
            .then_some(true);
        // The remaining budget may come from the workspace and the spend is
        // lifetime usage for this key. They do not form a percentage.
        entry.used = spent;
        credits.push(entry);
    }
    Ok(credits)
}

fn mistral_vibe(response: &Value) -> Result<Vec<QuotaCredits>, SubscriptionError> {
    let response = response
        .as_object()
        .ok_or(SubscriptionError::InvalidResponse)?;
    let Some(budget) = response
        .get("vibe_budget")
        .filter(|budget| !budget.is_null())
    else {
        return Ok(vec![]);
    };
    let initial = budget["initial_budget"]
        .as_f64()
        .ok_or(SubscriptionError::InvalidResponse)?;
    if initial <= 0.0 {
        return Ok(vec![]);
    }
    let used = budget["usage_percentage"]
        .as_f64()
        .ok_or(SubscriptionError::InvalidResponse)?
        .max(0.0);
    let mut entry = credit(
        "mistral-vibe-monthly",
        "Vibe Code monthly quota",
        "%",
        Some((100.0 - used).max(0.0)),
    );
    entry.used = Some(used);
    entry.reset_at = budget["reset_at"]
        .as_str()
        .and_then(parse_timestamp)
        .map(timestamp);
    Ok(vec![entry])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openrouter_key_spend_includes_byok_when_the_budget_does() {
        let mut data = json!({
            "effective_budget": {"scope": "workspace", "remaining_usd": 3},
            "budgets": [{"scope": "workspace", "spend_usd": 97}, {"scope": "key_limit", "spend_usd": 13}],
            "limit": 20, "limit_remaining": 7, "limit_reset": "daily",
            "usage_daily": 8, "byok_usage_daily": 4, "byok_usage": 200,
            "include_byok_in_limit": true
        });
        let observed = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(openrouter(&data, observed).unwrap()[1].used, Some(13.0));
        data["budgets"] = Value::Null;
        assert_eq!(openrouter(&data, observed).unwrap()[1].used, Some(12.0));
        data["include_byok_in_limit"] = Value::Bool(false);
        assert_eq!(openrouter(&data, observed).unwrap()[1].used, Some(8.0));
    }

    #[test]
    fn openrouter_ignores_management_and_legacy_provisioning_keys() {
        for flag in ["is_management_key", "is_provisioning_key"] {
            let mut data = json!({"limit": null, "limit_remaining": null});
            data[flag] = Value::Bool(true);
            assert!(matches!(
                openrouter(&data, OffsetDateTime::UNIX_EPOCH),
                Err(SubscriptionError::Unsupported)
            ));
        }
    }
}
