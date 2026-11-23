use super::{
    AccountCredential, AppState, BTreeSet, Discovery, Model, SubscriptionProvider, Value, json,
};

impl AppState {
    pub(super) async fn compatible_models(&self, account: &AccountCredential) -> Discovery {
        use SubscriptionProvider::*;
        let base = account
            .base_url
            .as_deref()
            .unwrap_or(match account.provider {
                Meta => crate::subscriptions::meta::API,
                OpencodeGo => "https://opencode.ai/zen/go/v1",
                Qwen => "https://portal.qwen.ai/v1",
                _ => "",
            });
        if base.is_empty() {
            return Discovery::Bundled;
        }
        let Some(listing) = self.read_compatible_models(account, base).await else {
            return Discovery::CompatibleUnavailable;
        };
        let Some(rows) = listing["data"].as_array() else {
            return Discovery::CompatibleUnavailable;
        };
        let mut available: Vec<_> = rows
            .iter()
            .filter_map(|row| compatible_model(account.provider, row))
            .collect();
        // Only a Chat endpoint carries reasoning_effort.
        if account.provider == OpenaiCompatible && account.dialect.as_deref() == Some("chat") {
            available.retain(|model| account.offers_model(&model.id));
            let probe: BTreeSet<_> = rows
                .iter()
                .filter(|row| crate::openai_reasoning::needs_probe(row))
                .filter_map(|row| row["id"].as_str())
                .collect();
            self.reasoning_levels_for(account, base, &probe, &mut available)
                .await;
        }
        Discovery::Live(available)
    }

    async fn read_compatible_models(
        &self,
        account: &AccountCredential,
        base: &str,
    ) -> Option<Value> {
        let root = base.trim_end_matches('/');
        let (url, headers) = if account.provider == SubscriptionProvider::Meta {
            let credential = self.oauth.credential(&account.id).await.ok();
            let url = format!(
                "{}/muse-code/models",
                root.strip_suffix("/v1").unwrap_or(root)
            );
            let headers = credential.as_ref().map(listing_headers).unwrap_or_default();
            (url, headers)
        } else {
            (
                crate::relay::endpoint(base, "/v1/models"),
                listing_headers(account),
            )
        };
        self.client
            .get(url)
            .headers(headers)
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()
    }

    /// Fills the effort levels of `available` rows named in `probe`, asking
    /// the endpoint only for the models this account has not answered yet.
    async fn reasoning_levels_for(
        &self,
        account: &AccountCredential,
        base: &str,
        probe: &BTreeSet<&str>,
        available: &mut [Model],
    ) {
        use futures_util::StreamExt;
        let known = |model: &str| {
            self.reasoning_levels
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(account.id.clone(), model.to_owned()))
                .cloned()
        };
        let missing: Vec<String> = available
            .iter()
            .filter(|m| probe.contains(m.id.as_str()) && known(&m.id).is_none())
            .map(|m| m.id.clone())
            .collect();
        if !missing.is_empty() {
            let url = crate::relay::endpoint(base, "/v1/chat/completions");
            let headers = crate::relay::upstream_headers(account, &axum::http::HeaderMap::new());
            let answers: Vec<(String, crate::openai_reasoning::Probe)> =
                futures_util::stream::iter(missing)
                    .map(|model| {
                        let (url, headers) = (url.clone(), headers.clone());
                        async move {
                            let answer =
                                crate::openai_reasoning::probe(&self.client, &url, headers, &model)
                                    .await;
                            (model, answer)
                        }
                    })
                    .buffer_unordered(crate::openai_reasoning::PROBE_CONCURRENCY)
                    .collect()
                    .await;
            let mut cache = self
                .reasoning_levels
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for (model, answer) in answers {
                let levels = match answer {
                    crate::openai_reasoning::Probe::Levels(levels) => levels,
                    crate::openai_reasoning::Probe::Silent => vec![],
                    crate::openai_reasoning::Probe::Retry => continue,
                };
                cache.insert((account.id.clone(), model), levels);
            }
        }
        for model in available
            .iter_mut()
            .filter(|m| probe.contains(m.id.as_str()))
        {
            if let Some(levels) = known(&model.id).filter(|levels| !levels.is_empty()) {
                model.thinking = Some(json!({ "levels": levels }));
            }
        }
    }
}

fn listing_headers(account: &AccountCredential) -> axum::http::HeaderMap {
    crate::relay::upstream_headers(account, &axum::http::HeaderMap::new())
}

fn compatible_model(provider: SubscriptionProvider, row: &Value) -> Option<Model> {
    let id = row["id"].as_str()?;
    let native = (provider == SubscriptionProvider::Meta)
        .then(|| &row["metadata"]["muse-code"])
        .filter(|v| v.is_object());
    Some(Model {
        id: id.into(),
        name: row["name"].as_str().unwrap_or(id).into(),
        provider,
        context_length: row["context_length"]
            .as_u64()
            .or_else(|| row["context_window"].as_u64())
            .or_else(|| row["max_context_length"].as_u64())
            .or_else(|| native.and_then(|v| v["limit"]["context"].as_u64())),
        max_output_tokens: native.and_then(|v| v["limit"]["output"].as_u64()),
        thinking: row.get("thinking").cloned(),
        metadata: native.map(|v| json!({"muse-code":v})),
        ..Model::default()
    })
}
