use super::*;
pub(crate) struct CallGuard {
    pub call: Call,
    pub complete: bool,
    start: Instant,
    ledger: Arc<Ledger>,
    uncached_input: Option<u64>,
    clients: Option<Arc<crate::clients::Clients>>,
}
impl CallGuard {
    pub fn new(ledger: Arc<Ledger>, account: &AccountCredential, model: &str) -> Self {
        ledger.in_flight.fetch_add(1, Ordering::Relaxed);
        Self {
            call: Call {
                id: format!("{:032x}", rand::random::<u128>()),
                started_at: crate::subscriptions::now(),
                provider: account.pool().into(),
                account_id: account.id.clone(),
                model: model.into(),
                status: 200,
                ..Call::default()
            },
            complete: false,
            start: Instant::now(),
            ledger,
            uncached_input: None,
            clients: None,
        }
    }
    /// Records the call under its client and counts it in the client usage
    /// of its account.
    pub fn for_client(
        mut self,
        clients: &Arc<crate::clients::Clients>,
        client: Option<&crate::clients::Client>,
    ) -> Self {
        self.call.client_id = client.map(|c| c.id.clone());
        clients.call_started_on(
            &self.call.account_id,
            self.call.client_id.as_deref(),
            &self.call.model,
        );
        self.clients = Some(clients.clone());
        self
    }
    pub fn fail(&mut self, error: &str) {
        self.call.error = Some(error.to_owned());
        self.complete = true;
    }
    pub fn observe(&mut self, value: &Value) {
        let usage = value
            .get("usage")
            .or_else(|| value.pointer("/message/usage"))
            .or_else(|| value.pointer("/response/usage"))
            .or_else(|| value.get("usageMetadata"))
            .or_else(|| value.pointer("/response/usageMetadata"));
        if let Some(u) = usage {
            self.call.input_tokens = u["input_tokens"]
                .as_u64()
                .or_else(|| u["prompt_tokens"].as_u64())
                .or_else(|| u["promptTokenCount"].as_u64())
                .unwrap_or(self.call.input_tokens);
            self.call.output_tokens = u["output_tokens"]
                .as_u64()
                .or_else(|| u["completion_tokens"].as_u64())
                .or_else(|| u["candidatesTokenCount"].as_u64())
                .unwrap_or(self.call.output_tokens);
            self.call.cache_read_tokens = u["cache_read_input_tokens"]
                .as_u64()
                .or_else(|| {
                    u.pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                })
                .or_else(|| {
                    u.pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                })
                .or_else(|| u["cachedContentTokenCount"].as_u64())
                .unwrap_or(self.call.cache_read_tokens);
            self.call.cache_write_tokens = u["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(self.call.cache_write_tokens);
            // Anthropic input_tokens excludes cache reads/writes; other dialects include them.
            if u.get("cache_read_input_tokens").is_some()
                || u.get("cache_creation_input_tokens").is_some()
            {
                self.uncached_input = u["input_tokens"].as_u64().or(self.uncached_input);
            }
            if let Some(uncached) = self.uncached_input {
                self.call.input_tokens =
                    uncached + self.call.cache_read_tokens + self.call.cache_write_tokens;
            }
        }
        if matches!(
            value["type"].as_str(),
            Some(
                "message_stop"
                    | "response.completed"
                    | "response.incomplete"
                    | "__douane_stream_done"
            )
        ) {
            self.complete = true;
        }
        if value["type"] == "error"
            || value["type"] == "response.failed"
            || value.get("error").is_some_and(|v| !v.is_null())
        {
            self.fail("upstream reported an error in its stream");
        }
    }
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.ledger.in_flight.fetch_sub(1, Ordering::Relaxed);
        if !self.complete {
            self.call.error =
                Some("client disconnected or upstream response was interrupted".into());
        }
        self.call.latency_ms = self.start.elapsed().as_millis() as u64;
        if let Some(clients) = &self.clients {
            clients.call_ended_on(
                &self.call.account_id,
                self.call.client_id.as_deref(),
                crate::clients::call_weight(&self.call),
                &self.call.model,
            );
        }
        self.ledger.record(std::mem::take(&mut self.call));
    }
}
