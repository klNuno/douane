use super::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<String>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub account_id: String,
    pub provider: SubscriptionProvider,
    pub windows: Vec<QuotaWindow>,
    pub updated_at: String,
    pub error: Option<String>,
    #[serde(default)]
    pub subscription: Option<SubscriptionTier>,
    #[serde(default)]
    pub credits: Vec<QuotaCredits>,
    #[serde(default)]
    pub verification_required: bool,
    #[serde(default)]
    pub verification_available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaCredits {
    pub id: String,
    pub label: String,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub unit: String,
    pub reset_at: Option<String>,
    pub unlimited: Option<bool>,
    pub enabled: Option<bool>,
}
impl QuotaSnapshot {
    pub fn weekly_reset(&self) -> Option<&str> {
        self.windows
            .iter()
            .filter(|window| is_weekly(&window.id))
            .filter_map(|window| window.reset_at.as_deref())
            .filter(|reset| parse_timestamp(reset).is_some())
            .min_by_key(|reset| parse_timestamp(reset))
    }

    /// The longest account-wide window, when it lasts a week or more, resets
    /// within `seconds` after `now`: what is left of it is lost unless spent.
    /// Windows count as the default reserve counts them, `window-<n>s` included.
    pub fn longest_window_ends_within(&self, seconds: i64, now: i64) -> bool {
        if seconds <= 0 {
            return false;
        }
        let windows = || {
            self.windows
                .iter()
                .filter_map(|window| Some((window, window_seconds(&window.id)?)))
        };
        let Some(longest) = windows().map(|(_, length)| length).reduce(f64::max) else {
            return false;
        };
        longest >= 604_800.0
            && windows()
                .filter(|(_, length)| *length == longest)
                .filter_map(|(window, _)| window.reset_at.as_deref().and_then(parse_timestamp))
                .map(|reset| reset.unix_timestamp())
                .any(|reset| reset > now && reset - now <= seconds)
    }

    pub fn remaining_percent(&self) -> Option<f64> {
        self.windows
            .iter()
            .filter(|window| is_global(&window.id))
            .filter_map(|window| window.remaining_percent)
            .reduce(f64::min)
    }

    /// Whether the weekly window of a model family (`seven-day-fable`) is
    /// used up, with its reset still ahead.
    pub fn family_week_spent(&self, family: &str, now: i64) -> bool {
        self.windows.iter().any(|window| {
            window.id.strip_prefix("seven-day-") == Some(family)
                && window.remaining_percent.is_some_and(|left| left <= 0.0)
                && window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .is_some_and(|reset| reset.unix_timestamp() > now)
        })
    }

    pub fn weekly_remaining_percent(&self) -> Option<f64> {
        self.windows
            .iter()
            .filter(|window| is_weekly(&window.id))
            .filter_map(|window| window.remaining_percent)
            .reduce(f64::min)
    }

    /// The provider reported a paid balance that keeps serving requests once
    /// the subscription windows are empty: Codex credits or Claude extra usage.
    /// Only an explicit `enabled: true` counts; an unknown balance on an
    /// enabled pool is the provider's to refuse, a known zero is not usable.
    pub fn usable_credits(&self) -> bool {
        self.credits.iter().any(|credits| {
            credits.enabled == Some(true)
                && (credits.unlimited == Some(true)
                    || credits.remaining.is_none_or(|remaining| remaining > 0.0))
        })
    }

    /// A subscription-wide window is empty and has not reset yet.
    pub fn window_exhausted(&self, now: time::OffsetDateTime) -> bool {
        self.windows.iter().any(|window| {
            is_global(&window.id)
                && window
                    .remaining_percent
                    .is_some_and(|percent| percent <= 0.0)
                && window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .is_none_or(|reset| reset > now)
        })
    }

    /// Requests would be paid from credits rather than the subscription.
    pub fn credits_only(&self, now: time::OffsetDateTime) -> bool {
        self.window_exhausted(now) && self.usable_credits()
    }

    /// A watched window, not yet reset, is at or below the account's reserve.
    /// With no `windows` chosen, only the longest window (monthly over weekly
    /// over daily over five-hour) is watched; otherwise each chosen kind is.
    /// Model-scoped windows and unknown values never count.
    pub fn reserve_reached(
        &self,
        reserve: f64,
        windows: &[String],
        now: time::OffsetDateTime,
    ) -> bool {
        self.reserve_window_remaining(windows, now)
            .is_some_and(|remaining| remaining <= reserve)
    }

    /// Lowest remaining share among the watched windows, or `None` when they
    /// are unknown or have reset since the read.
    pub fn reserve_window_remaining(
        &self,
        windows: &[String],
        now: time::OffsetDateTime,
    ) -> Option<f64> {
        let known = || {
            self.windows
                .iter()
                .filter(|window| window.remaining_percent.is_some_and(f64::is_finite))
                .filter_map(|window| Some((window, window_seconds(&window.id)?)))
        };
        let longest = if windows.is_empty() {
            Some(known().map(|(_, seconds)| seconds).reduce(f64::max)?)
        } else {
            None
        };
        known()
            .filter(|(window, seconds)| match longest {
                Some(longest) => *seconds == longest,
                None => {
                    reserve_kind(&window.id).is_some_and(|kind| windows.iter().any(|w| w == kind))
                }
            })
            .filter(|(window, _)| {
                window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .is_none_or(|reset| reset > now)
            })
            .filter_map(|(window, _)| window.remaining_percent)
            .reduce(f64::min)
    }

    /// The provider reports a known share for a window the reserve watches:
    /// any account-wide window by default, or one of the chosen kinds.
    pub fn reports_reserve_window(&self, windows: &[String]) -> bool {
        if windows.is_empty() {
            return self.reports_account_window();
        }
        self.windows.iter().any(|window| {
            window.remaining_percent.is_some_and(f64::is_finite)
                && reserve_kind(&window.id).is_some_and(|kind| windows.iter().any(|w| w == kind))
        })
    }

    /// The provider reports a known share for at least one account-wide
    /// window, whether or not that window has reset since the last read.
    pub fn reports_account_window(&self) -> bool {
        self.windows.iter().any(|window| {
            is_global(&window.id)
                && window
                    .remaining_percent
                    .is_some_and(|value| value.is_finite())
        })
    }
}

/// Window kinds a reserve can watch, shortest first.
pub const RESERVE_WINDOWS: [&str; 4] = ["five-hour", "daily", "weekly", "monthly"];

/// The reserve kind of an account-wide window id; `None` for model-scoped
/// windows and for lengths no provider names.
pub fn reserve_kind(id: &str) -> Option<&'static str> {
    match id {
        "five-hour" => Some("five-hour"),
        "daily" => Some("daily"),
        "weekly" | "seven-day" | "seven_day" => Some("weekly"),
        "monthly" => Some("monthly"),
        _ => None,
    }
}

pub(super) fn is_weekly(id: &str) -> bool {
    matches!(id, "weekly" | "seven-day" | "seven_day")
}
/// Length of an account-wide window in seconds, used to find the longest
/// one. A `window-` id without a parsable length ranks below every known one.
pub(crate) fn window_seconds(id: &str) -> Option<f64> {
    match id {
        "five-hour" => Some(18_000.0),
        "daily" => Some(86_400.0),
        "weekly" | "seven-day" | "seven_day" => Some(604_800.0),
        "monthly" => Some(30.0 * 86_400.0),
        _ => id.strip_prefix("window-").map(|rest| {
            rest.strip_suffix('s')
                .and_then(|n| n.parse::<f64>().ok())
                .filter(|n| n.is_finite())
                .unwrap_or(0.0)
        }),
    }
}
pub(super) fn is_global(id: &str) -> bool {
    id.starts_with("window-")
        || matches!(
            id,
            "five-hour" | "weekly" | "seven-day" | "seven_day" | "monthly" | "daily" | "session"
        )
}
