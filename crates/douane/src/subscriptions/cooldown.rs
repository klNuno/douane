//! Persist only the scope explicitly identified by a provider refusal.
use super::{SubscriptionProvider, parse_timestamp};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Duration, OffsetDateTime};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub(crate) enum QuotaScope {
    Global,
    Model(String),
    Family(String),
}

impl QuotaScope {
    pub(crate) fn applies(&self, model: &str) -> bool {
        match self {
            Self::Global => true,
            Self::Model(id) => id == model,
            Self::Family(family) => model
                .strip_prefix("claude-")
                .is_some_and(|id| id.split('-').next() == Some(family.as_str())),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ScopedCooldown {
    pub scope: QuotaScope,
    pub until: String,
    pub retry_after: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RefusalKind {
    Quota,
    RateLimit,
    Billing,
    Auth,
    Transport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provenance {
    RetryAfter,
    ProviderReset,
    Inferred,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Refusal {
    pub id: u64,
    pub kind: RefusalKind,
    pub scope: QuotaScope,
    pub window: Option<String>,
    pub started_at: String,
    pub reset_at: Option<String>,
    pub retry_at: String,
    pub retry_after_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_retry_at: Option<String>,
    pub provenance: Provenance,
    pub budget_revision: Option<u64>,
    pub reason: String,
    #[serde(default)]
    pub legacy: bool,
}

impl Refusal {
    pub(crate) fn active(&self, now: time::OffsetDateTime) -> bool {
        parse_timestamp(&self.retry_at).is_some_and(|until| until > now)
    }

    pub(crate) fn retryable_at(&self, now: OffsetDateTime) -> bool {
        !self.explicit_delay_at(now)
            && !(self.legacy && self.reason == "provider request failed")
            && matches!(self.kind, RefusalKind::RateLimit | RefusalKind::Transport)
    }

    pub(crate) fn explicit_delay_at(&self, now: OffsetDateTime) -> bool {
        [
            self.retry_after_at.as_deref(),
            self.provider_retry_at.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(parse_timestamp)
        .any(|until| until > now)
    }

    pub(crate) fn retry_from_explicit_delay(&self) -> bool {
        self.retry_after_at.as_ref() == Some(&self.retry_at)
            || self.provider_retry_at.as_ref() == Some(&self.retry_at)
    }

    pub(crate) fn timed(
        observed: time::OffsetDateTime,
        seconds: u64,
        reason: &str,
        scope: QuotaScope,
        explicit: bool,
    ) -> Self {
        let delay = if explicit {
            seconds
        } else {
            seconds.min(86_400)
        };
        let until = deadline(observed, delay);
        let reason = safe_reason(reason);
        let kind = match reason {
            "upstream quota exceeded" | "websocket upstream quota exceeded" => RefusalKind::Quota,
            "authentication needs reconnecting" | "websocket upstream authentication failed" => {
                RefusalKind::Auth
            }
            "upstream rate limit reached" | "websocket upstream rate limited" | "rate limited" => {
                RefusalKind::RateLimit
            }
            _ => RefusalKind::Transport,
        };
        Self {
            id: 0,
            kind,
            scope,
            window: None,
            started_at: super::timestamp(observed),
            reset_at: None,
            retry_at: super::timestamp(until),
            retry_after_at: explicit.then(|| super::timestamp(until)),
            provider_retry_at: None,
            provenance: if explicit {
                Provenance::RetryAfter
            } else {
                Provenance::Inferred
            },
            budget_revision: None,
            reason: reason.into(),
            legacy: false,
        }
    }
}

pub(crate) fn deadline(observed: time::OffsetDateTime, seconds: u64) -> time::OffsetDateTime {
    observed
        .checked_add(time::Duration::seconds(
            i64::try_from(seconds).unwrap_or(i64::MAX),
        ))
        .unwrap_or(time::PrimitiveDateTime::MAX.assume_utc())
}

pub(crate) fn safe_reason(reason: &str) -> &'static str {
    match reason {
        "upstream quota exceeded" => "upstream quota exceeded",
        "websocket upstream quota exceeded" => "websocket upstream quota exceeded",
        "upstream rate limit reached" => "upstream rate limit reached",
        "websocket upstream rate limited" => "websocket upstream rate limited",
        "websocket upstream authentication failed" => "websocket upstream authentication failed",
        "authentication needs reconnecting" => "authentication needs reconnecting",
        "upstream connection failed" => "upstream connection failed",
        "upstream request timed out" => "upstream request timed out",
        "upstream temporarily unavailable" => "upstream temporarily unavailable",
        "websocket upstream connection failed" => "websocket upstream connection failed",
        "rate limited" => "rate limited",
        _ => "provider request failed",
    }
}

pub(crate) fn claude_family(model: &str) -> Option<&str> {
    let family = model.strip_prefix("claude-")?.split('-').next()?;
    matches!(family, "fable" | "opus" | "sonnet").then_some(family)
}

pub(crate) fn window_scope(id: &str) -> Option<QuotaScope> {
    if let Some((model, _)) = id
        .strip_prefix("model:")
        .and_then(|rest| rest.rsplit_once(':'))
    {
        return Some(QuotaScope::Model(model.into()));
    }
    match id.strip_prefix("seven-day-")? {
        family @ ("fable" | "opus" | "sonnet") => Some(QuotaScope::Family(family.into())),
        model if claude_family(model).is_some() => Some(QuotaScope::Model(model.into())),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct GoogleRefusal {
    /// `error.status`, then the first `details[].reason`, such as
    /// `RESOURCE_EXHAUSTED/MODEL_CAPACITY_EXHAUSTED`.
    pub reason: String,
    /// The delay Google advises (`RetryInfo.retryDelay` or `quotaResetDelay`).
    pub retry_seconds: Option<u64>,
    scope: Option<QuotaScope>,
}

pub(crate) fn google_refusal(body: &[u8], model: &str) -> Option<GoogleRefusal> {
    let value: Value = serde_json::from_slice(body).ok()?;
    parse_google_error(value.get("error")?, model)
}

fn parse_google_error(error: &Value, model: &str) -> Option<GoogleRefusal> {
    let token = |v: &Value| {
        v.as_str()
            .filter(|s| {
                matches!(
                    *s,
                    "RESOURCE_EXHAUSTED"
                        | "QUOTA_EXHAUSTED"
                        | "MODEL_CAPACITY_EXHAUSTED"
                        | "RATE_LIMIT_EXCEEDED"
                        | "INVALID_ARGUMENT"
                        | "UNAUTHENTICATED"
                        | "PERMISSION_DENIED"
                        | "NOT_FOUND"
                        | "UNAVAILABLE"
                        | "INTERNAL"
                )
            })
            .map(str::to_owned)
    };
    let details = error["details"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut parts: Vec<String> = token(&error["status"]).into_iter().collect();
    parts.extend(details.iter().find_map(|d| token(&d["reason"])));
    let retry_seconds = details
        .iter()
        .flat_map(|d| [d.get("retryDelay"), d.pointer("/metadata/quotaResetDelay")])
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(google_duration)
        .max();
    if parts.is_empty() && retry_seconds.is_none() {
        return None;
    }
    Some(GoogleRefusal {
        reason: parts.join("/"),
        retry_seconds,
        scope: google_scope(error, model),
    })
}

/// Seconds in a Google duration: `2.5s`, `1h2m3.4s`, `750ms`; rounded up.
pub(crate) fn google_duration(text: &str) -> Option<u64> {
    let mut total = 0.0;
    let mut number = String::new();
    let mut chars = text.trim().chars().peekable();
    let mut seen = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let value: f64 = number.parse().ok()?;
        number.clear();
        let unit = match (c, chars.peek()) {
            ('m', Some('s')) => {
                chars.next();
                0.001
            }
            ('h', _) => 3600.0,
            ('m', _) => 60.0,
            ('s', _) => 1.0,
            _ => return None,
        };
        total += value * unit;
        seen = true;
    }
    (seen && number.is_empty() && total.is_finite()).then(|| total.ceil() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_google_and_header_delays_keep_their_own_deadlines() {
        let received = OffsetDateTime::UNIX_EPOCH;
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "129600".parse().unwrap());
        let body = json!({"error":{
            "code":429, "status":"RESOURCE_EXHAUSTED", "resets_at":200000,
            "details":[{"retryDelay":"172800s"}]
        }});
        let google = parse_google_error(&body["error"], "gemini-exact").unwrap();
        let refusal = classify_http(
            SubscriptionProvider::Antigravity,
            "gemini-exact",
            429,
            &headers,
            Some(&body),
            Some(&google),
            received,
            Transport::Http,
        )
        .unwrap();
        assert_eq!(
            refusal.reset_at,
            Some(super::super::timestamp(deadline(received, 200000)))
        );
        assert_eq!(
            refusal.retry_after_at,
            Some(super::super::timestamp(deadline(received, 129600)))
        );
        assert_eq!(
            refusal.provider_retry_at,
            Some(super::super::timestamp(deadline(received, 172800)))
        );
        assert_eq!(refusal.retry_at, refusal.provider_retry_at.unwrap());
        assert_eq!(refusal.provenance, Provenance::ProviderReset);
    }

    #[test]
    fn a_compatible_billing_event_has_the_same_kind_as_a_payment_refusal() {
        let event = json!({"type":"error", "status":402,"error":{"code":"insufficient_quota"}});
        let failure = classify_event(
            SubscriptionProvider::OpenaiCompatible,
            &event,
            "exact-model",
            OffsetDateTime::UNIX_EPOCH,
            Transport::WebSocket,
        )
        .unwrap();
        assert_eq!(failure.status, 402);
        assert_eq!(failure.refusal.kind, RefusalKind::Billing);
        assert!(!failure.refusal.retryable_at(OffsetDateTime::UNIX_EPOCH));
    }

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

#[derive(Clone, Copy)]
pub(crate) enum Transport {
    Http,
    WebSocket,
}

#[derive(Default)]
struct Timing {
    window: Option<String>,
    reset: Option<OffsetDateTime>,
    retry_after: Option<OffsetDateTime>,
    google_retry: Option<OffsetDateTime>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn classify_http(
    provider: SubscriptionProvider,
    model: &str,
    status: u16,
    headers: &HeaderMap,
    body: Option<&Value>,
    google: Option<&GoogleRefusal>,
    received: OffsetDateTime,
    transport: Transport,
) -> Option<Refusal> {
    let error = body.map(error_object);
    let exhausted = window_exhausted(headers)
        || error.and_then(code_kind) == Some(RefusalKind::Quota)
        || google_quota(google);
    let kind = classify_status(provider, status, exhausted)?;
    let (scope, window, reset) = if status == 429 {
        let scope = claude_scope(provider, model, headers)
            .or_else(|| google.and_then(|r| r.scope.clone()))
            .unwrap_or(QuotaScope::Global);
        let (window, reset) = match provider {
            SubscriptionProvider::Claude => claude_budget(headers, &scope),
            SubscriptionProvider::Codex => codex_budget(headers, received),
            _ => (None, None),
        };
        (scope, window, reset)
    } else {
        (QuotaScope::Global, None, None)
    };
    let timing = Timing {
        window,
        reset: reset.or_else(|| error.and_then(|e| provider_reset(e, received))),
        retry_after: retry_after(headers, received)
            .max(error.and_then(|e| delay_field(e, "retry_after", whole_second(received)))),
        google_retry: google
            .and_then(|r| r.retry_seconds)
            .map(|s| deadline(received, s)),
    };
    Some(build_refusal(kind, transport, scope, received, timing))
}

pub(crate) struct EventRefusal {
    pub status: u16,
    pub refusal: Refusal,
    pub diagnostic: String,
}

pub(crate) fn classify_event(
    provider: SubscriptionProvider,
    value: &Value,
    model: &str,
    received: OffsetDateTime,
    transport: Transport,
) -> Option<EventRefusal> {
    if !matches!(value["type"].as_str(), Some("error" | "response.failed"))
        && !value["error"].is_object()
    {
        return None;
    }
    let error = error_object(value);
    let status = value["status"]
        .as_u64()
        .or_else(|| error["status"].as_u64())
        .or_else(|| error["code"].as_u64())
        .and_then(|n| u16::try_from(n).ok());
    let google = error["details"]
        .is_array()
        .then(|| parse_google_error(error, model))
        .flatten();
    let code = classified_code(error);
    let from_code = code.map(|(_, kind)| kind);
    let kind = status
        .and_then(|status| {
            let exhausted = from_code == Some(RefusalKind::Quota) || google_quota(google.as_ref());
            classify_status(provider, status, exhausted)
        })
        .or(from_code)?;
    let status = match kind {
        RefusalKind::Auth => status
            .filter(|code| *code == 401 || *code == 403)
            .unwrap_or(401),
        RefusalKind::Billing => 402,
        RefusalKind::Transport => status.unwrap_or(503),
        _ => 429,
    };
    let timing = Timing {
        reset: provider_reset(error, received),
        retry_after: delay_field(error, "retry_after", whole_second(received)),
        google_retry: google
            .as_ref()
            .and_then(|r| r.retry_seconds)
            .map(|s| deadline(received, s)),
        ..Timing::default()
    };
    let scope = google.and_then(|r| r.scope).unwrap_or(QuotaScope::Global);
    let refusal = build_refusal(kind, transport, scope, received, timing);
    let diagnostic = match code {
        Some((code, _)) => format!("{} ({code})", refusal.reason),
        None => refusal.reason.clone(),
    };
    Some(EventRefusal {
        status,
        refusal,
        diagnostic,
    })
}

fn classify_status(
    provider: SubscriptionProvider,
    status: u16,
    exhausted: bool,
) -> Option<RefusalKind> {
    Some(match status {
        402 if provider == SubscriptionProvider::OpenaiCompatible => RefusalKind::Billing,
        429 if exhausted => RefusalKind::Quota,
        429 => RefusalKind::RateLimit,
        401 | 403 => RefusalKind::Auth,
        500..=599 => RefusalKind::Transport,
        _ => return None,
    })
}

fn google_quota(google: Option<&GoogleRefusal>) -> bool {
    google.is_some_and(|r| r.reason.split('/').any(|code| code == "QUOTA_EXHAUSTED"))
}

fn error_object(value: &Value) -> &Value {
    value
        .get("error")
        .or_else(|| value.pointer("/response/error"))
        .unwrap_or(value)
}

fn code_kind(error: &Value) -> Option<RefusalKind> {
    classified_code(error).map(|(_, kind)| kind)
}

fn classified_code(error: &Value) -> Option<(&str, RefusalKind)> {
    [error["code"].as_str(), error["type"].as_str()]
        .into_iter()
        .flatten()
        .find_map(|code| {
            let kind = match code {
                "usage_limit_reached"
                | "insufficient_quota"
                | "credit_balance_exhausted"
                | "organization_spend_limit_exceeded"
                | "project_spend_limit_exceeded"
                | "quota_exceeded" => RefusalKind::Quota,
                "rate_limit_exceeded" | "rate_limit_error" | "slow_down" | "overloaded_error" => {
                    RefusalKind::RateLimit
                }
                "invalid_api_key"
                | "authentication_error"
                | "invalid_authentication"
                | "invalid_token"
                | "token_expired"
                | "unauthorized" => RefusalKind::Auth,
                _ => return None,
            };
            Some((code, kind))
        })
}

fn build_refusal(
    kind: RefusalKind,
    transport: Transport,
    scope: QuotaScope,
    received: OffsetDateTime,
    timing: Timing,
) -> Refusal {
    let (reason, fallback) = match (kind, transport) {
        (RefusalKind::Quota, Transport::WebSocket) => ("websocket upstream quota exceeded", 60),
        (RefusalKind::Quota | RefusalKind::Billing, _) => ("upstream quota exceeded", 30),
        (RefusalKind::RateLimit, Transport::WebSocket) => ("websocket upstream rate limited", 60),
        (RefusalKind::RateLimit, Transport::Http) => ("upstream rate limit reached", 30),
        (RefusalKind::Auth, Transport::WebSocket) => {
            ("websocket upstream authentication failed", 300)
        }
        (RefusalKind::Auth, Transport::Http) => ("authentication needs reconnecting", 300),
        (RefusalKind::Transport, Transport::WebSocket) => {
            ("websocket upstream connection failed", 15)
        }
        (RefusalKind::Transport, Transport::Http) => ("upstream temporarily unavailable", 30),
    };
    let inferred = timing
        .reset
        .map(|reset| reset.min(received + Duration::days(1)));
    let retry_at = inferred
        .max(timing.google_retry)
        .max(timing.retry_after)
        .unwrap_or_else(|| deadline(received, fallback))
        .max(received + Duration::seconds(1));
    let provenance = if timing.retry_after == Some(retry_at) {
        Provenance::RetryAfter
    } else if timing.reset.is_some() || timing.google_retry.is_some() {
        Provenance::ProviderReset
    } else if timing.retry_after.is_some() {
        Provenance::RetryAfter
    } else {
        Provenance::Inferred
    };
    Refusal {
        id: 0,
        kind,
        scope,
        window: timing.window,
        started_at: super::timestamp(received),
        reset_at: timing.reset.map(super::timestamp),
        retry_at: super::timestamp(retry_at),
        retry_after_at: timing.retry_after.map(super::timestamp),
        provider_retry_at: timing.google_retry.map(super::timestamp),
        provenance,
        budget_revision: None,
        reason: reason.into(),
        legacy: false,
    }
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn retry_after(headers: &HeaderMap, received: OffsetDateTime) -> Option<OffsetDateTime> {
    header_number(headers, "retry-after")
        .map(|seconds| deadline(whole_second(received), seconds.ceil() as u64))
        .or_else(|| {
            let value = headers.get("retry-after")?.to_str().ok()?;
            Some(OffsetDateTime::from(httpdate::parse_http_date(value).ok()?))
        })
}

fn whole_second(received: OffsetDateTime) -> OffsetDateTime {
    received.replace_nanosecond(0).unwrap()
}

fn delay_field(value: &Value, key: &str, received: OffsetDateTime) -> Option<OffsetDateTime> {
    let seconds = value[key].as_f64().filter(|n| n.is_finite() && *n >= 0.0)?;
    Some(deadline(received, seconds.ceil() as u64))
}

fn provider_reset(error: &Value, received: OffsetDateTime) -> Option<OffsetDateTime> {
    error["resets_at"]
        .as_i64()
        .and_then(|at| OffsetDateTime::from_unix_timestamp(at).ok())
        .or_else(|| delay_field(error, "resets_in_seconds", whole_second(received)))
}

fn claude_budget(
    headers: &HeaderMap,
    scope: &QuotaScope,
) -> (Option<String>, Option<OffsetDateTime>) {
    let text = |key| headers.get(key).and_then(|value| value.to_str().ok());
    if text("anthropic-ratelimit-unified-status") != Some("rejected") {
        return (None, None);
    }
    let claim = text("anthropic-ratelimit-unified-representative-claim");
    let window = match (claim, scope) {
        (Some("five_hour"), QuotaScope::Global) => Some("five-hour"),
        (Some("seven_day"), QuotaScope::Global) => Some("seven-day"),
        (_, QuotaScope::Family(family)) if family == "opus" => Some("seven-day-opus"),
        (_, QuotaScope::Family(family)) if family == "sonnet" => Some("seven-day-sonnet"),
        (Some("seven_day_overage_included"), QuotaScope::Model(model))
            if claude_family(model) == Some("fable") =>
        {
            Some("seven-day-fable")
        }
        _ => None,
    };
    let reset = header_number(headers, "anthropic-ratelimit-unified-reset")
        .and_then(|at| OffsetDateTime::from_unix_timestamp(at as i64).ok());
    (window.map(str::to_owned), reset)
}

fn codex_budget(
    headers: &HeaderMap,
    received: OffsetDateTime,
) -> (Option<String>, Option<OffsetDateTime>) {
    let limit = codex_header_budgets(headers, received)
        .filter_map(|(window, used, reset)| (used >= 100.0).then_some((window, reset?)))
        .max_by_key(|(_, reset)| *reset);
    limit.map_or((None, None), |(window, reset)| (Some(window), Some(reset)))
}

pub(crate) fn codex_header_budgets(
    headers: &HeaderMap,
    received: OffsetDateTime,
) -> impl Iterator<Item = (String, f64, Option<OffsetDateTime>)> + '_ {
    let base = whole_second(received);
    [("primary", "five-hour"), ("secondary", "weekly")]
        .into_iter()
        .filter_map(move |(bucket, default)| {
            let number = |suffix| header_number(headers, &format!("x-codex-{bucket}-{suffix}"));
            let used = number("used-percent")?;
            let reset = number("reset-at")
                .and_then(|at| OffsetDateTime::from_unix_timestamp(at as i64).ok())
                .or_else(|| {
                    number("reset-after-seconds").map(|delay| deadline(base, delay.ceil() as u64))
                });
            let window = number("window-minutes")
                .filter(|minutes| *minutes > 0.0)
                .map(|m| duration_window(m * 60.0))
                .unwrap_or_else(|| default.into());
            Some((window, used, reset))
        })
}

pub(crate) fn duration_window(seconds: f64) -> String {
    match seconds {
        18_000.0 => "five-hour".into(),
        86_400.0 => "daily".into(),
        604_800.0 => "weekly".into(),
        2_332_800.0..=2_764_800.0 => "monthly".into(),
        _ => format!("window-{}s", seconds as u64),
    }
}

fn window_exhausted(headers: &HeaderMap) -> bool {
    headers
        .get("anthropic-ratelimit-unified-status")
        .and_then(|v| v.to_str().ok())
        == Some("rejected")
        || ["primary", "secondary"].iter().any(|bucket| {
            header_number(headers, &format!("x-codex-{bucket}-used-percent"))
                .is_some_and(|used| used >= 100.0)
        })
}
