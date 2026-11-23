use super::*;
use time::{Duration, OffsetDateTime};

impl QuotaService {
    #[cfg(test)]
    pub(super) async fn observe_claude_headers(
        &self,
        account: &AccountCredential,
        headers: &reqwest::header::HeaderMap,
    ) {
        self.observe_response_headers_at(account, headers, OffsetDateTime::now_utc(), false)
            .await;
    }

    pub(crate) async fn observe_response_headers_at(
        &self,
        account: &AccountCredential,
        headers: &reqwest::header::HeaderMap,
        received: OffsetDateTime,
        accepted: bool,
    ) -> bool {
        let windows = match account.provider {
            SubscriptionProvider::Claude => claude_header_windows(headers),
            SubscriptionProvider::Codex => codex_header_windows(headers, received),
            _ => return false,
        };
        if windows.is_empty() {
            return false;
        }
        let observation = Observation {
            windows,
            received,
            plan: None,
            subscription: None,
            credits: vec![],
            organization_id: None,
        };
        self.publish_observation_inner(account, observation, received, false, accepted)
            .await
            .is_ok()
    }

    /// Muse reports subscription use in inference SSE and in key mint replies.
    /// Never send a synthetic inference to poll it.
    #[cfg(test)]
    pub(super) async fn observe_meta_event(&self, id: &str, event: &Value) {
        self.observe_meta_event_at(id, event, time::OffsetDateTime::now_utc())
            .await;
    }

    pub(crate) async fn observe_meta_event_at(
        &self,
        id: &str,
        event: &Value,
        observed: time::OffsetDateTime,
    ) {
        if !crate::subscriptions::meta::subscription_event(event) {
            return;
        }
        let Some(account) = self
            .store
            .get(id)
            .await
            .filter(|a| a.provider == SubscriptionProvider::Meta)
        else {
            return;
        };
        let mut windows = Vec::new();
        for (key, default_id, label) in [
            ("weekly", "weekly", "Weekly"),
            ("window", "session", "Session"),
        ] {
            let source = &event["subscription"][key];
            let Some(used) = source["used_percent"]
                .as_f64()
                .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
            else {
                continue;
            };
            let reset = source["resets_at"]
                .as_i64()
                .and_then(|n| time::OffsetDateTime::from_unix_timestamp(n).ok())
                .map(timestamp);
            let id = if key == "window" {
                source["window_duration_mins"]
                    .as_u64()
                    .filter(|n| *n > 0 && *n < 525600)
                    .map(|n| {
                        let candidate = match n {
                            300 => "five-hour".into(),
                            1440 => "daily".into(),
                            10080 => "weekly".into(),
                            38880..=46080 => "monthly".into(),
                            n => format!("window-{}s", n * 60),
                        };
                        if windows
                            .iter()
                            .any(|existing: &QuotaWindow| existing.id == candidate)
                        {
                            format!("window-{}s", n * 60)
                        } else {
                            candidate
                        }
                    })
                    .unwrap_or_else(|| default_id.into())
            } else {
                default_id.into()
            };
            windows.push(QuotaWindow {
                id,
                label: label.into(),
                remaining_percent: Some(100.0 - used),
                reset_at: reset,
                used: None,
                limit: None,
            });
        }
        if windows.is_empty() {
            return;
        }
        let observation = Observation {
            windows,
            received: observed,
            plan: None,
            subscription: None,
            credits: vec![],
            organization_id: None,
        };
        if self
            .publish_observation(&account, observation, observed, false)
            .await
            .is_ok()
        {
            self.save().await;
        }
    }

    pub(super) async fn publish_observation(
        &self,
        started: &AccountCredential,
        observation: Observation,
        began: OffsetDateTime,
        poll: bool,
    ) -> Result<QuotaSnapshot, SubscriptionError> {
        self.publish_observation_inner(started, observation, began, poll, false)
            .await
    }

    async fn publish_observation_inner(
        &self,
        started: &AccountCredential,
        observation: Observation,
        began: OffsetDateTime,
        poll: bool,
        accepted: bool,
    ) -> Result<QuotaSnapshot, SubscriptionError> {
        let _guard = self.transitions.lock().await;
        let id = &started.id;
        let previous = self.cache.read().await.get(id).cloned();
        let mut outcome = None;
        self.store
            .modify_with(id, |stored, account| {
                if account.budget_revision != started.budget_revision
                    || account.provider != started.provider
                    || account
                        .quota
                        .cleared_at
                        .as_deref()
                        .is_some_and(|at| newer_than(at, began))
                {
                    return false;
                }
                let previous = snapshot_for(account, previous);
                let freshest = previous
                    .as_ref()
                    .is_some_and(|s| newer_than(&s.updated_at, began));
                let mut snapshot =
                    previous.unwrap_or_else(|| empty_snapshot(account, observation.received));
                if account
                    .quota
                    .last_poll_began_at
                    .is_some_and(|last| last > began)
                {
                    outcome = Some((snapshot, false));
                    return false;
                }
                let (windows, windows_changed) = merge_windows(
                    &mut account.quota,
                    &mut snapshot,
                    observation.windows,
                    began,
                    observation.received,
                    poll,
                );
                if poll {
                    account.quota.last_poll_began_at = Some(began);
                    snapshot.credits = observation.credits;
                    if let Some(subscription) = observation.subscription {
                        account.plan = Some(subscription.id.clone());
                        snapshot.subscription = Some(subscription.clone());
                        account.subscription = Some(subscription);
                    } else if let Some(plan) = observation.plan {
                        account.plan = Some(plan);
                    }
                    if let Some(organization) = observation.organization_id {
                        account.organization_id = Some(organization);
                    }
                }
                let observed = observation.received;
                if !newer_than(&snapshot.updated_at, observed) {
                    snapshot.updated_at = timestamp(observed);
                }
                account.quota.last_observation_at = Some(snapshot.updated_at.clone());
                resolve_refusals(
                    account,
                    started,
                    &snapshot.windows,
                    &windows,
                    began,
                    observed,
                    poll,
                );
                let allow_global_exhaustion = poll
                    || !accepted
                    || account.provider != SubscriptionProvider::Codex
                    || account.quota.last_poll_began_at.is_some();
                apply_exhaustion(account, &snapshot, allow_global_exhaustion);
                outcome = Some((snapshot, poll || !freshest));
                windows_changed
                    || account.quota.exhaustion != stored.quota.exhaustion
                    || account.quota.zeros != stored.quota.zeros
                    || resets_differ(&account.quota.resets, &stored.quota.resets)
                    || account.quota.refusals.len() != stored.quota.refusals.len()
                    || account.quota.error.is_some() != stored.quota.error.is_some()
                    || account.plan != stored.plan
                    || account.subscription != stored.subscription
                    || account.organization_id != stored.organization_id
            })
            .await?;
        let (mut snapshot, publish_health) = outcome.ok_or(SubscriptionError::NotFound)?;
        if publish_health {
            let mut failures = self.failures.write().await;
            if failures
                .get(id)
                .is_none_or(|failure| failure.began <= began)
            {
                failures.remove(id);
                snapshot.error = None;
                snapshot.verification_required = false;
                snapshot.verification_available = false;
                self.verification_urls.write().await.remove(id);
            }
        }
        self.cache
            .write()
            .await
            .insert(id.clone(), snapshot.clone());
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(super) async fn apply_observation(
        &self,
        id: &str,
        snapshot: &QuotaSnapshot,
        plan: Option<String>,
        started: &AccountCredential,
    ) -> Result<(), SubscriptionError> {
        let observed = parse_timestamp(&snapshot.updated_at).unwrap();
        if snapshot.error.is_some()
            || snapshot.account_id != id
            || snapshot.provider != started.provider
        {
            return Ok(());
        }
        self.publish_observation(
            started,
            Observation {
                windows: snapshot.windows.clone(),
                plan,
                subscription: snapshot.subscription.clone(),
                credits: snapshot.credits.clone(),
                organization_id: None,
                received: observed,
            },
            observed,
            true,
        )
        .await
        .map(|_| ())
    }
}

pub(super) fn newer_than(at: &str, reference: OffsetDateTime) -> bool {
    parse_timestamp(at).is_some_and(|at| at > reference)
}

fn resets_differ(
    current: &std::collections::BTreeMap<String, crate::subscriptions::store::ConfirmedReset>,
    stored: &std::collections::BTreeMap<String, crate::subscriptions::store::ConfirmedReset>,
) -> bool {
    current.len() != stored.len()
        || current
            .iter()
            .any(|(id, proof)| stored.get(id).is_none_or(|old| old.zero != proof.zero))
}

fn empty_snapshot(account: &AccountCredential, observed: OffsetDateTime) -> QuotaSnapshot {
    QuotaSnapshot {
        account_id: account.id.clone(),
        provider: account.provider,
        windows: vec![],
        updated_at: timestamp(observed),
        error: None,
        subscription: account.summary().subscription,
        credits: vec![],
        verification_required: false,
        verification_available: false,
    }
}

pub(super) fn snapshot_for(
    account: &AccountCredential,
    cached: Option<QuotaSnapshot>,
) -> Option<QuotaSnapshot> {
    let mut snapshot = cached.filter(|s| account.quota.accepts_snapshot(s));
    let quota = &account.quota;
    let Some(latest) = quota
        .last_observation_at
        .as_deref()
        .and_then(parse_timestamp)
        .or_else(|| {
            quota
                .measurements
                .values()
                .filter_map(|reading| parse_timestamp(&reading.observed_at))
                .max()
        })
    else {
        return snapshot;
    };
    if snapshot
        .as_ref()
        .is_none_or(|s| !newer_than(&s.updated_at, latest))
    {
        let snapshot = snapshot.get_or_insert_with(|| empty_snapshot(account, latest));
        snapshot.updated_at = timestamp(latest);
        // A cached snapshot can still carry windows removed by a complete poll.
        if quota.last_observation_at.is_some() {
            snapshot
                .windows
                .retain(|window| quota.measurements.contains_key(&window.id));
        }
        for reading in quota.measurements.values() {
            if let Some(existing) = snapshot
                .windows
                .iter_mut()
                .find(|w| w.id == reading.window.id)
            {
                *existing = reading.window.clone();
            } else {
                snapshot.windows.push(reading.window.clone());
            }
        }
    }
    snapshot
}

fn merge_windows(
    quota: &mut AccountQuota,
    snapshot: &mut QuotaSnapshot,
    incoming: Vec<QuotaWindow>,
    began: OffsetDateTime,
    observed: OffsetDateTime,
    complete: bool,
) -> (Vec<QuotaWindow>, bool) {
    // Recover legacy snapshots and cache-only readings saved after the account.
    let seed = match quota
        .last_observation_at
        .as_deref()
        .and_then(parse_timestamp)
    {
        Some(last) => newer_than(&snapshot.updated_at, last),
        None => quota.measurements.is_empty(),
    };
    if seed {
        quota.measurements.clear();
        for window in &snapshot.windows {
            let reading = WindowReading {
                window: window.clone(),
                observed_at: snapshot.updated_at.clone(),
            };
            quota
                .measurements
                .insert(window.id.clone(), reading.clone());
            if window.remaining_percent == Some(0.0)
                && window
                    .reset_at
                    .as_deref()
                    .and_then(parse_timestamp)
                    .is_some()
            {
                quota.zeros.entry(window.id.clone()).or_insert(reading);
            }
        }
    }
    let mut durable_change = false;
    if complete {
        let before = quota.measurements.len();
        quota.measurements.retain(|id, reading| {
            newer_than(&reading.observed_at, began)
                || incoming.iter().any(|window| &window.id == id)
        });
        snapshot
            .windows
            .retain(|window| quota.measurements.contains_key(&window.id));
        durable_change = before != quota.measurements.len();
    }
    let reset = |window: &QuotaWindow| window.reset_at.as_deref().and_then(parse_timestamp);
    let mut accepted = Vec::new();
    for (position, evidence) in incoming.into_iter().enumerate() {
        let mut window = evidence.clone();
        if let Some(last) = quota.measurements.get(&window.id) {
            if newer_than(&last.observed_at, began) {
                continue;
            }
            if window.reset_at.is_none() {
                window.reset_at.clone_from(&last.window.reset_at);
            }
            let same_cycle = reset(&window) == reset(&last.window);
            durable_change |= !same_cycle
                && (last.window.remaining_percent == Some(0.0)
                    || window.remaining_percent == Some(0.0));
            if same_cycle && window.remaining_percent.is_none() {
                window.remaining_percent = last.window.remaining_percent;
                window.used = last.window.used;
            }
        }
        let reading = WindowReading {
            window: window.clone(),
            observed_at: timestamp(observed),
        };
        quota.measurements.insert(window.id.clone(), reading);
        match snapshot.windows.iter_mut().find(|old| old.id == window.id) {
            Some(old) => *old = window.clone(),
            None => snapshot
                .windows
                .insert(position.min(snapshot.windows.len()), window.clone()),
        }
        if evidence.remaining_percent.is_some() {
            accepted.push(window);
        }
    }
    (accepted, durable_change)
}

fn apply_exhaustion(account: &mut AccountCredential, snapshot: &QuotaSnapshot, allow_global: bool) {
    let credits = snapshot.usable_credits();
    account.quota.exhaustion = snapshot
        .windows
        .iter()
        .filter_map(|window| {
            if window.remaining_percent != Some(0.0) {
                return None;
            }
            let scope = if is_global(&window.id) {
                if credits || !allow_global {
                    return None;
                }
                QuotaScope::Global
            } else {
                window_scope(&window.id)?
            };
            let reset_at = window
                .reset_at
                .as_deref()
                .and_then(parse_timestamp)
                .map(timestamp);
            Some(Exhaustion { scope, reset_at })
        })
        .collect();
}

#[allow(clippy::too_many_arguments)]
fn resolve_refusals(
    account: &mut AccountCredential,
    started: &AccountCredential,
    windows: &[QuotaWindow],
    fresh: &[QuotaWindow],
    began: OffsetDateTime,
    observed: OffsetDateTime,
    poll: bool,
) {
    let provider = account.provider;
    let quota = &mut account.quota;
    if poll {
        quota.clear_error_observed_by(&started.quota);
    }
    let global_positive = global_positive(windows.iter());
    let measured_global = fresh.iter().any(|window| is_global(&window.id))
        && windows
            .iter()
            .filter(|window| is_global(&window.id))
            .all(|window| fresh.iter().any(|current| current.id == window.id));
    for window in fresh {
        if window.remaining_percent == Some(0.0) {
            let after_reset = quota.resets.remove(&window.id).is_some();
            if window
                .reset_at
                .as_deref()
                .and_then(parse_timestamp)
                .is_some()
                && (after_reset
                    || quota
                        .zeros
                        .get(&window.id)
                        .is_none_or(|zero| zero.window.reset_at != window.reset_at))
            {
                quota.zeros.insert(
                    window.id.clone(),
                    WindowReading {
                        window: window.clone(),
                        observed_at: timestamp(observed),
                    },
                );
            }
            continue;
        }
        if let Some(zero) = quota.zeros.get(&window.id)
            && reset_observed(zero, window, observed)
        {
            quota.resets.insert(
                window.id.clone(),
                ConfirmedReset {
                    zero: zero.clone(),
                    began_at: timestamp(began),
                    observed_at: timestamp(observed),
                },
            );
        }
    }
    quota.refusals.retain(|r| {
        if r.kind != RefusalKind::Quota || !started.quota.refusals.iter().any(|old| old.id == r.id)
        {
            return true;
        }
        if poll
            && measured_global
            && global_positive
            && provider == SubscriptionProvider::Claude
            && r.scope == QuotaScope::Global
            && !r.explicit_delay_at(observed)
        {
            return false;
        }
        let reset = fresh.iter().any(|window| {
            quota.resets.get(&window.id).is_some_and(|proof| {
                parse_timestamp(&proof.observed_at) == Some(observed)
                    && reset_matches(provider, r, &proof.zero, global_positive)
            })
        });
        !reset
    });
}

fn global_positive<'a>(windows: impl Iterator<Item = &'a QuotaWindow>) -> bool {
    windows
        .filter(|window| is_global(&window.id))
        .all(|window| window.remaining_percent.is_some_and(|left| left > 0.0))
}

fn reset_observed(zero: &WindowReading, full: &QuotaWindow, observed: OffsetDateTime) -> bool {
    full.remaining_percent == Some(100.0)
        && full.reset_at.as_deref().and_then(parse_timestamp).is_some()
        && parse_timestamp(&zero.observed_at).is_some_and(|old| old < observed)
}

fn reset_matches(
    provider: SubscriptionProvider,
    refusal: &Refusal,
    zero: &WindowReading,
    global_positive: bool,
) -> bool {
    if !matches!(
        provider,
        SubscriptionProvider::Claude | SubscriptionProvider::Codex
    ) || refusal.kind != RefusalKind::Quota
        || refusal.scope == QuotaScope::Global && refusal.window.is_none() && !global_positive
    {
        return false;
    }
    let Some(reset) = zero.window.reset_at.as_deref().and_then(parse_timestamp) else {
        return false;
    };
    if let Some(explicit) = &refusal.retry_after_at
        && parse_timestamp(explicit) != Some(reset)
    {
        return false;
    }
    if let Some(window) = &refusal.window {
        return window == &zero.window.id
            && refusal.reset_at.as_deref().is_none_or(|at| {
                parse_timestamp(at).is_some_and(|at| at.unix_timestamp() == reset.unix_timestamp())
            });
    }
    match &refusal.scope {
        QuotaScope::Global if !is_global(&zero.window.id) => return false,
        QuotaScope::Global => {}
        scope if window_scope(&zero.window.id).as_ref() != Some(scope) => return false,
        _ => {}
    }
    let Some(retry) = parse_timestamp(refusal.reset_at.as_deref().unwrap_or(&refusal.retry_at))
    else {
        return false;
    };
    if !refusal.legacy {
        return reset.unix_timestamp() == retry.unix_timestamp();
    }
    // Old files derived absolute deadlines under a later lock. Keep the
    // migration tolerance and the Codex one-day cap only on those records.
    let capped = provider == SubscriptionProvider::Codex
        && refusal.provenance == Provenance::Inferred
        && parse_timestamp(&refusal.started_at)
            .is_some_and(|start| (retry - start) >= Duration::days(1));
    (reset - retry).abs() < Duration::seconds(2) || capped && reset > retry
}

/// A slow refusal body can finish after a newer quota response has already
/// proved the same allowance reset. A poll that began before the refusal
/// cannot provide this ordering guarantee.
pub(crate) fn resolved_before_recording(account: &AccountCredential, refusal: &Refusal) -> bool {
    let Some(received) = parse_timestamp(&refusal.started_at) else {
        return false;
    };
    let quota = &account.quota;
    let positive = global_positive(quota.measurements.values().map(|reading| &reading.window));
    quota.resets.values().any(|proof| {
        newer_than(&proof.began_at, received)
            && reset_matches(account.provider, refusal, &proof.zero, positive)
    })
}
