use super::{
    AccountCredential, AccountListing, AppState, BTreeSet, Model, SubscriptionProvider, Value,
    catalog, catalog_rank, json,
};
use crate::subscriptions::antigravity_user_agent;

impl AppState {
    /// The native rows visible to this key, optionally limited to the account
    /// that owns a signed Gemini turn.
    pub(crate) async fn antigravity_catalog_for(
        &self,
        client: &crate::clients::Client,
        owner: Option<&str>,
    ) -> Vec<Model> {
        let accounts: Vec<_> = self
            .accounts
            .enabled_subscriptions(SubscriptionProvider::Antigravity)
            .await
            .into_iter()
            .filter(|a| owner.is_none_or(|id| a.id == id) && client.access.allows(a))
            .collect();
        let disabled = self.config.read().await.disabled_models.clone();
        let models = self.models.read().await;
        let listed = self.listed.lock().unwrap_or_else(|e| e.into_inner());
        models
            .iter()
            .filter(|m| {
                m.provider == SubscriptionProvider::Antigravity
                    && m.enabled
                    && !disabled.contains(&m.id)
                    && !disabled.contains(&m.upstream_id)
            })
            .filter_map(|m| {
                accounts.iter().find_map(|a| {
                    if !a.offers_model(&m.upstream_id) || self.refuses(&a.id, &m.upstream_id) {
                        return None;
                    }
                    let AccountListing::Antigravity(rows) = listed.get(&a.id)? else {
                        return None;
                    };
                    let row = rows.get(&m.upstream_id)?;
                    Some(Model {
                        id: m.id.clone(),
                        upstream_id: m.upstream_id.clone(),
                        ..row.clone()
                    })
                })
            })
            .collect()
    }

    /// Google renames Antigravity models without notice, so the bundled ids
    /// go stale; the account's own `fetchAvailableModels` list is the truth.
    pub(super) async fn antigravity_models(
        &self,
        stored: &AccountCredential,
    ) -> Option<Vec<Model>> {
        let account = self.oauth.credential(&stored.id).await.ok()?;
        let project = account.project_id.as_deref()?;
        let base = account
            .base_url
            .as_deref()
            .unwrap_or("https://daily-cloudcode-pa.googleapis.com");
        let response = self
            .client
            .post(crate::relay::endpoint(
                base,
                "/v1internal:fetchAvailableModels",
            ))
            .bearer_auth(&account.access_token)
            .header("User-Agent", antigravity_user_agent())
            .json(&json!({ "project": project }))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let value = response.json::<Value>().await.ok()?;
        let models = antigravity_catalog(&value);
        (!models.is_empty()).then_some(models)
    }
}

/// Agent models from a `fetchAvailableModels` answer, in Antigravity's own
/// order. Tab-completion and internal models are skipped, and an alias that
/// repeats a display name already listed is dropped.
pub(in crate::state) fn antigravity_catalog(value: &Value) -> Vec<Model> {
    let Some(available) = value["models"].as_object() else {
        return vec![];
    };
    let hidden: BTreeSet<&str> = value["tabModelIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut order: Vec<&str> = value["agentModelSorts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sort| sort["groups"].as_array())
        .flatten()
        .filter_map(|group| group["modelIds"].as_array())
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut rest: Vec<&str> = available.keys().map(String::as_str).collect();
    rest.sort_by_key(|id| (catalog_rank(SubscriptionProvider::Antigravity, id), *id));
    order.extend(rest);
    let mut seen_ids = BTreeSet::new();
    let mut seen_names = BTreeSet::new();
    let mut models = vec![];
    for id in order {
        let Some(entry) = available.get(id) else {
            continue;
        };
        let Some(name) = entry["displayName"].as_str() else {
            continue;
        };
        if hidden.contains(id)
            || entry["isInternal"].as_bool() == Some(true)
            || !seen_ids.insert(id)
            || !seen_names.insert(name)
        {
            continue;
        }
        let bundled = catalog()
            .iter()
            .find(|m| m.provider == SubscriptionProvider::Antigravity && m.id == id);
        let thinking = bundled.and_then(|m| m.thinking.clone()).or_else(|| {
            (entry["supportsThinking"].as_bool() == Some(true)).then(|| {
                json!({
                    "min": entry["minThinkingBudget"].as_u64().unwrap_or(1),
                    "dynamic_allowed": entry["thinkingBudget"].as_i64() == Some(-1),
                })
            })
        });
        models.push(Model {
            id: id.into(),
            provider: SubscriptionProvider::Antigravity,
            name: name.into(),
            context_length: entry["maxTokens"]
                .as_u64()
                .or_else(|| bundled.and_then(|m| m.context_length)),
            max_output_tokens: entry["maxOutputTokens"]
                .as_u64()
                .or_else(|| bundled.and_then(|m| m.max_output_tokens)),
            enabled: true,
            upstream_id: String::new(),
            thinking,
            ..Model::default()
        });
    }
    models
}
