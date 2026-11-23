use super::*;
use crate::subscriptions::cooldown::{QuotaScope, Refusal, ScopedCooldown, safe_reason};
use time::OffsetDateTime;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct AccountQuota {
    #[serde(default = "quota_version")]
    version: u8,
    pub serial: u64,
    #[serde(default)]
    pub cleared_at: Option<String>,
    pub refusals: Vec<Refusal>,
    pub exhaustion: Vec<Exhaustion>,
    pub error: Option<AccountError>,
    #[serde(default)]
    pub measurements: BTreeMap<String, WindowReading>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observation_at: Option<String>,
    #[serde(skip)]
    pub last_poll_began_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub zeros: BTreeMap<String, WindowReading>,
    #[serde(default)]
    pub resets: BTreeMap<String, ConfirmedReset>,
}

const fn quota_version() -> u8 {
    1
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConfirmedReset {
    pub zero: WindowReading,
    pub began_at: String,
    pub observed_at: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WindowReading {
    pub observed_at: String,
    pub window: crate::subscriptions::QuotaWindow,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AccountError {
    pub id: u64,
    pub reason: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Exhaustion {
    pub scope: QuotaScope,
    pub reset_at: Option<String>,
}

impl Exhaustion {
    pub fn active(&self, now: OffsetDateTime) -> bool {
        self.reset_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_none_or(|until| until > now)
    }
}

impl AccountQuota {
    pub(super) fn new() -> Self {
        Self {
            version: quota_version(),
            ..Self::default()
        }
    }

    pub fn accepts_snapshot(&self, snapshot: &crate::subscriptions::QuotaSnapshot) -> bool {
        self.cleared_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_none_or(|cleared| {
                parse_timestamp(&snapshot.updated_at).is_some_and(|observed| observed >= cleared)
            })
    }

    pub fn record(&mut self, mut refusal: Refusal, now: OffsetDateTime) {
        self.refusals.retain(|old| old.active(now));
        if !refusal.active(now) {
            return;
        }
        // A repeat gets its own identity, even at the same deadline. Separate
        // sources/deadlines coexist so new evidence cannot shorten a pause.
        let same = |old: &Refusal| {
            old.kind == refusal.kind
                && old.scope == refusal.scope
                && old.window == refusal.window
                && old.retry_at == refusal.retry_at
                && old.reset_at == refusal.reset_at
                && old.retry_after_at == refusal.retry_after_at
                && old.provider_retry_at == refusal.provider_retry_at
                && old.provenance == refusal.provenance
                && old.budget_revision == refusal.budget_revision
        };
        if self.refusals.iter().any(|old| {
            same(old) && parse_timestamp(&old.started_at) > parse_timestamp(&refusal.started_at)
        }) {
            return;
        }
        self.refusals.retain(|old| !same(old));
        self.serial += 1;
        refusal.id = self.serial;
        self.refusals.push(refusal);
    }

    pub fn active_refusals(&self, now: OffsetDateTime) -> impl Iterator<Item = &Refusal> {
        self.refusals
            .iter()
            .filter(move |refusal| refusal.active(now))
    }

    pub fn set_error(&mut self, reason: Option<&str>) {
        self.serial += 1;
        self.error = reason.map(|reason| AccountError {
            id: self.serial,
            reason: safe_reason(reason).into(),
        });
    }

    pub fn clear_error_observed_by(&mut self, started: &Self) {
        if let Some(error) = &self.error
            && started.error.as_ref().is_some_and(|old| old.id == error.id)
        {
            self.error = None;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionProvider {
    Codex,
    Claude,
    Gemini,
    Antigravity,
    Qwen,
    Kimi,
    Xai,
    OpencodeGo,
    Copilot,
    Meta,
    OpenaiCompatible,
}

impl SubscriptionProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
            Self::Qwen => "qwen",
            Self::Kimi => "kimi",
            Self::Xai => "xai",
            Self::OpencodeGo => "opencode_go",
            Self::Copilot => "copilot",
            Self::Meta => "meta",
            Self::OpenaiCompatible => "openai_compatible",
        }
    }

    /// Whether every account on this provider has a quota reader. Compatible
    /// endpoints may report billing data depending on their base URL.
    pub fn reports_quota(self) -> bool {
        !matches!(self, Self::OpenaiCompatible | Self::Qwen)
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" | "chatgpt" => Some(Self::Codex),
            "claude" | "anthropic" => Some(Self::Claude),
            "gemini" | "gemini-cli" => Some(Self::Gemini),
            "antigravity" => Some(Self::Antigravity),
            "qwen" => Some(Self::Qwen),
            "kimi" | "kimi.ai" | "kimi-ai" => Some(Self::Kimi),
            "xai" | "grok" => Some(Self::Xai),
            "opencode_go" | "opencode-go" | "opencode" => Some(Self::OpencodeGo),
            "meta" | "muse" | "muse-code" => Some(Self::Meta),
            "copilot" | "github-copilot" | "github_copilot" => Some(Self::Copilot),
            "openai" | "openai_compatible" | "openai-compatible" => Some(Self::OpenaiCompatible),
            _ => None,
        }
    }
}

/// Provider-confirmed plan metadata. No credential or arbitrary provider body
/// can enter a management response through this type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionTier {
    pub id: String,
    pub label: String,
    pub multiplier: Option<u32>,
    /// How much this plan counts against the provider's smallest paid plan
    /// when the provider states no multiplier: the provider's published
    /// ratio, never a guess. The pool and the average weigh accounts by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_weight: Option<u32>,
    pub source: String,
}

/// Published allowance ratios of plans the provider reports without a
/// multiplier. Codex: Pro is 20x Plus and Pro Lite 5x (OpenAI pricing, 2026-04).
pub(crate) fn default_plan_weight(provider: SubscriptionProvider, id: &str) -> Option<u32> {
    match (provider, id) {
        (SubscriptionProvider::Codex, "pro") => Some(20),
        (SubscriptionProvider::Codex, "prolite") => Some(5),
        (SubscriptionProvider::Codex, "plus") => Some(1),
        _ => None,
    }
}

impl SubscriptionTier {
    /// Sets the published ratio from the current table, so a tier saved
    /// before it (an account file, a snapshot from a previous run) carries it.
    pub(crate) fn with_default_weight(mut self, provider: SubscriptionProvider) -> Self {
        self.default_weight = default_plan_weight(provider, &self.id);
        self
    }

    /// The weight of one account of this plan before normalisation: the
    /// provider's multiplier, else the published ratio, else 1.
    pub fn weight(&self, provider: SubscriptionProvider) -> f64 {
        self.multiplier
            .filter(|m| *m > 0)
            .or_else(|| default_plan_weight(provider, &self.id))
            .unwrap_or(1) as f64
    }

    pub(crate) fn detect(
        provider: SubscriptionProvider,
        plan: Option<&str>,
        rate_tier: Option<&str>,
        source: &'static str,
    ) -> Option<Self> {
        let plan = plan.unwrap_or_default().trim().to_ascii_lowercase();
        let rate_tier = rate_tier.unwrap_or_default().trim().to_ascii_lowercase();
        let (id, label, multiplier) = match provider {
            SubscriptionProvider::Claude => match rate_tier.as_str() {
                "default_claude_max_20x" => ("max", "Claude Max", Some(20)),
                "default_claude_max_5x" => ("max", "Claude Max", Some(5)),
                _ => match plan.trim_start_matches("claude_") {
                    "max" => ("max", "Claude Max", None),
                    "pro" => ("pro", "Claude Pro", None),
                    "team" => ("team", "Claude Team", None),
                    "enterprise" => ("enterprise", "Claude Enterprise", None),
                    "free" => ("free", "Claude Free", None),
                    _ => return None,
                },
            },
            SubscriptionProvider::Codex => match plan.as_str() {
                "pro" => ("pro", "Codex Pro", None),
                "prolite" => ("prolite", "Codex Pro Lite", None),
                "promax" => ("promax", "Codex Pro Max", None),
                "pro_20x" | "pro-20x" => ("pro", "Codex Pro", Some(20)),
                "pro_5x" | "pro-5x" => ("pro", "Codex Pro", Some(5)),
                "plus" => ("plus", "Codex Plus", None),
                "go" => ("go", "Codex Go", None),
                "team" => ("team", "Codex Team", None),
                "business" => ("business", "Codex Business", None),
                "enterprise" => ("enterprise", "Codex Enterprise", None),
                "free" | "guest" => ("free", "Codex Free", None),
                _ => return None,
            },
            SubscriptionProvider::Xai => match plan.replace([' ', '-'], "_").as_str() {
                "supergrok" => ("supergrok", "SuperGrok", None),
                "supergrok_heavy" | "heavy" => ("supergrok_heavy", "SuperGrok Heavy", None),
                "premium" | "x_premium" => ("premium", "X Premium", None),
                "premium_plus" | "x_premium+" => ("premium_plus", "X Premium+", None),
                "free" => ("free", "Grok Free", None),
                _ => return None,
            },
            SubscriptionProvider::OpencodeGo if plan == "go" || plan == "opencode_go" => {
                ("go", "OpenCode Go", None)
            }
            // GitHub names the plan by its SKU; education plans carry their own.
            SubscriptionProvider::Copilot => match plan.as_str() {
                sku if sku.contains("educational") || sku == "student" => {
                    ("student", "Copilot Student", None)
                }
                sku if sku.contains("plus") => ("pro_plus", "Copilot Pro+", None),
                "free" | "free_limited_copilot" => ("free", "Copilot Free", None),
                "pro" | "copilot_pro" | "individual" | "monthly_subscriber"
                | "yearly_subscriber" => ("pro", "Copilot Pro", None),
                "business" | "copilot_business" => ("business", "Copilot Business", None),
                "enterprise" | "copilot_enterprise" => ("enterprise", "Copilot Enterprise", None),
                _ => return None,
            },
            _ => return None,
        };
        Some(Self {
            id: id.into(),
            label: multiplier
                .map(|multiple| format!("{label} x{multiple}"))
                .unwrap_or_else(|| label.into()),
            multiplier,
            default_weight: default_plan_weight(provider, id),
            source: source.into(),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountSummary {
    pub id: String,
    pub name: String,
    pub provider: SubscriptionProvider,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub subscription: Option<SubscriptionTier>,
    pub enabled: bool,
    pub status: String,
    pub last_error: Option<String>,
    pub(crate) refusals: Vec<Refusal>,
    pub(crate) status_reason: Option<String>,
    pub(crate) retry_at: Option<String>,

    pub reserve_percent: Option<f64>,
    pub reserve_windows: Vec<String>,
    /// Kept out of the Quotas page and the provider pool, still routed.
    pub hidden: bool,
    /// A custom endpoint's model prefix, address, wire format and model
    /// filter. Absent for subscriptions; never a credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// Images a Chat request to this endpoint carries at most; absent uses
    /// the default of 20.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_limit: Option<u32>,
    /// Entries added to each request's `metadata`; see
    /// [`crate::endpoint_metadata`].
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip)]
    pub(crate) endpoint_revision: u64,
}

/// Request credentials are available only inside the broker. Do not serialize
/// this type into management responses or derive Debug for it.
#[derive(Clone, Serialize, Deserialize)]
pub struct AccountCredential {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) provider: SubscriptionProvider,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    /// Meta device credential, distinct from its minted inference key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dca_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dca_expires_at: Option<String>,
    pub(crate) id_token: Option<String>,
    pub(crate) account_id: Option<String>,
    #[serde(default)]
    pub(crate) organization_id: Option<String>,
    pub(crate) base_url: Option<String>,
    #[serde(default)]
    pub(crate) models: Vec<String>,
    pub(crate) project_id: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) plan: Option<String>,
    #[serde(default)]
    pub(crate) subscription: Option<SubscriptionTier>,
    pub(crate) expires_at: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    #[serde(skip)]
    pub(crate) endpoint_revision: u64,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    #[serde(default)]
    pub(crate) cooldown_until: Option<String>,
    #[serde(default)]
    pub(crate) quota: AccountQuota,
    #[serde(default)]
    pub(crate) budget_revision: u64,
    /// An explicit provider retry delay survives a successful usage read.
    /// Older records have no provenance and are revalidated from fresh quotas.
    #[serde(default)]
    pub(crate) cooldown_retry_after: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) scoped_cooldowns: Vec<ScopedCooldown>,
    #[serde(default)]
    pub(crate) exhausted_until: Option<String>,
    #[serde(default)]
    pub(crate) exhausted_without_reset: bool,
    #[serde(default)]
    pub(crate) client_id: Option<String>,
    #[serde(default)]
    pub(crate) client_secret: Option<String>,
    #[serde(default)]
    pub(crate) token_endpoint: Option<String>,
    #[serde(default)]
    pub(crate) model_exhaustion: BTreeMap<String, Option<String>>,
    /// Share of every account-wide quota window that douane leaves unused.
    #[serde(default)]
    pub(crate) reserve_percent: Option<f64>,
    /// Window kinds the reserve watches; empty watches the longest window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) reserve_windows: Vec<String>,
    /// Model prefix of a custom endpoint (`deepseek` serves `deepseek/...`).
    /// Keys sharing a route rotate; keys on different routes never mix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<String>,
    /// Wire format a custom endpoint speaks: `chat`, `responses` or
    /// `messages`. Absent forwards the client's own format unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dialect: Option<String>,
    /// Images a Chat request to this endpoint carries at most: older ones
    /// become a text placeholder. Absent uses [`crate::chat_images::DEFAULT_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) image_limit: Option<u32>,
    /// Entries a custom endpoint adds to each request's `metadata`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) metadata: BTreeMap<String, String>,
    /// The user hid it from the Quotas page; routing ignores this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) hidden: bool,
}

/// A reserve is a share strictly between 0 and 100.
pub fn valid_reserve(percent: f64) -> bool {
    percent.is_finite() && percent > 0.0 && percent < 100.0
}
/// Known reserve window kinds, deduplicated in shortest-first order, or
/// `None` when one is unknown.
pub fn reserve_windows(kinds: &[String]) -> Option<Vec<String>> {
    if kinds
        .iter()
        .any(|kind| !crate::subscriptions::RESERVE_WINDOWS.contains(&kind.as_str()))
    {
        return None;
    }
    Some(
        crate::subscriptions::RESERVE_WINDOWS
            .iter()
            .filter(|known| kinds.iter().any(|kind| kind == *known))
            .map(|known| known.to_string())
            .collect(),
    )
}

impl AccountCredential {
    /// Images a Chat request to this account may carry.
    pub(crate) fn image_limit(&self) -> u32 {
        self.image_limit
            .unwrap_or(crate::chat_images::DEFAULT_LIMIT)
    }

    /// The stored reserve, ignored when a hand-edited file holds a value
    /// outside the accepted range.
    pub(crate) fn reserve(&self) -> Option<f64> {
        self.reserve_percent
            .filter(|percent| valid_reserve(*percent))
    }

    /// The watched window kinds; a hand-edited unknown kind falls back to
    /// the longest window rather than silently watching nothing.
    pub(crate) fn reserve_windows(&self) -> Vec<String> {
        reserve_windows(&self.reserve_windows).unwrap_or_default()
    }

    pub(crate) fn offers_model(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m == model)
    }

    pub(crate) fn serves(&self, model: &str) -> bool {
        self.serves_at(model, OffsetDateTime::now_utc())
    }

    pub(crate) fn serves_at(&self, model: &str, now: OffsetDateTime) -> bool {
        self.offers_model(model)
            && !self.quota.exhaustion.iter().any(|bucket| {
                bucket.scope != QuotaScope::Global
                    && bucket.scope.applies(model)
                    && bucket.active(now)
            })
            && !self
                .quota
                .active_refusals(now)
                .any(|refusal| refusal.scope != QuotaScope::Global && refusal.scope.applies(model))
    }

    pub(crate) fn blocked(&self, model: &str) -> bool {
        let now = OffsetDateTime::now_utc();
        self.quota
            .exhaustion
            .iter()
            .any(|bucket| bucket.active(now) && bucket.scope.applies(model))
            || self
                .quota
                .active_refusals(now)
                .any(|refusal| refusal.scope.applies(model))
    }

    /// A replay owner receives the latest applicable deadline locally.
    pub(crate) fn retry_at(&self, model: &str) -> Option<OffsetDateTime> {
        let now = OffsetDateTime::now_utc();
        self.quota
            .active_refusals(now)
            .filter(|refusal| refusal.scope.applies(model))
            .filter_map(|refusal| parse_timestamp(&refusal.retry_at))
            .chain(
                self.exhaustion_deadlines(now)
                    .filter_map(|(scope, deadline)| scope.applies(model).then_some(deadline)),
            )
            .max()
    }

    fn exhaustion_deadlines(
        &self,
        now: OffsetDateTime,
    ) -> impl Iterator<Item = (&QuotaScope, OffsetDateTime)> {
        self.quota.exhaustion.iter().filter_map(move |bucket| {
            let reset = parse_timestamp(bucket.reset_at.as_deref()?)?;
            (reset > now).then_some((&bucket.scope, reset))
        })
    }

    pub(crate) fn resting(&self) -> bool {
        self.resting_at(OffsetDateTime::now_utc())
    }

    pub(crate) fn resting_at(&self, now: OffsetDateTime) -> bool {
        self.status_at(now) == "cooldown"
            && self
                .quota
                .active_refusals(now)
                .filter(|refusal| refusal.scope == QuotaScope::Global)
                .all(|refusal| refusal.retryable_at(now))
    }

    pub(crate) fn status_at(&self, now: OffsetDateTime) -> &'static str {
        if !self.enabled {
            "disabled"
        } else if self
            .quota
            .exhaustion
            .iter()
            .any(|bucket| bucket.scope == QuotaScope::Global && bucket.active(now))
        {
            "exhausted"
        } else if self
            .quota
            .active_refusals(now)
            .any(|refusal| refusal.scope == QuotaScope::Global)
        {
            "cooldown"
        } else if self
            .expires_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|t| t <= now)
            && self.refresh_token.is_none()
        {
            "expired"
        } else if self.quota.error.is_some() {
            "error"
        } else {
            "ready"
        }
    }

    fn primary_refusal(&self, now: OffsetDateTime) -> Option<&Refusal> {
        self.quota
            .active_refusals(now)
            .filter(|refusal| refusal.scope == QuotaScope::Global)
            .max_by_key(|refusal| {
                (
                    !refusal.retryable_at(now),
                    parse_timestamp(&refusal.retry_at),
                    refusal.id,
                )
            })
    }

    /// Import pre-typed account files. Refusal strings are normalized once.
    pub(super) fn migrate_quota(&mut self) {
        if self.quota.version != 0 {
            return;
        }
        self.quota.version = quota_version();
        let now = OffsetDateTime::now_utc();
        if let Some(until) = &self.cooldown_until {
            if let Some(deadline) = parse_timestamp(until) {
                let started = parse_timestamp(&self.updated_at).unwrap_or(now);
                let seconds = (deadline - started).whole_seconds().max(0) as u64;
                let mut refusal = Refusal::timed(
                    started,
                    seconds,
                    self.last_error
                        .as_deref()
                        .unwrap_or("provider request failed"),
                    QuotaScope::Global,
                    self.cooldown_retry_after,
                );
                refusal.retry_at = until.clone();
                refusal.retry_after_at = self.cooldown_retry_after.then(|| until.clone());
                refusal.legacy = true;
                self.quota.record(refusal, now);
            }
        } else if let Some(error) = &self.last_error
            && !matches!(
                error.as_str(),
                "upstream quota exceeded"
                    | "websocket upstream quota exceeded"
                    | "upstream rate limit reached"
                    | "websocket upstream rate limited"
                    | "rate limited"
            )
        {
            self.quota.set_error(Some(error));
        }
        for old in &self.scoped_cooldowns {
            let mut refusal = Refusal::timed(
                now,
                0,
                "upstream quota exceeded",
                old.scope.clone(),
                old.retry_after,
            );
            refusal.retry_at = old.until.clone();
            refusal.retry_after_at = old.retry_after.then(|| old.until.clone());
            refusal.legacy = true;
            self.quota.record(refusal, now);
        }
        if self.exhausted_without_reset {
            self.quota.exhaustion.push(Exhaustion {
                scope: QuotaScope::Global,
                reset_at: None,
            });
        }
        if self.exhausted_until.is_some() {
            self.quota.exhaustion.push(Exhaustion {
                scope: QuotaScope::Global,
                reset_at: self.exhausted_until.clone(),
            });
        }
        self.quota
            .exhaustion
            .extend(self.model_exhaustion.iter().map(|(id, reset)| Exhaustion {
                scope: if matches!(id.as_str(), "fable" | "opus" | "sonnet")
                    && self.provider == SubscriptionProvider::Claude
                {
                    QuotaScope::Family(id.clone())
                } else {
                    QuotaScope::Model(id.clone())
                },
                reset_at: reset.clone(),
            }));
    }

    /// Keep the old JSON shape available to backup tools and older binaries.
    pub(super) fn project_quota(&mut self) {
        let now = OffsetDateTime::now_utc();
        let global = self
            .quota
            .active_refusals(now)
            .filter(|r| r.scope == QuotaScope::Global)
            .max_by_key(|r| (parse_timestamp(&r.retry_at), r.id));
        self.cooldown_until = global.map(|r| r.retry_at.clone());
        self.cooldown_retry_after = global.is_some_and(Refusal::retry_from_explicit_delay);
        self.last_error = self
            .primary_refusal(now)
            .map(|r| r.reason.clone())
            .or_else(|| self.quota.error.as_ref().map(|error| error.reason.clone()));

        let mut scoped = BTreeMap::<_, &Refusal>::new();
        for refusal in self
            .quota
            .active_refusals(now)
            .filter(|r| r.scope != QuotaScope::Global)
        {
            let selected = scoped.entry(refusal.scope.clone()).or_insert(refusal);
            if parse_timestamp(&selected.retry_at) < parse_timestamp(&refusal.retry_at) {
                *selected = refusal;
            }
        }
        self.scoped_cooldowns = scoped
            .into_values()
            .map(|r| ScopedCooldown {
                scope: r.scope.clone(),
                until: r.retry_at.clone(),
                retry_after: r.retry_from_explicit_delay(),
            })
            .collect();
        self.exhausted_without_reset = self
            .quota
            .exhaustion
            .iter()
            .any(|bucket| bucket.scope == QuotaScope::Global && bucket.reset_at.is_none());
        self.exhausted_until = self
            .exhaustion_deadlines(now)
            .filter(|(scope, _)| **scope == QuotaScope::Global)
            .map(|(_, until)| until)
            .max()
            .map(timestamp);
        self.model_exhaustion.clear();
        for bucket in self
            .quota
            .exhaustion
            .iter()
            .filter(|bucket| bucket.active(now))
        {
            let id = match &bucket.scope {
                QuotaScope::Family(id) | QuotaScope::Model(id) => id,
                QuotaScope::Global => continue,
            };
            let reset = self
                .model_exhaustion
                .entry(id.clone())
                .or_insert_with(|| bucket.reset_at.clone());
            if bucket.reset_at.is_none()
                || reset
                    .as_deref()
                    .and_then(parse_timestamp)
                    .zip(bucket.reset_at.as_deref().and_then(parse_timestamp))
                    .is_some_and(|(left, right)| left < right)
            {
                *reset = bucket.reset_at.clone();
            }
        }
    }

    pub(crate) fn summary(&self) -> AccountSummary {
        let current = OffsetDateTime::now_utc();
        let status = self.status_at(current);
        let primary = self.primary_refusal(current);
        let refusals = self.quota.active_refusals(current).cloned().collect();
        let retry_at = self
            .quota
            .active_refusals(current)
            .filter(|r| r.scope == QuotaScope::Global)
            .filter_map(|r| parse_timestamp(&r.retry_at))
            .chain(
                self.exhaustion_deadlines(current)
                    .filter_map(|(scope, at)| (*scope == QuotaScope::Global).then_some(at)),
            )
            .max()
            .map(timestamp);
        AccountSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            provider: self.provider,
            email: self.email.clone(),
            plan: self.plan.clone(),
            subscription: self
                .subscription
                .clone()
                .map(|tier| tier.with_default_weight(self.provider))
                .or_else(|| {
                    SubscriptionTier::detect(
                        self.provider,
                        self.plan.as_deref(),
                        None,
                        "credential",
                    )
                }),
            enabled: self.enabled,
            status: status.into(),
            last_error: primary
                .map(|r| r.reason.clone())
                .or_else(|| self.quota.error.as_ref().map(|error| error.reason.clone())),
            refusals,
            status_reason: primary
                .map(|r| r.reason.clone())
                .or_else(|| (status == "exhausted").then(|| "subscription quota exhausted".into())),
            retry_at,
            reserve_percent: self.reserve(),
            reserve_windows: self.reserve_windows(),
            hidden: self.hidden,
            route: self.route.clone(),
            base_url: (self.provider == SubscriptionProvider::OpenaiCompatible)
                .then(|| self.base_url.clone())
                .flatten(),
            dialect: self.dialect.clone(),
            image_limit: self.image_limit,
            metadata: self.metadata.clone(),
            models: if self.provider == SubscriptionProvider::OpenaiCompatible {
                self.models.clone()
            } else {
                vec![]
            },
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            endpoint_revision: self.endpoint_revision,
        }
    }

    /// The pool this account serves: a custom endpoint's route, otherwise
    /// its provider. Scheduling, affinity and model prefixes use it.
    pub(crate) fn pool(&self) -> &str {
        self.route.as_deref().unwrap_or(self.provider.as_str())
    }

    pub(crate) fn needs_refresh(&self) -> bool {
        if self.provider == SubscriptionProvider::Meta {
            return self.access_token.starts_with("dca:") && self.dca_token.is_some();
        }
        self.expires_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|t| t <= time::OffsetDateTime::now_utc() + time::Duration::minutes(5))
    }
}
