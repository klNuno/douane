//! Persist only the scope explicitly identified by a provider refusal.
use super::{SubscriptionProvider, parse_timestamp};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub(crate) enum QuotaScope {
    Model(String),
    Family(String),
}

impl QuotaScope {
    pub(crate) fn applies(&self, model: &str) -> bool {
        match self {
            Self::Model(id) => id == model,
            Self::Family(family) => model
                .strip_prefix("claude-")
                .is_some_and(|id| id.split('-').next() == Some(family.as_str())),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ScopedCooldown {
    pub scope: QuotaScope,
    pub until: String,
    pub retry_after: bool,
}

impl ScopedCooldown {
    pub(crate) fn active(&self, now: time::OffsetDateTime) -> bool {
        parse_timestamp(&self.until).is_some_and(|until| until > now)
    }
}

pub(crate) fn claude_scope(
    provider: SubscriptionProvider,
    model: &str,
    headers: &HeaderMap,
) -> Option<QuotaScope> {
    if provider != SubscriptionProvider::Claude {
        return None;
    }
    let text = |key: &str| headers.get(key).and_then(|v| v.to_str().ok());
    if text("anthropic-ratelimit-unified-status") != Some("rejected") {
        return None;
    }
    // A representative claim does not override another exhausted global window.
    for key in ["5h", "7d"] {
        if text(&format!("anthropic-ratelimit-unified-{key}-status")) == Some("rejected")
            || text(&format!("anthropic-ratelimit-unified-{key}-utilization"))
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|used| used >= 1.0)
        {
            return None;
        }
    }
    let scope = match text("anthropic-ratelimit-unified-representative-claim")? {
        "seven_day_opus" => QuotaScope::Family("opus".into()),
        "seven_day_sonnet" => QuotaScope::Family("sonnet".into()),
        // The included-model list is server-owned. Do not assume every model
        // belongs to it, or invent a permanent family mapping for that group.
        "seven_day_overage_included" => QuotaScope::Model(model.into()),
        _ => return None,
    };
    scope.applies(model).then_some(scope)
}

pub(crate) fn google_scope(error: &Value, model: &str) -> Option<QuotaScope> {
    let mut identified = false;
    for detail in error.get("details")?.as_array()? {
        if let Some(violations) = detail.get("violations") {
            let violations = violations.as_array()?;
            if violations.is_empty() {
                return None;
            }
            for violation in violations {
                if !model_descriptors(violation)
                    || violation
                        .pointer("/quotaDimensions/model")
                        .and_then(Value::as_str)
                        != Some(model)
                {
                    return None;
                }
                identified = true;
            }
        } else if let Some(reason) = detail.get("reason") {
            if !matches!(
                reason.as_str()?,
                "QUOTA_EXHAUSTED" | "MODEL_CAPACITY_EXHAUSTED" | "RATE_LIMIT_EXCEEDED"
            ) || detail.pointer("/metadata/model").and_then(Value::as_str) != Some(model)
                || !model_descriptors(&detail["metadata"])
            {
                return None;
            }
            identified = true;
        }
    }
    identified.then(|| QuotaScope::Model(model.into()))
}

// A model dimension cannot narrow a project/global or unknown quota descriptor.
// Accept an explicit per-model descriptor; absent descriptors leave the exact
// model dimension authoritative.
fn model_descriptors(value: &Value) -> bool {
    ["quotaMetric", "quotaId", "quota_metric", "quota_limit"]
        .into_iter()
        .filter_map(|key| value.get(key))
        .all(|descriptor| {
            descriptor.as_str().is_some_and(|text| {
                let text = text.to_ascii_lowercase();
                !["global", "project", "account", "user"]
                    .iter()
                    .any(|scope| text.contains(scope))
                    && (text.contains("permodel") || text.contains("per_model"))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn google_mixed_and_unknown_limits_remain_global() {
        let scoped = json!({"reason":"QUOTA_EXHAUSTED","metadata":{"model":"gemini-4-argon"}});
        for details in [
            json!([]),
            json!([{"reason":"QUOTA_EXHAUSTED"}]),
            json!([scoped.clone(),{"violations":[{"quotaId":"GlobalRequestsPerDay"}]}]),
            json!([scoped.clone(),{"reason":"QUOTA_EXHAUSTED","metadata":{"model":"other"}}]),
            json!([{"violations":[{"quotaMetric":"example.googleapis.com/requests", "quotaId":"GlobalRequestsPerDay", "quotaDimensions":{"model":"gemini-4-argon"}}]}]),
            json!([{"reason":"QUOTA_EXHAUSTED","metadata":{"model":"gemini-4-argon","quota_metric":"example.googleapis.com/requests","quota_limit":"RequestsPerProjectPerDay"}}]),
            json!([{"violations":[{"quotaId":"UnknownLimit", "quotaDimensions":{"model":"gemini-4-argon"}}]}]),
            json!([{"violations":[{"quotaId":"RequestsPerModel", "quotaMetric":"example.googleapis.com/global_requests", "quotaDimensions":{"model":"gemini-4-argon"}}]}]),
        ] {
            assert_eq!(
                google_scope(&json!({"details":details}), "gemini-4-argon"),
                None
            );
        }
        assert_eq!(
            google_scope(&json!({"details":[scoped]}), "gemini-4-argon"),
            Some(QuotaScope::Model("gemini-4-argon".into()))
        );
        assert!(!QuotaScope::Model("gemini-4-argon".into()).applies("gemini-4-argon-extra"));
        assert_eq!(
            google_scope(
                &json!({"details":[{"violations":[{"quotaId":"RequestsPerModelPerDay", "quotaMetric":"example.googleapis.com/requests_per_model", "quotaDimensions":{"model":"gemini-4-argon"}}]}]}),
                "gemini-4-argon"
            ),
            Some(QuotaScope::Model("gemini-4-argon".into()))
        );
    }

    #[test]
    fn claude_global_or_unknown_claims_never_become_model_limits() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-status",
            "rejected".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day_opus".parse().unwrap(),
        );
        let scope =
            claude_scope(SubscriptionProvider::Claude, "claude-opus-5-5", &headers).unwrap();
        assert!(scope.applies("claude-opus-4-8"));
        assert!(!scope.applies("claude-fable-5-2"));
        assert_eq!(
            claude_scope(SubscriptionProvider::Claude, "claude-fable-5-2", &headers),
            None
        );
        for claim in ["seven_day", "five_hour", "overage", "unknown"] {
            headers.insert(
                "anthropic-ratelimit-unified-representative-claim",
                claim.parse().unwrap(),
            );
            assert_eq!(
                claude_scope(SubscriptionProvider::Claude, "claude-opus-5-5", &headers),
                None
            );
        }
        headers.insert(
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day_opus".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "1.0".parse().unwrap(),
        );
        assert_eq!(
            claude_scope(SubscriptionProvider::Claude, "claude-opus-5-5", &headers),
            None
        );
    }
}
