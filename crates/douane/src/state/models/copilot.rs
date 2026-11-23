use super::{AccountCredential, AppState, Model, SubscriptionProvider, Value, json};

impl AppState {
    /// Copilot's own catalog: the models this account's plan enables, with
    /// the upstream paths each one accepts.
    pub(super) async fn copilot_models(&self, account: &AccountCredential) -> Option<Vec<Model>> {
        let base = account
            .base_url
            .as_deref()
            .unwrap_or(crate::relay::COPILOT_API);
        let response = self
            .client
            .get(crate::relay::endpoint(base, "/models"))
            .bearer_auth(&account.access_token)
            .header("User-Agent", crate::relay::COPILOT_USER_AGENT)
            .header("X-GitHub-Api-Version", crate::relay::COPILOT_API_VERSION)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let value = response.json::<Value>().await.ok()?;
        let models = copilot_catalog(&value);
        (!models.is_empty()).then_some(models)
    }
}

/// Paths of Copilot's API that douane can forward to.
const COPILOT_PATHS: [&str; 3] = ["/v1/messages", "/responses", "/chat/completions"];

/// The chat models a Copilot account's plan enables, from its `/models`
/// answer. A model whose policy is not `enabled` is refused upstream, and
/// one without `supported_endpoints` speaks Chat Completions only.
pub(crate) fn copilot_catalog(value: &Value) -> Vec<Model> {
    let Some(rows) = value["data"].as_array() else {
        return vec![];
    };
    let mut models: Vec<Model> = vec![];
    for row in rows {
        let Some(id) = row["id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        if row.pointer("/policy/state").and_then(Value::as_str) != Some("enabled")
            || row.pointer("/capabilities/type").and_then(Value::as_str) != Some("chat")
            || models.iter().any(|m| m.id == id)
        {
            continue;
        }
        let endpoints: Vec<String> = match row["supported_endpoints"].as_array() {
            Some(paths) => paths
                .iter()
                .filter_map(Value::as_str)
                .filter(|path| COPILOT_PATHS.contains(path))
                .map(str::to_owned)
                .collect(),
            None => vec!["/chat/completions".into()],
        };
        if endpoints.is_empty() {
            continue;
        }
        let limits = row.pointer("/capabilities/limits");
        let levels: Vec<Value> = row
            .pointer("/capabilities/supports/reasoning_effort")
            .and_then(Value::as_array)
            .map(|levels| levels.iter().filter(|l| l.is_string()).cloned().collect())
            .unwrap_or_default();
        models.push(Model {
            id: id.into(),
            provider: SubscriptionProvider::Copilot,
            name: row["name"].as_str().unwrap_or(id).into(),
            context_length: limits.and_then(|l| {
                l["max_context_window_tokens"]
                    .as_u64()
                    .or_else(|| l["max_prompt_tokens"].as_u64())
            }),
            max_output_tokens: limits.and_then(|l| l["max_output_tokens"].as_u64()),
            enabled: true,
            thinking: (!levels.is_empty()).then(|| json!({ "levels": levels })),
            endpoints,
            ..Model::default()
        });
    }
    models
}
