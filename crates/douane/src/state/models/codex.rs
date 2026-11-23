use super::{
    AccountCredential, AccountListing, AppState, BTreeMap, BTreeSet, Model, SubscriptionProvider,
    Value, catalog, json,
};

use crate::clients::Client;

impl AppState {
    /// Codex's native list carries newly released models and their current
    /// context/reasoning metadata, independently of the bundled fallback.
    pub(super) async fn codex_models(&self, stored: &AccountCredential) -> Option<Vec<Model>> {
        const VERSION: &str = "0.170.0";
        let account = self.oauth.credential(&stored.id).await.ok()?;
        let base = account
            .base_url
            .as_deref()
            .unwrap_or("https://chatgpt.com/backend-api/codex");
        let mut request = self
            .client
            .get(crate::relay::endpoint(base, "/models"))
            .query(&[("client_version", VERSION)])
            .bearer_auth(&account.access_token)
            .header("originator", "codex_cli_rs")
            .header("user-agent", format!("codex_cli_rs/{VERSION}"))
            .timeout(std::time::Duration::from_secs(15));
        if let Some(id) = &account.account_id {
            request = request.header("chatgpt-account-id", id);
        }
        let response = request.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let value: Value = response.json().await.ok()?;
        let rows = value.get("models")?.as_array()?;
        let mut models = Vec::new();
        for row in rows {
            let Some(id) = row
                .get("slug")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            else {
                continue;
            };
            // `supported_in_api` describes API-key access. This native listing
            // belongs to a ChatGPT subscription and may include models without it.
            let known = catalog()
                .iter()
                .find(|m| m.provider == SubscriptionProvider::Codex && m.id == id);
            let levels: Vec<&str> = row
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|level| level.get("effort").and_then(Value::as_str))
                .collect();
            models.push(Model {
                id: id.to_owned(),
                upstream_id: id.to_owned(),
                provider: SubscriptionProvider::Codex,
                name: row
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned(),
                context_length: row
                    .get("context_window")
                    .and_then(Value::as_u64)
                    .or_else(|| known.and_then(|m| m.context_length)),
                max_output_tokens: row
                    .get("max_output_tokens")
                    .and_then(Value::as_u64)
                    .or_else(|| known.and_then(|m| m.max_output_tokens)),
                thinking: if levels.is_empty() {
                    known.and_then(|m| m.thinking.clone())
                } else {
                    let mut thinking = json!({"levels":levels});
                    if let Some(default) =
                        row.get("default_reasoning_level").and_then(Value::as_str)
                        && levels.contains(&default)
                    {
                        thinking["default"] = json!(default);
                    }
                    Some(thinking)
                },
                metadata: Some(row.clone()),
                ..Model::default()
            });
        }
        Some(models)
    }

    pub(crate) async fn codex_catalog_contains(&self, client: &Client, id: &str) -> bool {
        let accounts = self
            .accounts
            .enabled_subscriptions(SubscriptionProvider::Codex)
            .await;
        let disabled = self.config.read().await.disabled_models.clone();
        let models = self.models.read().await;
        if !models
            .iter()
            .any(|model| model.upstream_id == id && codex_enabled(model, &disabled))
        {
            return false;
        }
        let listed = self.listed.lock().unwrap_or_else(|e| e.into_inner());
        self.codex_sources(&listed, codex_accounts(&accounts, Some(client)), id)
            .flatten()
            .any(Value::is_object)
    }

    pub(crate) async fn codex_catalog(&self, client: &Client) -> Option<Vec<Value>> {
        let accounts = self
            .accounts
            .enabled_subscriptions(SubscriptionProvider::Codex)
            .await;
        let disabled = self.config.read().await.disabled_models.clone();
        let models = self.models.read().await;
        let (entries, awaiting_first_read) =
            self.codex_entries(&models, &accounts, Some(client), &disabled);
        let mut catalog = Vec::new();
        for (id, entry) in entries {
            if entry.incomplete {
                return None;
            }
            let Some(mut row) = entry.native else {
                continue;
            };
            // A client key can route subscription-only models.
            row.insert("supported_in_api".into(), json!(true));
            row.remove("available_access_programs");
            row.remove("availability_nux");
            // Older clients use qualified slugs when resuming sessions.
            let mut alias = row.clone();
            alias.insert("slug".into(), json!(id));
            alias.insert("visibility".into(), json!("hide"));
            catalog.extend([Value::Object(row), Value::Object(alias)]);
        }
        if catalog.is_empty() && awaiting_first_read {
            return None;
        }
        Some(catalog)
    }

    pub(crate) fn codex_entries(
        &self,
        models: &[Model],
        accounts: &[AccountCredential],
        client: Option<&Client>,
        disabled: &BTreeSet<String>,
    ) -> (BTreeMap<String, CodexEntry>, bool) {
        let accounts = codex_accounts(accounts, client);
        let listed = self.listed.lock().unwrap_or_else(|e| e.into_inner());
        let awaiting_first_read = accounts.clone().any(|a| !listed.contains_key(&a.id));
        let entries = models
            .iter()
            .filter(|m| codex_enabled(m, disabled))
            .filter_map(|model| {
                let sources: Vec<_> = self
                    .codex_sources(&listed, accounts.clone(), &model.upstream_id)
                    .collect();
                let entry = CodexEntry::from_sources(&sources)?;
                Some((model.id.clone(), entry))
            })
            .collect();
        (entries, awaiting_first_read)
    }

    fn codex_sources<'a, 'b, I>(
        &'b self,
        listed: &'a BTreeMap<String, AccountListing>,
        accounts: I,
        id: &'b str,
    ) -> impl Iterator<Item = Option<&'a Value>> + use<'a, 'b, I>
    where
        I: Iterator<Item = &'b AccountCredential>,
    {
        accounts.filter_map(move |account| {
            if !account.offers_model(id) || self.refuses(&account.id, id) {
                return None;
            }
            match listed.get(&account.id) {
                Some(AccountListing::Codex(catalog)) => catalog.get(id).map(Some),
                // The account may serve the model, with unknown tiers.
                _ => Some(None),
            }
        })
    }
}

fn codex_accounts<'a>(
    accounts: &'a [AccountCredential],
    client: Option<&'a Client>,
) -> impl Iterator<Item = &'a AccountCredential> + Clone {
    accounts
        .iter()
        .filter(move |account| client.is_none_or(|client| client.access.allows(account)))
}

fn codex_enabled(model: &Model, disabled: &BTreeSet<String>) -> bool {
    model.provider == SubscriptionProvider::Codex
        && model.enabled
        && !disabled.contains(&model.id)
        && !disabled.contains(&model.upstream_id)
}

pub(crate) struct CodexEntry {
    pub native: Option<serde_json::Map<String, Value>>,
    incomplete: bool,
}

impl CodexEntry {
    fn from_sources(sources: &[Option<&Value>]) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        let first = sources.iter().flatten().find_map(|row| row.as_object());
        let incomplete = first.is_some() && sources.iter().any(Option::is_none);
        let native = first
            .filter(|_| !incomplete)
            .map(|row| intersect_tiers(row.clone(), sources));
        Some(Self { native, incomplete })
    }
}

pub(crate) fn tier_id(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| value["id"].as_str())
}

fn intersect_tiers(
    mut row: serde_json::Map<String, Value>,
    sources: &[Option<&Value>],
) -> serde_json::Map<String, Value> {
    for field in ["service_tiers", "additional_speed_tiers"] {
        if let Some(tiers) = row.get_mut(field).and_then(Value::as_array_mut) {
            tiers.retain(|tier| {
                tier_id(tier).is_some_and(|id| {
                    sources.iter().flatten().all(|source| {
                        source[field]
                            .as_array()
                            .is_some_and(|other| other.iter().any(|tier| tier_id(tier) == Some(id)))
                    })
                })
            });
        }
    }
    if row.get("default_service_tier").is_some_and(|default| {
        tier_id(default).is_none_or(|id| {
            !sources.iter().all(|source| {
                source
                    .and_then(|row| row.get("default_service_tier"))
                    .and_then(tier_id)
                    == Some(id)
            }) || id != "default"
                && !row
                    .get("service_tiers")
                    .and_then(Value::as_array)
                    .is_some_and(|tiers| tiers.iter().any(|tier| tier_id(tier) == Some(id)))
        })
    }) {
        row.remove("default_service_tier");
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_default_service_tier_requires_consensus() {
        let first =
            json!({"service_tiers":["default","priority"], "default_service_tier":"priority"});
        let mut second = json!({"service_tiers":["default","priority"], "default_service_tier":{"id":"priority"}});
        let merged = |second: &Value| {
            CodexEntry::from_sources(&[Some(&first), Some(second)])
                .unwrap()
                .native
                .unwrap()
        };
        assert_eq!(merged(&second)["default_service_tier"], "priority");
        second["default_service_tier"] = json!("default");
        assert!(!merged(&second).contains_key("default_service_tier"));
        second
            .as_object_mut()
            .unwrap()
            .remove("default_service_tier");
        assert!(!merged(&second).contains_key("default_service_tier"));
    }
}
