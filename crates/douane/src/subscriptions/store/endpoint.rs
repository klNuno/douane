use super::*;

/// A route names a custom endpoint in model ids (`deepseek/deepseek-chat`):
/// 1 to 32 lowercase letters, digits, `-` or `_`, never a provider's name,
/// which would make `<route>/<model>` ambiguous.
pub fn valid_route(route: &str) -> bool {
    route.len() <= 32
        && route
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && route
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && SubscriptionProvider::parse(route).is_none()
}

/// The most images a custom endpoint's `image_limit` can allow.
pub const MAX_IMAGE_LIMIT: u32 = 1000;

/// An image limit from 0 (no image) to [`MAX_IMAGE_LIMIT`].
pub fn valid_image_limit(limit: u32) -> bool {
    limit <= MAX_IMAGE_LIMIT
}

/// Wire formats a custom endpoint can speak.
pub fn valid_dialect(dialect: &str) -> bool {
    matches!(dialect, "chat" | "responses" | "messages")
}

/// An upstream address: http or https, without credentials or a query.
pub fn valid_base_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
    })
}

/// Fields a custom endpoint can change in place; `None` keeps the value.
#[derive(Default)]
pub struct EndpointPatch {
    pub route: Option<String>,
    pub base_url: Option<String>,
    /// `Some(None)` returns to forwarding the client's own format.
    pub dialect: Option<Option<String>>,
    /// `Some(None)` returns to the default image limit.
    pub image_limit: Option<Option<u32>>,
    /// Replaces the request metadata entries; an empty map removes them.
    pub metadata: Option<BTreeMap<String, String>>,
    /// An empty list serves every model the endpoint lists.
    pub models: Option<Vec<String>>,
    pub api_key: Option<String>,
}

pub(super) const ROUTE_RULE: &str = "route must be 1 to 32 lowercase letters, digits, - or _, and not a provider name such as claude, codex, openai or copilot";
pub(super) const DIALECT_RULE: &str = "dialect must be chat, responses or messages";
pub(super) const IMAGE_LIMIT_RULE: &str = "image_limit must be a whole number from 0 to 1000";
pub(super) const BASE_URL_RULE: &str =
    "base_url must be an http or https address without credentials or query";
impl AccountStore {
    /// Changes a custom endpoint in place. Its key stays in the account file
    /// and never comes back in the summary.
    pub async fn update_endpoint(
        &self,
        id: &str,
        patch: EndpointPatch,
    ) -> Result<AccountSummary, SubscriptionError> {
        let route = patch.route.map(|r| r.trim().to_ascii_lowercase());
        if route.as_deref().is_some_and(|r| !valid_route(r)) {
            return Err(SubscriptionError::InvalidEndpoint(ROUTE_RULE));
        }
        let base_url = patch.base_url.map(|url| url.trim().to_owned());
        if base_url.as_deref().is_some_and(|url| !valid_base_url(url)) {
            return Err(SubscriptionError::InvalidEndpoint(BASE_URL_RULE));
        }
        let dialect = patch
            .dialect
            .map(|d| d.map(|d| d.trim().to_ascii_lowercase()));
        if dialect
            .as_ref()
            .and_then(Option::as_deref)
            .is_some_and(|d| !valid_dialect(d))
        {
            return Err(SubscriptionError::InvalidEndpoint(DIALECT_RULE));
        }
        if patch
            .image_limit
            .flatten()
            .is_some_and(|limit| !valid_image_limit(limit))
        {
            return Err(SubscriptionError::InvalidEndpoint(IMAGE_LIMIT_RULE));
        }
        if patch
            .metadata
            .as_ref()
            .is_some_and(|metadata| !crate::endpoint_metadata::valid(metadata))
        {
            return Err(SubscriptionError::InvalidEndpoint(
                crate::endpoint_metadata::RULE,
            ));
        }
        let api_key = patch.api_key.map(|key| key.trim().to_owned());
        if api_key.as_deref().is_some_and(|key| {
            key.is_empty() || key.len() > 8192 || key.chars().any(char::is_control)
        }) {
            return Err(SubscriptionError::InvalidEndpoint(
                "api_key must be 1 to 8192 characters without control characters",
            ));
        }
        let models = patch.models.map(|models| {
            models
                .into_iter()
                .map(|m| m.trim().to_owned())
                .filter(|m| !m.is_empty())
                .collect::<Vec<_>>()
        });
        if models
            .as_ref()
            .is_some_and(|models| models.len() > 500 || models.iter().any(|m| m.len() > 200))
        {
            return Err(SubscriptionError::InvalidEndpoint(
                "models takes at most 500 ids of 200 characters",
            ));
        }
        let mut accounts = self.accounts.write().await;
        let current = accounts.get(id).ok_or(SubscriptionError::NotFound)?;
        if current.provider != SubscriptionProvider::OpenaiCompatible {
            return Err(SubscriptionError::InvalidEndpoint(
                "only openai_compatible endpoints take a route, base_url, dialect, image_limit, metadata, models or api_key",
            ));
        }
        let mut account = current.clone();
        if let Some(route) = route {
            account.route = Some(route);
        }
        if let Some(url) = base_url {
            account.base_url = Some(url);
        }
        if let Some(dialect) = dialect {
            account.dialect = dialect;
        }
        if let Some(limit) = patch.image_limit {
            account.image_limit = limit;
        }
        if let Some(metadata) = patch.metadata {
            account.metadata = metadata;
        }
        if let Some(models) = models {
            account.models = models;
        }
        if let Some(key) = api_key {
            account.access_token = key;
        }
        if account.access_token != current.access_token || account.base_url != current.base_url {
            account.endpoint_revision = rand::random();
        }
        if account.endpoint_revision != current.endpoint_revision
            || account.metadata != current.metadata
        {
            account.budget_revision = rand::random();
            account.quota.refusals.clear();
            account.quota.set_error(None);
            account.project_quota();
        }
        if account.route.is_some() && account.base_url.is_none() {
            return Err(SubscriptionError::InvalidEndpoint(
                "an endpoint with a route needs its base_url",
            ));
        }
        account.updated_at = now();
        self.persist(&account)?;
        let summary = account.summary();
        accounts.insert(account.id.clone(), account);
        Ok(summary)
    }
}
