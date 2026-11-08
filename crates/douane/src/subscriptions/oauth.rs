use super::google::{GoogleOAuth, GoogleSignIn, OAuthClientError, OAuthClientView};
use super::store::{apply_identity, prepare_import, text};
use super::{
    AccountCredential, AccountStore, AccountSummary, SubscriptionError, SubscriptionProvider,
    parse_timestamp, random_id, timestamp,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CLAUDE_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const XAI_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_DISCOVERY: &str = "https://auth.x.ai/.well-known/openid-configuration";
/// The OpenCode GitHub app, which GitHub admits to the Copilot API.
const COPILOT_CLIENT: &str = "Ov23li8tweQw6odWQebz";

#[derive(Clone, Debug, Serialize)]
pub struct OAuthAttempt {
    pub id: String,
    pub provider: SubscriptionProvider,
    pub url: String,
    pub state: String,
    pub redirect_uri: String,
    pub expires_at: String,
    pub status: String,
    pub account_id: Option<String>,
    pub error: Option<String>,
    pub error_code: Option<&'static str>,
    pub user_code: Option<String>,
    pub interval: Option<u64>,
    pub verification_required: bool,
    pub verification_available: bool,
}

struct Pending {
    public: OAuthAttempt,
    verifier: String,
    device: Option<DeviceFlow>,
    verification_url: Option<String>,
    /// The Google client and project a sign-in started with; its code
    /// exchange uses them.
    google_client: Option<GoogleSignIn>,
}

#[derive(Clone)]
struct DeviceFlow {
    provider: SubscriptionProvider,
    code: String,
    token_endpoint: String,
    next_poll: time::OffsetDateTime,
    interval: u64,
}

pub struct OAuthManager {
    client: reqwest::Client,
    meta_client: reqwest::Client,
    /// Mint replies include quotas before the first inference. Keep only that
    /// observation until QuotaService consumes it, never the minted key/body.
    meta_usage: Mutex<BTreeMap<String, (time::OffsetDateTime, Value)>>,
    store: Arc<AccountStore>,
    pending: Mutex<BTreeMap<String, Pending>>,
    codex_token_url: String,
    claude_token_url: String,
    google: GoogleOAuth,
    /// Refresh tokens a provider has revoked, by account. Asking again gets the
    /// same answer and only looks like abuse on the account; a reconnect
    /// stores a new token, which is asked normally.
    revoked: std::sync::Mutex<BTreeMap<String, String>>,
    /// GitHub's device flow and identity endpoints; tests use a loopback.
    pub(crate) copilot_device_url: String,
    pub(crate) copilot_token_url: String,
    pub(crate) github_user_url: String,
    pub(crate) meta_device_url: String,
    pub(crate) meta_token_url: String,
    pub(crate) meta_mint_url: String,
}

impl OAuthManager {
    pub fn new(client: reqwest::Client, store: Arc<AccountStore>) -> Self {
        let google = GoogleOAuth::load(client.clone(), &store.oauth_clients_path());
        Self {
            meta_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("muse-code/1.0.2")
                .build()
                .expect("Meta HTTP client"),
            meta_usage: Mutex::new(BTreeMap::new()),
            client,
            store,
            pending: Mutex::new(BTreeMap::new()),
            codex_token_url: "https://auth.openai.com/oauth/token".into(),
            claude_token_url: "https://platform.claude.com/v1/oauth/token".into(),
            google,
            revoked: std::sync::Mutex::new(BTreeMap::new()),
            copilot_device_url: "https://github.com/login/device/code".into(),
            copilot_token_url: "https://github.com/login/oauth/access_token".into(),
            github_user_url: "https://api.github.com/user".into(),
            meta_device_url: "https://auth.meta.com/oidc/device/authorization/".into(),
            meta_token_url: "https://auth.meta.com/oidc/device/token/".into(),
            meta_mint_url: "https://api.meta.ai/muse-code/key".into(),
        }
    }

    /// The Google OAuth clients and where each comes from; never a secret.
    /// `oauth-clients.json` is read again, so hand edits show and apply.
    pub fn oauth_clients(&self) -> Vec<OAuthClientView> {
        self.google.reload_views()
    }

    /// Sets a custom Google OAuth client, effective at once.
    pub fn set_oauth_client(
        &self,
        provider: SubscriptionProvider,
        body: &Value,
    ) -> Result<OAuthClientView, OAuthClientError> {
        self.google.set_client(provider, body)
    }

    /// Removes a custom Google OAuth client, effective at once.
    pub fn remove_oauth_client(
        &self,
        provider: SubscriptionProvider,
    ) -> Result<OAuthClientView, OAuthClientError> {
        self.google.remove_client(provider)
    }

    /// Finds and validates the Antigravity client secret, from an installed
    /// Antigravity CLI or a secret douanectl detected elsewhere. Nothing is
    /// saved: pass the result to `save_antigravity_client`.
    pub async fn detect_antigravity_client(
        &self,
        request: super::DetectRequest,
    ) -> Result<super::DetectedClient, super::DetectError> {
        self.google.detect_antigravity(request).await
    }

    /// Saves a detected Antigravity client, effective at once; the Cloud
    /// project already set is kept. Blocking file IO: run it on the blocking
    /// pool.
    pub fn save_antigravity_client(
        &self,
        detected: super::DetectedClient,
        force: bool,
    ) -> Result<OAuthClientView, super::DetectError> {
        self.google.save_detected(detected, force)
    }

    /// Imports a credential file. A Google account without a Cloud project
    /// gets one from Code Assist, as sign-in does. Finding it is best effort:
    /// when it fails for any reason the account is stored anyway without a
    /// project, as before discovery existed, with the reason in `last_error`,
    /// and tokens refreshed meanwhile are kept.
    pub async fn import(
        &self,
        value: Value,
        name: Option<String>,
    ) -> Result<AccountSummary, SubscriptionError> {
        let (mut account, explicit_name) = prepare_import(value, name)?;
        if matches!(
            account.provider,
            SubscriptionProvider::Gemini | SubscriptionProvider::Antigravity
        ) && account.project_id.is_none()
            && let Err(error) = self.discover_project(&mut account).await
        {
            account.last_error = Some(discovery_failure(&error));
        }
        self.store.import_account(account, explicit_name).await
    }

    async fn discover_project(
        &self,
        account: &mut AccountCredential,
    ) -> Result<(), SubscriptionError> {
        if let Some(project) = self.store.known_project(account).await {
            account.project_id = Some(project);
            return Ok(());
        }
        // A token without a known expiry may be long dead: refresh it first
        // when possible. A token known to be dead without a way to refresh it
        // fails here, before any Google call.
        let stale = account.needs_refresh() || account.expires_at.is_none();
        match account.refresh_token.clone() {
            Some(refresh_token) if stale => {
                let request = match self.google.refresh_request(
                    account.provider,
                    &refresh_token,
                    account.client_id.as_deref(),
                    account.client_secret.as_deref(),
                ) {
                    Ok(request) => request,
                    Err(error) => return self.discover_without_client(account, error).await,
                };
                let response = send_refresh(request).await?;
                // Keep a rotated refresh token first: the old one may be dead.
                if let Some(rotated) = text(&response, &["refresh_token"]) {
                    account.refresh_token = Some(rotated);
                }
                account.access_token =
                    text(&response, &["access_token"]).ok_or(SubscriptionError::InvalidResponse)?;
                if let Some(token) = text(&response, &["id_token"]) {
                    account.id_token = Some(token);
                }
                account.expires_at = expiry_from_response(&response);
            }
            None if account.needs_refresh() => {
                return Err(SubscriptionError::InvalidImport(
                    "the access token has expired and there is no refresh token; sign in again",
                ));
            }
            _ => {}
        }
        self.onboard_import(account).await
    }

    /// Discovery when no client douane has can refresh the imported tokens.
    /// With no client at all for them (none configured, or only the
    /// provider's public id without its secret), the import goes on as it
    /// did before discovery: a token that has not expired still looks its
    /// project up, an expired one is stored without a project. A file that
    /// names a custom client douane does not have fails the lookup.
    async fn discover_without_client(
        &self,
        account: &mut AccountCredential,
        error: SubscriptionError,
    ) -> Result<(), SubscriptionError> {
        let custom = account
            .client_id
            .as_deref()
            .is_some_and(|id| !super::google::is_public_client(account.provider, id));
        match error {
            SubscriptionError::OAuthClientUnavailable if custom => {
                Err(SubscriptionError::InvalidImport(
                    "the file names a Google OAuth client douane does not have; set that client in douane first, or add its client_secret to the file",
                ))
            }
            SubscriptionError::OAuthNotConfigured
            | SubscriptionError::OAuthClientUnavailable
            | SubscriptionError::OAuthClientNotSetUp => {
                if account.needs_refresh() {
                    Ok(())
                } else {
                    self.onboard_import(account).await
                }
            }
            error => Err(error),
        }
    }

    /// Looks up and onboards the Cloud project of an imported account.
    /// `import` stores the account even when this fails.
    async fn onboard_import(
        &self,
        account: &mut AccountCredential,
    ) -> Result<(), SubscriptionError> {
        let configured = self.google.configured_project(account.provider);
        let (project, tier) = self
            .google
            .onboard(
                account.provider,
                &account.access_token,
                configured.as_deref(),
            )
            .await?;
        account.project_id = Some(project);
        if account.plan.is_none() {
            account.plan = tier;
        }
        Ok(())
    }

    pub fn supports(&self, provider: SubscriptionProvider) -> bool {
        match provider {
            SubscriptionProvider::Codex
            | SubscriptionProvider::Claude
            | SubscriptionProvider::Xai
            | SubscriptionProvider::Copilot
            | SubscriptionProvider::Meta => true,
            SubscriptionProvider::Gemini | SubscriptionProvider::Antigravity => {
                self.google.supports(provider)
            }
            _ => false,
        }
    }

    pub async fn start(
        &self,
        provider: SubscriptionProvider,
        redirect_uri: Option<String>,
    ) -> Result<OAuthAttempt, SubscriptionError> {
        if provider == SubscriptionProvider::Meta {
            return self.start_meta().await;
        }
        if provider == SubscriptionProvider::Xai {
            return self.start_xai().await;
        }
        if provider == SubscriptionProvider::Copilot {
            return self.start_copilot().await;
        }
        if matches!(
            provider,
            SubscriptionProvider::Gemini | SubscriptionProvider::Antigravity
        ) {
            return self.start_google(provider, redirect_uri).await;
        }
        let (auth_url, client_id, default_redirect, scope) = match provider {
            SubscriptionProvider::Codex => (
                "https://auth.openai.com/oauth/authorize",
                CODEX_CLIENT,
                "http://localhost:1455/auth/callback",
                "openid email profile offline_access",
            ),
            SubscriptionProvider::Claude => (
                "https://claude.ai/oauth/authorize",
                CLAUDE_CLIENT,
                "http://localhost:54545/callback",
                CLAUDE_SCOPE,
            ),
            _ => return Err(SubscriptionError::Unsupported),
        };
        let redirect = redirect_uri.unwrap_or_else(|| default_redirect.into());
        validate_redirect(&redirect)?;
        let state = random_id();
        let verifier = format!("{}{}", random_id(), random_id());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url =
            reqwest::Url::parse(auth_url).map_err(|_| SubscriptionError::InvalidResponse)?;
        url.query_pairs_mut().extend_pairs([
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("scope", scope),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ]);
        match provider {
            SubscriptionProvider::Codex => {
                url.query_pairs_mut().extend_pairs([
                    ("prompt", "login"),
                    ("id_token_add_organizations", "true"),
                    ("codex_cli_simplified_flow", "true"),
                ]);
            }
            SubscriptionProvider::Claude => {
                url.query_pairs_mut().append_pair("code", "true");
            }
            _ => {}
        }
        let attempt = OAuthAttempt {
            id: random_id(),
            provider,
            url: url.into(),
            state,
            redirect_uri: redirect,
            expires_at: timestamp(time::OffsetDateTime::now_utc() + time::Duration::minutes(15)),
            status: "pending".into(),
            account_id: None,
            error: None,
            error_code: None,
            user_code: None,
            interval: None,
            verification_required: false,
            verification_available: false,
        };
        self.save_attempt(Pending {
            public: attempt.clone(),
            verifier,
            device: None,
            verification_url: None,
            google_client: None,
        })
        .await;
        Ok(attempt)
    }

    async fn start_google(
        &self,
        provider: SubscriptionProvider,
        redirect_uri: Option<String>,
    ) -> Result<OAuthAttempt, SubscriptionError> {
        let default_redirect = match provider {
            SubscriptionProvider::Gemini => "https://codeassist.google.com/authcode",
            SubscriptionProvider::Antigravity => "http://localhost:51121/oauth-callback",
            _ => return Err(SubscriptionError::Unsupported),
        };
        let redirect = redirect_uri.unwrap_or_else(|| default_redirect.into());
        if provider != SubscriptionProvider::Gemini
            || redirect != "https://codeassist.google.com/authcode"
        {
            validate_redirect(&redirect)?;
        }
        let state = random_id();
        let verifier = format!("{}{}", random_id(), random_id());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let (url, sign_in) = self
            .google
            .authorize(provider, &state, &redirect, &challenge)?;
        let attempt = OAuthAttempt {
            id: random_id(),
            provider,
            url,
            state,
            redirect_uri: redirect,
            expires_at: timestamp(time::OffsetDateTime::now_utc() + time::Duration::minutes(15)),
            status: "pending".into(),
            account_id: None,
            error: None,
            error_code: None,
            user_code: None,
            interval: None,
            verification_required: false,
            verification_available: false,
        };
        self.save_attempt(Pending {
            public: attempt.clone(),
            verifier,
            device: None,
            verification_url: None,
            google_client: Some(sign_in),
        })
        .await;
        Ok(attempt)
    }

    async fn save_attempt(&self, pending: Pending) {
        let mut attempts = self.pending.lock().await;
        let now = time::OffsetDateTime::now_utc();
        attempts.retain(|_, attempt| {
            parse_timestamp(&attempt.public.expires_at)
                .is_some_and(|t| t + time::Duration::hours(1) > now)
        });
        if attempts.len() >= 64 {
            if let Some(oldest) = attempts
                .iter()
                .min_by_key(|(_, p)| &p.public.expires_at)
                .map(|(id, _)| id.clone())
            {
                attempts.remove(&oldest);
            }
        }
        attempts.insert(pending.public.id.clone(), pending);
    }

    pub async fn status(&self, id: &str) -> Option<OAuthAttempt> {
        self.poll_device(id).await;
        let mut attempts = self.pending.lock().await;
        let attempt = attempts.get_mut(id)?;
        if parse_timestamp(&attempt.public.expires_at)
            .is_none_or(|t| t <= time::OffsetDateTime::now_utc())
        {
            if attempt.public.status == "pending" {
                attempt.public.status = "expired".into();
                attempt.device = None;
                attempt.verifier.clear();
            }
            attempt.verification_url = None;
            attempt.public.verification_available = false;
        }
        Some(attempt.public.clone())
    }

    pub async fn verification_url(&self, id: &str) -> Option<String> {
        let mut attempts = self.pending.lock().await;
        let attempt = attempts.get_mut(id)?;
        if parse_timestamp(&attempt.public.expires_at)
            .is_none_or(|expires| expires <= time::OffsetDateTime::now_utc())
        {
            attempt.verification_url = None;
            attempt.public.verification_available = false;
            return None;
        }
        if attempt.public.status != "error" || !attempt.public.verification_required {
            return None;
        }
        attempt.verification_url.clone()
    }

    pub async fn complete(
        &self,
        id: &str,
        code: &str,
        state: &str,
    ) -> Result<AccountSummary, SubscriptionError> {
        let (code, state) = callback_fields(code, state)?;
        let (provider, redirect, verifier, google_client) = {
            let mut attempts = self.pending.lock().await;
            let pending = attempts.get_mut(id).ok_or(SubscriptionError::Expired)?;
            if pending.public.status != "pending"
                || pending.device.is_some()
                || parse_timestamp(&pending.public.expires_at)
                    .is_none_or(|t| t <= time::OffsetDateTime::now_utc())
            {
                return Err(SubscriptionError::Expired);
            }
            if state != pending.public.state {
                return Err(SubscriptionError::InvalidState);
            }
            pending.public.status = "exchanging".into();
            (
                pending.public.provider,
                pending.public.redirect_uri.clone(),
                std::mem::take(&mut pending.verifier),
                pending.google_client.take(),
            )
        };
        let result = self
            .exchange(
                provider,
                google_client.as_ref(),
                &code,
                &state,
                &redirect,
                &verifier,
            )
            .await;
        self.finish_attempt(id, &result).await;
        result
    }

    async fn exchange(
        &self,
        provider: SubscriptionProvider,
        google_client: Option<&GoogleSignIn>,
        code: &str,
        state: &str,
        redirect: &str,
        verifier: &str,
    ) -> Result<AccountSummary, SubscriptionError> {
        if matches!(
            provider,
            SubscriptionProvider::Gemini | SubscriptionProvider::Antigravity
        ) {
            let sign_in = google_client.ok_or(SubscriptionError::OAuthNotConfigured)?;
            let value = self
                .google
                .exchange(provider, sign_in, code, redirect, verifier)
                .await?;
            return self.store.import(value, None).await;
        }
        let request = match provider {
            SubscriptionProvider::Codex => self.client.post(&self.codex_token_url).form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CODEX_CLIENT),
                ("code", code),
                ("redirect_uri", redirect),
                ("code_verifier", verifier),
            ]),
            SubscriptionProvider::Claude => {
                #[derive(Serialize)]
                struct ClaudeExchange<'a> {
                    grant_type: &'a str,
                    code: &'a str,
                    redirect_uri: &'a str,
                    client_id: &'a str,
                    code_verifier: &'a str,
                    state: &'a str,
                }
                self.client
                    .post(&self.claude_token_url)
                    .header("User-Agent", "axios/1.15.2")
                    .json(&ClaudeExchange {
                        grant_type: "authorization_code",
                        code,
                        redirect_uri: redirect,
                        client_id: CLAUDE_CLIENT,
                        code_verifier: verifier,
                        state,
                    })
            }
            _ => return Err(SubscriptionError::Unsupported),
        };
        let response = send_json(request).await?;
        let value = normalized_tokens(provider, response, None)?;
        self.store.import(value, None).await
    }

    async fn finish_attempt(&self, id: &str, result: &Result<AccountSummary, SubscriptionError>) {
        if let Some(attempt) = self.pending.lock().await.get_mut(id) {
            attempt.device = None;
            attempt.verifier.clear();
            attempt.verification_url = result
                .as_ref()
                .err()
                .and_then(SubscriptionError::verification_url)
                .map(str::to_owned);
            attempt.public.verification_required = result
                .as_ref()
                .err()
                .is_some_and(SubscriptionError::is_verification_required);
            attempt.public.verification_available = attempt.verification_url.is_some();
            attempt.public.error_code = match result {
                Err(SubscriptionError::GoogleProjectUnavailable) => {
                    Some("google_project_unavailable")
                }
                _ => None,
            };
            match result {
                Ok(account) => {
                    attempt.public.status = "complete".into();
                    attempt.public.account_id = Some(account.id.clone());
                    attempt.public.error = None;
                }
                Err(error) => {
                    attempt.public.status = "error".into();
                    attempt.public.error = Some(error.to_string());
                }
            }
        }
    }

    pub async fn credential(&self, id: &str) -> Result<AccountCredential, SubscriptionError> {
        self.refresh(id, false).await
    }

    pub async fn force_refresh(&self, id: &str) -> Result<AccountCredential, SubscriptionError> {
        self.refresh(id, true).await
    }

    fn is_revoked(&self, id: &str, refresh_token: &str) -> bool {
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .is_some_and(|revoked| revoked == refresh_token)
    }

    async fn refresh(&self, id: &str, force: bool) -> Result<AccountCredential, SubscriptionError> {
        let initial = self
            .store
            .get(id)
            .await
            .ok_or(SubscriptionError::NotFound)?;
        if !force && !initial.needs_refresh() {
            return Ok(initial);
        }
        let lock = self
            .store
            .refresh_locks
            .lock()
            .await
            .entry(id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        let mut account = self
            .store
            .get(id)
            .await
            .ok_or(SubscriptionError::NotFound)?;
        if (!force && !account.needs_refresh())
            || (force && account.access_token != initial.access_token)
        {
            return Ok(account);
        }
        if account.provider == SubscriptionProvider::Meta {
            let dca = account
                .dca_token
                .as_deref()
                .ok_or(SubscriptionError::NoRefreshToken)?;
            let observed = time::OffsetDateTime::now_utc();
            let value = super::meta::mint(&self.meta_client, &self.meta_mint_url, dca).await?;
            super::meta::apply_minted(&mut account, &value)?;
            self.store.update_credential(account).await?;
            self.remember_meta_usage(id, &value, observed).await;
            return self.store.get(id).await.ok_or(SubscriptionError::NotFound);
        }
        let refresh_token = account
            .refresh_token
            .as_deref()
            .ok_or(SubscriptionError::NoRefreshToken)?;
        if self.is_revoked(id, refresh_token) {
            return Err(SubscriptionError::SessionRevoked);
        }
        let request = match account.provider {
            SubscriptionProvider::Codex => self.client.post(&self.codex_token_url).form(&[
                ("grant_type", "refresh_token"), ("client_id", CODEX_CLIENT), ("refresh_token", refresh_token), ("scope", "openid profile email"),
            ]),
            SubscriptionProvider::Claude => self.client.post(&self.claude_token_url).header("User-Agent", "axios/1.15.2")
                .json(&json!({"grant_type":"refresh_token","client_id":CLAUDE_CLIENT,"refresh_token":refresh_token,"scope":CLAUDE_SCOPE})),
            SubscriptionProvider::Xai => {
                let endpoint = match &account.token_endpoint { Some(endpoint) => validate_xai_endpoint(endpoint)?, None => self.xai_discovery().await?.1 };
                self.client.post(endpoint).form(&[("grant_type", "refresh_token"), ("client_id", XAI_CLIENT), ("refresh_token", refresh_token)])
            }
            SubscriptionProvider::Gemini | SubscriptionProvider::Antigravity => {
                match self.google.refresh_request(account.provider, refresh_token, account.client_id.as_deref(), account.client_secret.as_deref()) {
                    Ok(request) => request,
                    // The account cannot refresh until its client is back or it
                    // reconnects: store why on it. These errors are fixed texts
                    // from douane, never provider data, so they are kept whole.
                    Err(error) => {
                        let reason = error.to_string();
                        let _ = self.store.modify(id, |account| account.last_error = Some(reason)).await;
                        return Err(error);
                    }
                }
            }
            SubscriptionProvider::Kimi => self.client.post("https://auth.kimi.com/api/oauth/token")
                .form(&[("grant_type", "refresh_token"), ("client_id", "17e5f671-d194-4dfb-9706-5516cb48c098"), ("refresh_token", refresh_token)]),
            _ => return Err(SubscriptionError::Unsupported),
        };
        let result = send_refresh(request).await;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                if matches!(error, SubscriptionError::SessionRevoked) {
                    self.revoked
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id.into(), refresh_token.into());
                }
                let _ = self.store.mark_error(id, Some(error.to_string())).await;
                return Err(error);
            }
        };
        account.access_token =
            text(&response, &["access_token"]).ok_or(SubscriptionError::InvalidResponse)?;
        if let Some(rotated) = text(&response, &["refresh_token"]) {
            account.refresh_token = Some(rotated);
        }
        if let Some(token) = text(&response, &["id_token"]) {
            account.id_token = Some(token);
        }
        account.expires_at = expiry_from_response(&response);
        account.last_error = None;
        account.cooldown_until = None;
        apply_identity(&mut account);
        self.store.update_credential(account).await?;
        self.store.get(id).await.ok_or(SubscriptionError::NotFound)
    }

    async fn start_meta(&self) -> Result<OAuthAttempt, SubscriptionError> {
        let response = send_json(
            self.meta_client
                .post(&self.meta_device_url)
                .header("User-Agent", "muse-code/1.0.2")
                .form(&[("client_id", super::meta::CLIENT)]),
        )
        .await?;
        let code = text(&response, &["device_code"]).ok_or(SubscriptionError::InvalidResponse)?;
        let user_code =
            text(&response, &["user_code"]).ok_or(SubscriptionError::InvalidResponse)?;
        let url = text(
            &response,
            &["verification_uri_complete", "verification_uri"],
        )
        .filter(|url| super::meta::valid_verification(url))
        .ok_or(SubscriptionError::InvalidResponse)?;
        let interval = response["interval"].as_u64().unwrap_or(5).clamp(1, 60);
        let expires = response["expires_in"].as_i64().unwrap_or(900).clamp(1, 900);
        let attempt = OAuthAttempt {
            id: random_id(),
            provider: SubscriptionProvider::Meta,
            url,
            state: random_id(),
            redirect_uri: String::new(),
            expires_at: timestamp(
                time::OffsetDateTime::now_utc() + time::Duration::seconds(expires),
            ),
            status: "pending".into(),
            account_id: None,
            error: None,
            error_code: None,
            user_code: Some(user_code),
            interval: Some(interval),
            verification_required: false,
            verification_available: false,
        };
        self.save_attempt(Pending {
            public: attempt.clone(),
            verifier: String::new(),
            device: Some(DeviceFlow {
                provider: SubscriptionProvider::Meta,
                code,
                token_endpoint: self.meta_token_url.clone(),
                next_poll: time::OffsetDateTime::now_utc(),
                interval,
            }),
            verification_url: None,
            google_client: None,
        })
        .await;
        Ok(attempt)
    }

    async fn meta_account(&self, token: Value) -> Result<AccountSummary, SubscriptionError> {
        let dca = text(&token, &["access_token"]).ok_or(SubscriptionError::InvalidResponse)?;
        let observed = time::OffsetDateTime::now_utc();
        let minted = super::meta::mint(&self.meta_client, &self.meta_mint_url, &dca).await?;
        let mut value = minted.clone();
        value["type"] = json!("meta");
        value["dca_token"] = json!(dca);
        if let Some(expires) = token["expires_in"].as_i64().filter(|n| *n > 0) {
            value["dca_expired"] = json!(timestamp(
                time::OffsetDateTime::now_utc() + time::Duration::seconds(expires)
            ));
        }
        let account = self.store.import(value, None).await?;
        self.remember_meta_usage(&account.id, &minted, observed)
            .await;
        Ok(account)
    }

    async fn remember_meta_usage(&self, id: &str, minted: &Value, observed: time::OffsetDateTime) {
        if let Some(usage) = minted.get("subs_usage").filter(|value| value.is_object()) {
            self.meta_usage.lock().await.insert(
                id.into(),
                (
                    observed,
                    json!({"type":"response.subscription_usage", "subscription":usage}),
                ),
            );
        }
    }

    pub(crate) async fn take_meta_usage(&self, id: &str) -> Option<(time::OffsetDateTime, Value)> {
        self.meta_usage.lock().await.remove(id)
    }

    async fn xai_discovery(&self) -> Result<(String, String), SubscriptionError> {
        let response = send_json(self.client.get(XAI_DISCOVERY)).await?;
        let device = text(&response, &["device_authorization_endpoint"])
            .ok_or(SubscriptionError::InvalidResponse)?;
        let token =
            text(&response, &["token_endpoint"]).ok_or(SubscriptionError::InvalidResponse)?;
        Ok((
            validate_xai_endpoint(&device)?,
            validate_xai_endpoint(&token)?,
        ))
    }

    async fn start_xai(&self) -> Result<OAuthAttempt, SubscriptionError> {
        let (device_endpoint, token_endpoint) = self.xai_discovery().await?;
        let response = send_json(
            self.client
                .post(device_endpoint)
                .form(&[("client_id", XAI_CLIENT), ("scope", XAI_SCOPE)]),
        )
        .await?;
        let device_code =
            text(&response, &["device_code"]).ok_or(SubscriptionError::InvalidResponse)?;
        let user_code =
            text(&response, &["user_code"]).ok_or(SubscriptionError::InvalidResponse)?;
        let url = text(
            &response,
            &["verification_uri_complete", "verification_uri"],
        )
        .ok_or(SubscriptionError::InvalidResponse)?;
        validate_xai_endpoint(&url)?;
        let interval = response
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(5, 60);
        let expires = response
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(900)
            .clamp(1, 1800);
        let attempt = OAuthAttempt {
            id: random_id(),
            provider: SubscriptionProvider::Xai,
            url,
            state: random_id(),
            redirect_uri: String::new(),
            expires_at: timestamp(
                time::OffsetDateTime::now_utc() + time::Duration::seconds(expires),
            ),
            status: "pending".into(),
            account_id: None,
            error: None,
            error_code: None,
            user_code: Some(user_code),
            interval: Some(interval),
            verification_required: false,
            verification_available: false,
        };
        self.save_attempt(Pending {
            public: attempt.clone(),
            verifier: String::new(),
            device: Some(DeviceFlow {
                provider: SubscriptionProvider::Xai,
                code: device_code,
                token_endpoint,
                next_poll: time::OffsetDateTime::now_utc(),
                interval,
            }),
            verification_url: None,
            google_client: None,
        })
        .await;
        Ok(attempt)
    }

    /// GitHub's device flow: the user enters a code on github.com and the
    /// resulting OAuth token serves the Copilot API until it is revoked.
    async fn start_copilot(&self) -> Result<OAuthAttempt, SubscriptionError> {
        let response = send_json(
            self.client
                .post(&self.copilot_device_url)
                .header("User-Agent", crate::relay::COPILOT_USER_AGENT)
                .form(&[("client_id", COPILOT_CLIENT), ("scope", "read:user")]),
        )
        .await?;
        let device_code =
            text(&response, &["device_code"]).ok_or(SubscriptionError::InvalidResponse)?;
        let user_code =
            text(&response, &["user_code"]).ok_or(SubscriptionError::InvalidResponse)?;
        let url = text(&response, &["verification_uri"])
            .filter(|url| {
                reqwest::Url::parse(url)
                    .is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("github.com"))
            })
            .ok_or(SubscriptionError::InvalidResponse)?;
        let interval = response
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, 60);
        let expires = response
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(900)
            .clamp(1, 1800);
        let attempt = OAuthAttempt {
            id: random_id(),
            provider: SubscriptionProvider::Copilot,
            url,
            state: random_id(),
            redirect_uri: String::new(),
            expires_at: timestamp(
                time::OffsetDateTime::now_utc() + time::Duration::seconds(expires),
            ),
            status: "pending".into(),
            account_id: None,
            error: None,
            error_code: None,
            user_code: Some(user_code),
            interval: Some(interval),
            verification_required: false,
            verification_available: false,
        };
        self.save_attempt(Pending {
            public: attempt.clone(),
            verifier: String::new(),
            device: Some(DeviceFlow {
                provider: SubscriptionProvider::Copilot,
                code: device_code,
                token_endpoint: self.copilot_token_url.clone(),
                next_poll: time::OffsetDateTime::now_utc(),
                interval,
            }),
            verification_url: None,
            google_client: None,
        })
        .await;
        Ok(attempt)
    }

    /// The GitHub login behind a Copilot token, so a reconnect replaces the
    /// account instead of adding a second one. Missing identity is no error.
    async fn copilot_account(&self, token: &str) -> Value {
        let mut account = json!({"type":"copilot","access_token":token});
        if let Ok(user) = send_json(
            self.client
                .get(&self.github_user_url)
                .header("Authorization", format!("token {token}"))
                .header("User-Agent", crate::relay::COPILOT_USER_AGENT)
                .timeout(Duration::from_secs(10)),
        )
        .await
        {
            if let Some(id) = user.get("id").and_then(Value::as_u64) {
                account["account_id"] = json!(id.to_string());
            }
            if let Some(login) = text(&user, &["login"]) {
                account["name"] = json!(login);
            }
            if let Some(email) = text(&user, &["email"]) {
                account["email"] = json!(email);
            }
        }
        account
    }

    async fn poll_device(&self, id: &str) {
        let flow = {
            let mut attempts = self.pending.lock().await;
            let Some(pending) = attempts.get_mut(id) else {
                return;
            };
            if pending.public.status != "pending"
                || parse_timestamp(&pending.public.expires_at)
                    .is_none_or(|t| t <= time::OffsetDateTime::now_utc())
            {
                return;
            }
            let Some(flow) = pending.device.as_mut() else {
                return;
            };
            if flow.next_poll > time::OffsetDateTime::now_utc() {
                return;
            }
            // Reserve a full HTTP timeout so overlapping dashboard polls cannot
            // consume a single device code simultaneously.
            flow.next_poll = time::OffsetDateTime::now_utc() + time::Duration::seconds(31);
            flow.clone()
        };
        let client_id = match flow.provider {
            SubscriptionProvider::Copilot => COPILOT_CLIENT,
            SubscriptionProvider::Meta => super::meta::CLIENT,
            _ => XAI_CLIENT,
        };
        let poll_client = if flow.provider == SubscriptionProvider::Meta {
            &self.meta_client
        } else {
            &self.client
        };
        let response = poll_client
            .post(&flow.token_endpoint)
            .timeout(Duration::from_secs(30))
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", flow.code.as_str()),
            ])
            .send()
            .await;
        let result = match response {
            Ok(response) => {
                let status = response.status();
                let value = response.json::<Value>().await.ok();
                // GitHub answers a pending authorization with 200 and an error.
                let error = value.as_ref().and_then(|v| text(v, &["error"]));
                if matches!(
                    error.as_deref(),
                    Some("authorization_pending" | "slow_down")
                ) {
                    let mut attempts = self.pending.lock().await;
                    if let Some(device) = attempts.get_mut(id).and_then(|p| p.device.as_mut()) {
                        if error.as_deref() == Some("slow_down") {
                            device.interval = value
                                .as_ref()
                                .and_then(|v| v.get("interval"))
                                .and_then(Value::as_u64)
                                .unwrap_or(device.interval + 5)
                                .clamp(1, 60);
                        }
                        device.next_poll = time::OffsetDateTime::now_utc()
                            + time::Duration::seconds(device.interval as i64);
                    }
                    return;
                }
                match value {
                    _ if error.as_deref() == Some("expired_token") => {
                        Err(SubscriptionError::Expired)
                    }
                    _ if error.as_deref() == Some("access_denied") => {
                        Err(SubscriptionError::Upstream(403))
                    }
                    Some(value) if status.is_success() && error.is_none() => match flow.provider {
                        SubscriptionProvider::Meta => self.meta_account(value).await,

                        SubscriptionProvider::Copilot => match text(&value, &["access_token"]) {
                            Some(token) => {
                                let account = self.copilot_account(&token).await;
                                self.store.import(account, None).await
                            }
                            None => Err(SubscriptionError::InvalidResponse),
                        },
                        provider => {
                            match normalized_tokens(provider, value, Some(&flow.token_endpoint)) {
                                Ok(value) => self.store.import(value, None).await,
                                Err(error) => Err(error),
                            }
                        }
                    },
                    _ if status.is_success() => Err(SubscriptionError::InvalidResponse),
                    _ => Err(SubscriptionError::Upstream(status.as_u16())),
                }
            }
            Err(_) => Err(SubscriptionError::Transport),
        };
        self.finish_attempt(id, &result).await;
    }
}

pub(crate) async fn send_json(
    request: reqwest::RequestBuilder,
) -> Result<Value, SubscriptionError> {
    let mut response = request
        .timeout(Duration::from_secs(30))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| SubscriptionError::Transport)?;
    if !response.status().is_success() {
        let status = response.status();
        let source = response.url().clone();
        if status == reqwest::StatusCode::FORBIDDEN && is_google_cloudcode_url(&source) {
            let mut body = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) if body.len() + chunk.len() <= GOOGLE_ERROR_BODY_LIMIT => {
                        body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    _ => return Err(SubscriptionError::Upstream(status.as_u16())),
                }
            }
            if let Some(error) = google_verification_error(status, &source, &body) {
                return Err(error);
            }
        }
        return Err(SubscriptionError::Upstream(status.as_u16()));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| SubscriptionError::InvalidResponse)
}

const GOOGLE_ERROR_BODY_LIMIT: usize = 65_536;
const REFRESH_ERROR_BODY_LIMIT: usize = 16_384;

/// Token refresh with a bounded look at the OAuth error code. A revoked or
/// reused refresh token cannot recover by retrying, so it surfaces as
/// `SessionRevoked` instead of a bare HTTP status. The body is never logged
/// or stored: only the standard `error` code is read.
/// The `last_error` of an account imported without its Cloud project: why
/// the lookup failed. An import refusal's "invalid authentication file"
/// prefix is left out, since the account was imported.
fn discovery_failure(error: &SubscriptionError) -> String {
    let reason = match error {
        SubscriptionError::InvalidImport(reason) => (*reason).to_owned(),
        error => error.to_string(),
    };
    format!("no Cloud project was found at import: {reason}")
}

async fn send_refresh(request: reqwest::RequestBuilder) -> Result<Value, SubscriptionError> {
    let mut response = request
        .timeout(Duration::from_secs(30))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| SubscriptionError::Transport)?;
    let status = response.status();
    if status.is_success() {
        return response
            .json::<Value>()
            .await
            .map_err(|_| SubscriptionError::InvalidResponse);
    }
    if matches!(status.as_u16(), 400 | 401) {
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if body.len() + chunk.len() <= REFRESH_ERROR_BODY_LIMIT => {
                    body.extend_from_slice(&chunk)
                }
                Ok(None) => break,
                _ => return Err(SubscriptionError::Upstream(status.as_u16())),
            }
        }
        if refresh_error_is_revocation(&body) {
            return Err(SubscriptionError::SessionRevoked);
        }
    }
    Err(SubscriptionError::Upstream(status.as_u16()))
}

pub(crate) fn refresh_error_is_revocation(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let code = value.get("error").and_then(|error| {
        error
            .as_str()
            .or_else(|| error.get("code").and_then(Value::as_str))
    });
    matches!(
        code,
        Some(
            "invalid_grant"
                | "refresh_token_reused"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
        )
    )
}

pub(crate) fn validate_google_verification_url(value: &str) -> Option<String> {
    if value.len() > 8192
        || !value.is_ascii()
        || value
            .chars()
            .any(|character| character.is_ascii_control() || character.is_whitespace())
    {
        return None;
    }
    let suffix = value.strip_prefix("https://accounts.google.com")?;
    if suffix != "/signin/continue" && !suffix.starts_with("/signin/continue?") {
        return None;
    }
    let url = reqwest::Url::parse(value).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("accounts.google.com")
        || url.path() != "/signin/continue"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(value.to_owned())
}

fn is_google_cloudcode_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some(
                "cloudcode-pa.googleapis.com"
                    | "daily-cloudcode-pa.googleapis.com"
                    | "daily-cloudcode-pa.sandbox.googleapis.com"
            )
        )
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
}

pub(crate) fn google_verification_error(
    status: reqwest::StatusCode,
    source: &reqwest::Url,
    body: &[u8],
) -> Option<SubscriptionError> {
    if status != reqwest::StatusCode::FORBIDDEN
        || !is_google_cloudcode_url(source)
        || body.len() > GOOGLE_ERROR_BODY_LIMIT
    {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let details = value.get("error")?.get("details")?.as_array()?;
    let detail = details.iter().find(|detail| {
        detail["@type"] == "type.googleapis.com/google.rpc.ErrorInfo"
            && detail["reason"] == "VALIDATION_REQUIRED"
    })?;
    let help_url = details
        .iter()
        .filter(|detail| detail["@type"] == "type.googleapis.com/google.rpc.Help")
        .filter_map(|detail| detail.get("links").and_then(Value::as_array))
        .flatten()
        .filter_map(|link| link.get("url").and_then(Value::as_str))
        .find_map(validate_google_verification_url);
    let url = help_url.or_else(|| {
        let metadata = detail.get("metadata")?;
        ["validation_link", "validationUrl", "validation_url"]
            .into_iter()
            .filter_map(|key| metadata.get(key).and_then(Value::as_str))
            .find_map(validate_google_verification_url)
    });
    Some(SubscriptionError::verification_required(url.as_deref()))
}

fn expiry_from_response(value: &Value) -> Option<String> {
    value
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .map(|seconds| {
            timestamp(
                time::OffsetDateTime::now_utc() + time::Duration::seconds(seconds.min(31_536_000)),
            )
        })
}

fn normalized_tokens(
    provider: SubscriptionProvider,
    mut value: Value,
    token_endpoint: Option<&str>,
) -> Result<Value, SubscriptionError> {
    if text(&value, &["access_token"]).is_none() {
        return Err(SubscriptionError::InvalidResponse);
    }
    let expires = expiry_from_response(&value);
    let email = value
        .get("account")
        .and_then(|a| text(a, &["email_address", "email"]));
    let account_id = value.get("account").and_then(|a| text(a, &["uuid"]));
    let object = value
        .as_object_mut()
        .ok_or(SubscriptionError::InvalidResponse)?;
    object.insert("type".into(), json!(provider.as_str()));
    if let Some(expires) = expires {
        object.insert("expired".into(), json!(expires));
    }
    if let Some(email) = email {
        object.insert("email".into(), json!(email));
    }
    if let Some(id) = account_id {
        object.insert("account_id".into(), json!(id));
    }
    if let Some(endpoint) = token_endpoint {
        object.insert("token_endpoint".into(), json!(endpoint));
    }
    if provider == SubscriptionProvider::Xai {
        object.insert(
            "base_url".into(),
            json!("https://cli-chat-proxy.grok.com/v1"),
        );
    }
    Ok(value)
}

fn callback_fields(
    code: &str,
    supplied_state: &str,
) -> Result<(String, String), SubscriptionError> {
    let code = code.trim();
    let mut state = supplied_state.trim().to_owned();
    let code = if code.starts_with("http://") || code.starts_with("https://") {
        let url = reqwest::Url::parse(code).map_err(|_| SubscriptionError::InvalidResponse)?;
        let mut found = None;
        for (key, value) in url.query_pairs() {
            if key == "code" {
                found = Some(value.into_owned());
            } else if key == "state" {
                if !state.is_empty() && state != value {
                    return Err(SubscriptionError::InvalidState);
                }
                state = value.into_owned();
            }
        }
        found.ok_or(SubscriptionError::InvalidResponse)?
    } else if let Some((code, fragment_state)) = code.split_once('#') {
        if !state.is_empty() && state != fragment_state {
            return Err(SubscriptionError::InvalidState);
        }
        state = fragment_state.into();
        code.into()
    } else {
        code.into()
    };
    if code.is_empty() || state.is_empty() || code.len() > 8192 {
        return Err(SubscriptionError::InvalidResponse);
    }
    Ok((code, state))
}

fn validate_redirect(redirect: &str) -> Result<(), SubscriptionError> {
    let url = reqwest::Url::parse(redirect).map_err(|_| SubscriptionError::InvalidRedirect)?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SubscriptionError::InvalidRedirect);
    }
    Ok(())
}

fn validate_xai_endpoint(endpoint: &str) -> Result<String, SubscriptionError> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| SubscriptionError::InvalidResponse)?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https"
        || !(host == "x.ai" || host.ends_with(".x.ai"))
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(SubscriptionError::InvalidResponse);
    }
    Ok(endpoint.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GitHub keeps answering 200 while the user has not entered the code,
    /// then hands over a token; the login names the account.
    #[tokio::test]
    async fn copilot_device_flow_waits_through_pending_answers_then_connects() {
        use axum::{
            Json, Router,
            routing::{get, post},
        };
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = polls.clone();
        let router = Router::new()
            .route(
                "/device",
                post(|| async {
                    Json(json!({"device_code":"device-fixture","user_code":"ABCD-1234",
                        "verification_uri":"https://github.com/login/device","interval":1,"expires_in":900}))
                }),
            )
            .route(
                "/token",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                            Json(json!({"error":"authorization_pending"}))
                        } else {
                            Json(json!({"access_token":"gho_fixture","token_type":"bearer","scope":"read:user"}))
                        }
                    }
                }),
            )
            .route("/user", get(|| async { Json(json!({"id":42,"login":"octocat"})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let mut manager = OAuthManager::new(reqwest::Client::new(), store.clone());
        manager.copilot_device_url = format!("{base}/device");
        manager.copilot_token_url = format!("{base}/token");
        manager.github_user_url = format!("{base}/user");
        let attempt = manager
            .start(SubscriptionProvider::Copilot, None)
            .await
            .unwrap();
        assert_eq!(attempt.user_code.as_deref(), Some("ABCD-1234"));
        assert_eq!(attempt.url, "https://github.com/login/device");
        let first = manager.status(&attempt.id).await.unwrap();
        assert_eq!(first.status, "pending");
        assert!(store.list().await.is_empty());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let done = manager.status(&attempt.id).await.unwrap();
        assert_eq!(done.status, "complete", "{:?}", done.error);
        let accounts = store.list().await;
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].provider, SubscriptionProvider::Copilot);
        assert_eq!(accounts[0].name, "octocat");
        let credential = store.get(&accounts[0].id).await.unwrap();
        assert_eq!(credential.access_token, "gho_fixture");
        assert_eq!(credential.account_id.as_deref(), Some("42"));
        assert!(credential.refresh_token.is_none() && credential.expires_at.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn meta_device_lifecycle_mint_renewal_and_private_storage() {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let polls = Arc::new(AtomicUsize::new(0));
        let mints = Arc::new(AtomicUsize::new(0));
        let poll_count = polls.clone();
        let mint_count = mints.clone();
        let app = Router::new()
            .route("/device", post(|form: axum::extract::Form<BTreeMap<String,String>>| async move {
                assert_eq!(form["client_id"], super::super::meta::CLIENT);
                Json(json!({"device_code":"secret-device", "user_code":"TEST-CODE", "verification_uri":"https://auth.meta.com/activate", "interval":1, "expires_in":900}))
            }))
            .route("/token", post(move |headers: axum::http::HeaderMap, form: axum::extract::Form<BTreeMap<String,String>>| { let count=poll_count.clone(); async move {
                assert_eq!(form["device_code"], "secret-device");
                assert_eq!(form["client_id"], super::super::meta::CLIENT);
                // Meta redirects clients without its native User-Agent to
                // facebook.com/unsupportedbrowser instead of returning OAuth JSON.
                if headers.get("user-agent").and_then(|h| h.to_str().ok()) != Some("muse-code/1.0.2") {
                    return (StatusCode::FOUND, Json(json!({})));
                }
                match count.fetch_add(1, Ordering::SeqCst) {
                    0 => (StatusCode::BAD_REQUEST, Json(json!({"error":"authorization_pending"}))),
                    1 => (StatusCode::BAD_REQUEST, Json(json!({"error":"slow_down"}))),
                    _ => (StatusCode::OK, Json(json!({"access_token":"dca:private", "expires_in":3600})))
                }
            }}))
            .route("/mint", post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| { let count=mint_count.clone(); async move {
                assert_eq!(headers["authorization"], "Bearer dca:private");
                assert_eq!(body["dca_token"], "dca:private");
                let n=count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                Json(json!({"api_key":format!("private-key-{n}"), "base_url":"https://api.meta.ai/v1", "user_email":"meta@example.test", "subs_tier_name":if n==0 {"Contributor"} else {"Updated"}, "subs_tier_id":"contributor", "is_subs_active":true, "require_payment":false}))
            }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(dir.path()).unwrap());
        let mut manager = OAuthManager::new(reqwest::Client::new(), store.clone());
        manager.meta_device_url = format!("{base}/device");
        manager.meta_token_url = format!("{base}/token");
        manager.meta_mint_url = format!("{base}/mint");
        let attempt = manager
            .start(SubscriptionProvider::Meta, None)
            .await
            .unwrap();
        assert_eq!(attempt.user_code.as_deref(), Some("TEST-CODE"));
        assert!(
            !serde_json::to_string(&attempt)
                .unwrap()
                .contains("secret-device")
        );
        for expected in ["pending", "pending", "complete"] {
            manager
                .pending
                .lock()
                .await
                .get_mut(&attempt.id)
                .unwrap()
                .device
                .as_mut()
                .unwrap()
                .next_poll = time::OffsetDateTime::now_utc();
            let status = manager.status(&attempt.id).await.unwrap();
            assert_eq!(status.status, expected);
            if expected == "pending" && polls.load(Ordering::SeqCst) == 2 {
                assert_eq!(
                    manager.pending.lock().await[&attempt.id]
                        .device
                        .as_ref()
                        .unwrap()
                        .interval,
                    6
                );
            }
        }
        let id = manager
            .status(&attempt.id)
            .await
            .unwrap()
            .account_id
            .unwrap();
        let credential = store.get(&id).await.unwrap();
        assert_eq!(credential.access_token, "private-key-0");
        assert_eq!(credential.dca_token.as_deref(), Some("dca:private"));
        assert!(credential.expires_at.is_none());
        assert!(credential.dca_expires_at.is_some());
        let (a, b) = tokio::join!(manager.force_refresh(&id), manager.force_refresh(&id));
        assert_eq!(a.unwrap().access_token, "private-key-1");
        assert_eq!(b.unwrap().access_token, "private-key-1");
        assert_eq!(mints.load(Ordering::SeqCst), 2);
        assert_eq!(
            store.get(&id).await.unwrap().subscription.unwrap().label,
            "Updated"
        );
        let public = serde_json::to_string(&store.list().await).unwrap();
        for secret in ["dca:private", "private-key-1", "secret-device"] {
            assert!(!public.contains(secret));
        }
        let exported = serde_json::to_value(store.export().await.remove(0)).unwrap();
        assert_eq!(exported["dca_token"], "dca:private");
        let fresh_dir = tempfile::tempdir().unwrap();
        let restored = AccountStore::open(fresh_dir.path()).unwrap();
        let restored_id = restored.restore(exported).await.unwrap().id;
        assert_eq!(
            restored
                .get(&restored_id)
                .await
                .unwrap()
                .dca_token
                .as_deref(),
            Some("dca:private")
        );
        assert_eq!(
            AccountStore::open(dir.path())
                .unwrap()
                .get(&id)
                .await
                .unwrap()
                .access_token,
            "private-key-1"
        );
        server.abort();
    }

    #[tokio::test]
    async fn meta_quota_bootstraps_from_mint_and_refreshes_without_inference() {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mints = Arc::new(AtomicUsize::new(0));
        let calls = mints.clone();
        let app = Router::new().route("/mint", post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
            let calls = calls.clone();
            async move {
                assert_eq!(body["dca_token"], "dca:private");
                assert_eq!(body["onboard"], false);
                assert_eq!(headers["x-api-version"], "1.0.0");
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 2 {
                    return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"private-error"})));
                }
                (StatusCode::OK, Json(json!({
                    "api_key":format!("private-key-{n}"), "is_subs_active":true,
                    "require_payment":false, "subs_tier_name":"Power Usage", "subs_tier_id":"power",
                    "subs_usage":{"weekly":{"used_percent":20+n,"resets_at":1893456000},
                                  "window":{"used_percent":40+n,"resets_at":1893440000,"window_duration_mins":300}}
                })))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mint", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(dir.path()).unwrap());
        let mut manager = OAuthManager::new(reqwest::Client::new(), store.clone());
        manager.meta_mint_url = endpoint;
        let account = manager
            .meta_account(json!({"access_token":"dca:private"}))
            .await
            .unwrap();
        let service = super::super::quota::QuotaService::with_oauth(
            reqwest::Client::new(),
            store.clone(),
            Arc::new(manager),
        );
        let first = service.refresh(&account.id).await.unwrap();
        assert_eq!(
            first.windows.len(),
            2,
            "a newly connected account needs no inference to show quotas"
        );
        assert_eq!(first.windows[0].remaining_percent, Some(80.0));
        assert_eq!(first.windows[1].id, "five-hour");
        assert_eq!(first.windows[1].remaining_percent, Some(60.0));
        assert_eq!(
            mints.load(Ordering::SeqCst),
            1,
            "reuse login's quota observation"
        );
        let refreshed = service.refresh(&account.id).await.unwrap();
        assert_eq!(refreshed.windows[0].remaining_percent, Some(79.0));
        assert_eq!(refreshed.windows[1].remaining_percent, Some(59.0));
        assert_eq!(
            store.get(&account.id).await.unwrap().access_token,
            "private-key-1"
        );
        assert_eq!(mints.load(Ordering::SeqCst), 2);
        let failed = service.refresh(&account.id).await.unwrap();
        assert_eq!(
            serde_json::to_value(&failed.windows).unwrap(),
            serde_json::to_value(&refreshed.windows).unwrap()
        );
        assert_eq!(failed.updated_at, refreshed.updated_at);
        assert!(failed.error.as_deref().is_some_and(|e| e.contains("503")));
        let public = serde_json::to_string(&failed).unwrap();
        assert!(
            !public.contains("private-key")
                && !public.contains("dca:")
                && !public.contains("private-error")
        );
        let recovered = service.refresh(&account.id).await.unwrap();
        assert_eq!(recovered.windows[0].remaining_percent, Some(77.0));
        assert!(recovered.error.is_none());
        let reopened = super::super::quota::QuotaService::new(reqwest::Client::new(), store);
        assert_eq!(
            serde_json::to_value(reopened.get(&account.id).await.unwrap().windows).unwrap(),
            serde_json::to_value(recovered.windows).unwrap()
        );
        server.abort();
    }

    #[tokio::test]
    async fn meta_device_denial_expiry_and_mint_errors_are_safe() {
        use axum::{Json, Router, http::StatusCode, routing::post};
        for reason in ["access_denied", "expired_token", "private-token-error"] {
            let app = Router::new().route(
                "/token",
                post(move || async move {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error":reason,"error_description":"dca:secret"})),
                    )
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(AccountStore::open(dir.path()).unwrap());
            let manager = OAuthManager::new(reqwest::Client::new(), store.clone());
            let mut attempt = manager
                .start(SubscriptionProvider::Codex, None)
                .await
                .unwrap();
            attempt.provider = SubscriptionProvider::Meta;
            manager
                .save_attempt(Pending {
                    public: attempt.clone(),
                    verifier: String::new(),
                    device: Some(DeviceFlow {
                        provider: SubscriptionProvider::Meta,
                        code: "secret".into(),
                        token_endpoint: endpoint,
                        next_poll: time::OffsetDateTime::now_utc(),
                        interval: 1,
                    }),
                    verification_url: None,
                    google_client: None,
                })
                .await;
            let status = manager.status(&attempt.id).await.unwrap();
            assert_eq!(status.status, "error");
            let public = serde_json::to_string(&status).unwrap();
            assert!(!public.contains("dca:secret") && !public.contains("private-token-error"));
            assert!(store.list().await.is_empty());
            server.abort();
        }
    }

    #[test]
    fn revoked_refresh_tokens_are_terminal() {
        assert!(refresh_error_is_revocation(
            br#"{"error":"invalid_grant","error_description":"Bad Request"}"#
        ));
        assert!(refresh_error_is_revocation(
            br#"{"error":{"code":"refresh_token_reused","message":"x"}}"#
        ));
        assert!(!refresh_error_is_revocation(
            br#"{"error":"invalid_request","error_description":"client_secret is missing."}"#
        ));
        assert!(!refresh_error_is_revocation(b"<html>bad gateway</html>"));
        assert!(
            SubscriptionError::SessionRevoked
                .to_string()
                .contains("reconnect")
        );
    }

    #[tokio::test]
    async fn missing_google_project_is_terminal_without_importing_an_account() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let manager = OAuthManager::new(reqwest::Client::new(), store.clone());
        let attempt = manager
            .start(SubscriptionProvider::Codex, None)
            .await
            .unwrap();
        manager
            .finish_attempt(
                &attempt.id,
                &Err(SubscriptionError::GoogleProjectUnavailable),
            )
            .await;
        let status = manager.status(&attempt.id).await.unwrap();
        assert_eq!(status.status, "error");
        assert_eq!(status.error_code, Some("google_project_unavailable"));
        assert!(!status.verification_required);
        assert!(!status.verification_available);
        assert!(status.account_id.is_none());
        assert!(store.list().await.is_empty());
        assert!(
            manager.pending.lock().await[&attempt.id]
                .verifier
                .is_empty()
        );
        assert!(matches!(
            manager
                .complete(&attempt.id, "consumed-code", &attempt.state)
                .await,
            Err(SubscriptionError::Expired)
        ));
        let retry = manager
            .start(SubscriptionProvider::Codex, None)
            .await
            .unwrap();
        assert_ne!(retry.id, attempt.id);
        assert!(retry.error_code.is_none());
        assert_eq!(retry.status, "pending");
    }

    #[tokio::test]
    async fn oauth_verification_is_private_terminal_and_expires_without_persisting_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let manager = OAuthManager::new(reqwest::Client::new(), store.clone());
        let attempt = manager
            .start(SubscriptionProvider::Codex, None)
            .await
            .unwrap();
        let challenge = "https://accounts.google.com/signin/continue?challenge=private-fixture";
        manager
            .finish_attempt(
                &attempt.id,
                &Err(SubscriptionError::verification_required(Some(challenge))),
            )
            .await;
        let status = manager.status(&attempt.id).await.unwrap();
        assert_eq!(status.status, "error");
        assert!(status.verification_required);
        assert!(status.verification_available);
        assert!(status.account_id.is_none());
        assert_eq!(
            manager.verification_url(&attempt.id).await.as_deref(),
            Some(challenge)
        );
        assert!(
            manager.pending.lock().await[&attempt.id]
                .verifier
                .is_empty()
        );
        assert!(store.list().await.is_empty());
        let serialized = serde_json::to_string(&status).unwrap();
        assert!(!serialized.contains("private-fixture"));
        assert!(!serialized.contains("signin/continue"));
        assert!(!format!("{status:?}").contains("private-fixture"));
        assert!(matches!(
            manager
                .complete(&attempt.id, "fixture-code", &attempt.state)
                .await,
            Err(SubscriptionError::Expired)
        ));

        manager
            .finish_attempt(
                &attempt.id,
                &Err(SubscriptionError::verification_required(None)),
            )
            .await;
        let missing = manager.status(&attempt.id).await.unwrap();
        assert!(missing.verification_required);
        assert!(!missing.verification_available);
        assert!(manager.verification_url(&attempt.id).await.is_none());

        manager
            .finish_attempt(
                &attempt.id,
                &Err(SubscriptionError::verification_required(Some(challenge))),
            )
            .await;
        manager
            .pending
            .lock()
            .await
            .get_mut(&attempt.id)
            .unwrap()
            .public
            .expires_at = "2020-01-01T00:00:00Z".into();
        let expired = manager.status(&attempt.id).await.unwrap();
        assert!(expired.verification_required);
        assert!(!expired.verification_available);
        assert!(manager.verification_url(&attempt.id).await.is_none());
        assert!(manager.verification_url("unknown").await.is_none());
    }

    #[test]
    fn google_verification_native_reason_is_actionable_and_redacted() {
        let challenge = "https://accounts.google.com/signin/continue?challenge=private-fixture";
        let body = serde_json::to_vec(&json!({"error":{
            "status":"PERMISSION_DENIED",
            "message":"untrusted private provider message",
            "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo",
                "reason":"VALIDATION_REQUIRED","metadata":{"validationUrl":challenge}}]
        }}))
        .unwrap();
        let source = reqwest::Url::parse(
            "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        )
        .unwrap();
        let error = google_verification_error(reqwest::StatusCode::FORBIDDEN, &source, &body)
            .expect("native Google validation reason must survive quota refresh");
        assert!(error.is_verification_required());
        assert_eq!(error.verification_url(), Some(challenge));
        assert!(error.to_string().contains("verification is required"));
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("private-fixture"));
            assert!(!rendered.contains("accounts.google.com"));
            assert!(!rendered.contains("untrusted private"));
        }
    }

    #[test]
    fn google_verification_help_and_metadata_aliases_preserve_only_valid_actions() {
        let challenge = "https://accounts.google.com/signin/continue?challenge=private-fixture";
        let source =
            reqwest::Url::parse("https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
                .unwrap();
        for key in ["validation_link", "validationUrl", "validation_url"] {
            let mut metadata = serde_json::Map::new();
            metadata.insert(key.into(), json!(challenge));
            let body = serde_json::to_vec(&json!({"error":{"details":[{
                "@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"VALIDATION_REQUIRED","metadata":metadata
            }]}})).unwrap();
            let error =
                google_verification_error(reqwest::StatusCode::FORBIDDEN, &source, &body).unwrap();
            assert_eq!(error.verification_url(), Some(challenge));
        }
        for help_url in [challenge, "https://evil.test/challenge"] {
            let body = serde_json::to_vec(&json!({"error":{"details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"VALIDATION_REQUIRED"},
                {"@type":"type.googleapis.com/google.rpc.Help","links":[{"url":help_url,"description":"private provider text"}]}
            ]}})).unwrap();
            let error =
                google_verification_error(reqwest::StatusCode::FORBIDDEN, &source, &body).unwrap();
            assert!(error.is_verification_required());
            assert_eq!(
                error.verification_url(),
                (help_url == challenge).then_some(challenge)
            );
            assert!(!format!("{error:?}").contains("private"));
        }
    }

    #[test]
    fn google_verification_challenge_url_has_an_exact_allowlist() {
        let good = "https://accounts.google.com/signin/continue?challenge=private-fixture";
        assert_eq!(
            validate_google_verification_url(good).as_deref(),
            Some(good)
        );
        let oversized = format!("{good}{}", "x".repeat(8192));
        for bad in [
            "http://accounts.google.com/signin/continue",
            "https://accounts.google.com.evil.test/signin/continue",
            "https://evil.test/signin/continue",
            "https://user@accounts.google.com/signin/continue",
            "https://user:pass@accounts.google.com/signin/continue",
            "https://accounts.google.com:443/signin/continue",
            "https://accounts.google.com:8443/signin/continue",
            "https://accounts.google.com/signin/continue#private-fragment",
            "https://accounts.google.com/other",
            "https://accounts.google.com/signin/continue/other",
            "https://accounts.google.com/signin/./continue",
            "https://accounts.google.com/signin/continue?challenge=x\n",
            oversized.as_str(),
        ] {
            assert!(validate_google_verification_url(bad).is_none());
            let error = SubscriptionError::verification_required(Some(bad));
            assert!(error.is_verification_required());
            assert!(error.verification_url().is_none());
        }
    }

    #[test]
    fn google_verification_reason_requires_known_host_status_and_structure() {
        let body = serde_json::to_vec(&json!({"error":{"details":[{
            "@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"VALIDATION_REQUIRED"
        }]}}))
        .unwrap();
        let known =
            reqwest::Url::parse("https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
                .unwrap();
        let error =
            google_verification_error(reqwest::StatusCode::FORBIDDEN, &known, &body).unwrap();
        assert!(error.is_verification_required());
        assert!(error.verification_url().is_none());
        for host in [
            "https://cloudcode-pa.googleapis.com.evil.test",
            "https://evil.test",
            "http://cloudcode-pa.googleapis.com",
        ] {
            let source = reqwest::Url::parse(host).unwrap();
            assert!(
                google_verification_error(reqwest::StatusCode::FORBIDDEN, &source, &body).is_none()
            );
        }
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::OK,
        ] {
            assert!(google_verification_error(status, &known, &body).is_none());
        }
        for invalid in [
            json!({"error":{"message":"VALIDATION_REQUIRED"}}),
            json!({"error":{"details":[{"reason":"VALIDATION_REQUIRED"}]}}),
            json!({"error":{"details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"OTHER"}]}}),
        ] {
            let invalid = serde_json::to_vec(&invalid).unwrap();
            assert!(
                google_verification_error(reqwest::StatusCode::FORBIDDEN, &known, &invalid)
                    .is_none()
            );
        }
        let oversized = vec![b' '; 65_537];
        assert!(
            google_verification_error(reqwest::StatusCode::FORBIDDEN, &known, &oversized).is_none()
        );
    }

    #[test]
    fn accepts_pasted_callback_and_claude_fragment_with_state_validation() {
        assert_eq!(
            callback_fields("http://localhost:1455/auth/callback?code=abc&state=xyz", "").unwrap(),
            ("abc".into(), "xyz".into())
        );
        assert_eq!(
            callback_fields("code#state", "state").unwrap(),
            ("code".into(), "state".into())
        );
        assert!(matches!(
            callback_fields("code#wrong", "state"),
            Err(SubscriptionError::InvalidState)
        ));
        assert!(validate_redirect("https://unrelated.example/callback").is_err());
        assert!(validate_xai_endpoint("https://auth.x.ai.evil.example/token").is_err());
    }

    #[tokio::test]
    async fn browser_authorization_uses_pkce_and_rejects_wrong_state_before_network() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let manager = OAuthManager::new(reqwest::Client::new(), store);
        let attempt = manager
            .start(SubscriptionProvider::Codex, None)
            .await
            .unwrap();
        let url = reqwest::Url::parse(&attempt.url).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["state"], attempt.state);
        assert_eq!(query["code_challenge"].len(), 43);
        assert!(matches!(
            manager.complete(&attempt.id, "fixture-code", "wrong").await,
            Err(SubscriptionError::InvalidState)
        ));
        assert_eq!(manager.status(&attempt.id).await.unwrap().status, "pending");
    }

    #[tokio::test]
    async fn refresh_rotation_is_serialized_across_managers_and_persisted() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let app = axum::Router::new().route("/token", axum::routing::post(move || {
            let calls = server_calls.clone();
            async move {
                let index = calls.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::time::sleep(Duration::from_millis(40)).await;
                axum::Json(json!({"access_token":format!("new-access-{index}"), "refresh_token":format!("rotated-{index}"), "expires_in":3600}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let summary = store.import(json!({"type":"codex","access_token":"old","refresh_token":"old-refresh","expired":"2020-01-01T00:00:00Z"}), None).await.unwrap();
        let mut first = OAuthManager::new(reqwest::Client::new(), store.clone());
        first.codex_token_url = endpoint.clone();
        let mut second = OAuthManager::new(reqwest::Client::new(), store.clone());
        second.codex_token_url = endpoint;
        let (one, two) = tokio::join!(
            first.credential(&summary.id),
            second.credential(&summary.id)
        );
        assert_eq!(one.unwrap().access_token, "new-access-1");
        assert_eq!(two.unwrap().access_token, "new-access-1");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let (one, two) = tokio::join!(
            first.force_refresh(&summary.id),
            second.force_refresh(&summary.id)
        );
        assert_eq!(one.unwrap().access_token, "new-access-2");
        assert_eq!(two.unwrap().access_token, "new-access-2");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let reopened = AccountStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened
                .get(&summary.id)
                .await
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-2")
        );
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_revoked_refresh_token_is_not_sent_again_until_the_account_reconnects() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(move || {
                let calls = server_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({"error":"invalid_grant"})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let summary = store.import(json!({"type":"codex","access_token":"old","refresh_token":"revoked-refresh","expired":"2020-01-01T00:00:00Z"}), None).await.unwrap();
        let mut manager = OAuthManager::new(reqwest::Client::new(), store.clone());
        manager.codex_token_url = endpoint;
        for _ in 0..3 {
            assert!(matches!(
                manager.credential(&summary.id).await,
                Err(SubscriptionError::SessionRevoked)
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        store
            .modify(&summary.id, |account| {
                account.refresh_token = Some("reconnected-refresh".into())
            })
            .await
            .unwrap();
        assert!(manager.credential(&summary.id).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
        let _ = task.await;
    }

    /// A Google fixture. `/token` answers a code exchange (the access token
    /// names the code) and a refresh (`fixture-revoked` gets invalid_grant);
    /// `/userinfo` names the account after its access token; loadCodeAssist
    /// has no project, then onboarding returns `project` (or none). Every
    /// token call is recorded as "<grant> <client id> <client secret>", every
    /// loadCodeAssist call as its bearer.
    async fn google_fixture(
        project: Option<&'static str>,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Json, Router,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::{get, post},
        };
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (token_seen, load_seen) = (seen.clone(), seen.clone());
        let router = Router::new()
            .route(
                "/token",
                post(move |axum::Form(form): axum::Form<BTreeMap<String, String>>| {
                    let seen = token_seen.clone();
                    async move {
                        let grant = match form["grant_type"].as_str() {
                            "authorization_code" => "code",
                            "refresh_token" => "refresh",
                            other => panic!("unexpected grant {other}"),
                        };
                        seen.lock().unwrap().push(format!(
                            "{grant} {} {}",
                            form["client_id"], form["client_secret"]
                        ));
                        if grant == "code" {
                            let code = &form["code"];
                            return Json(json!({"access_token":format!("access-{code}"),"refresh_token":format!("refresh-{code}"),"expires_in":3600})).into_response();
                        }
                        if form["refresh_token"] == "fixture-revoked" {
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(json!({"error":"invalid_grant"})),
                            )
                                .into_response();
                        }
                        Json(json!({"access_token":"fixture-rotated","refresh_token":"fixture-rotated-refresh","expires_in":3600})).into_response()
                    }
                }),
            )
            .route(
                "/userinfo",
                get(|headers: HeaderMap| async move {
                    let bearer = headers["authorization"].to_str().unwrap();
                    let name = bearer.trim_start_matches("Bearer access-");
                    Json(json!({"email":format!("{name}@example.test"),"id":name}))
                }),
            )
            .route(
                "/v1internal:loadCodeAssist",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let seen = load_seen.clone();
                    async move {
                        let mut seen = seen.lock().unwrap();
                        seen.push(headers["authorization"].to_str().unwrap().to_owned());
                        if let Some(project) = body["cloudaicompanionProject"].as_str() {
                            seen.push(format!("project {project}"));
                        }
                        Json(json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]}))
                    }
                }),
            )
            .route(
                "/daily:onboardUser",
                post(move || async move { Json(onboarded(project)) }),
            )
            .route(
                "/v1internal:onboardUser",
                post(move || async move { Json(onboarded(project)) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (endpoint, seen, task)
    }

    /// A finished onboarding with `project`, or without any.
    fn onboarded(project: Option<&str>) -> Value {
        match project {
            Some(project) => {
                json!({"done":true,"response":{"cloudaicompanionProject":{"id":project}}})
            }
            None => json!({"done":true,"response":{"private":"fixture-private"}}),
        }
    }

    /// The token call the fixture records for a grant and client.
    fn token_call(grant: &str, id: &str, secret: &str) -> String {
        format!("{grant} {id} {secret}")
    }

    /// The Antigravity client douane is configured with in these tests, as a
    /// detected or pasted client would set it: the public id with a secret.
    const ANTIGRAVITY_TEST_SECRET: &str = "antigravity-secret";
    fn antigravity_client() -> Value {
        json!({
            "client_id": super::super::google::ANTIGRAVITY_CLIENT,
            "client_secret": ANTIGRAVITY_TEST_SECRET,
        })
    }
    fn antigravity_call(grant: &str) -> String {
        token_call(
            grant,
            super::super::google::ANTIGRAVITY_CLIENT,
            ANTIGRAVITY_TEST_SECRET,
        )
    }

    async fn google_manager(
        endpoint: &str,
        directory: &std::path::Path,
    ) -> (OAuthManager, Arc<AccountStore>) {
        let store = Arc::new(AccountStore::open(directory.join("auth")).unwrap());
        let mut manager = OAuthManager::new(reqwest::Client::new(), store.clone());
        // The developer's own `DOUANE_*` client variables never reach these tests.
        manager.google.use_test_env(|_| None);
        manager.google.use_test_endpoint(endpoint);
        (manager, store)
    }

    /// Signs in to Antigravity through the fixture; `code` names the account.
    async fn sign_in(manager: &OAuthManager, code: &str) -> Result<String, SubscriptionError> {
        let attempt = manager
            .start(SubscriptionProvider::Antigravity, None)
            .await?;
        manager
            .complete(&attempt.id, code, &attempt.state)
            .await
            .map(|summary| summary.id)
    }

    #[tokio::test]
    async fn google_sign_in_and_refresh_keep_the_client_they_started_with() {
        let (endpoint, seen, task) = google_fixture(Some("fixture-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        let custom = || json!({"client_id":"custom-id","client_secret":"custom-secret"});
        let custom_call = |grant: &str| token_call(grant, "custom-id", "custom-secret");
        let token_calls = || {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|call| !call.starts_with("Bearer"))
                .cloned()
                .collect::<Vec<_>>()
        };

        // Sign in with a configured client: the exchange records it, and the
        // account refreshes with it.
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &custom())
            .unwrap();
        let attempt = manager
            .start(SubscriptionProvider::Antigravity, None)
            .await
            .unwrap();
        assert!(attempt.url.contains("client_id=custom-id"));
        let custom_account = manager
            .complete(&attempt.id, "custom", &attempt.state)
            .await
            .unwrap()
            .id;
        assert_eq!(
            store
                .get(&custom_account)
                .await
                .unwrap()
                .client_id
                .as_deref(),
            Some("custom-id")
        );
        manager.force_refresh(&custom_account).await.unwrap();

        // Replacing the client makes that account reconnect; its last_error
        // records why.
        manager
            .set_oauth_client(
                SubscriptionProvider::Antigravity,
                &json!({"client_id":"other-id","client_secret":"other-secret"}),
            )
            .unwrap();
        let error = manager.force_refresh(&custom_account).await.err().unwrap();
        assert_eq!(
            error.to_string(),
            "this account was signed in with a Google client douane no longer has; reconnect it"
        );
        assert_eq!(
            store
                .get(&custom_account)
                .await
                .unwrap()
                .last_error
                .as_deref(),
            Some(
                "this account was signed in with a Google client douane no longer has; reconnect it"
            )
        );
        assert_eq!(token_calls(), [custom_call("code"), custom_call("refresh")]);

        // A sign-in whose client is removed before the callback fails without
        // sending the code with another client.
        let attempt = manager
            .start(SubscriptionProvider::Antigravity, None)
            .await
            .unwrap();
        assert!(attempt.url.contains("client_id=other-id"));
        manager
            .remove_oauth_client(SubscriptionProvider::Antigravity)
            .unwrap();
        let error = manager
            .complete(&attempt.id, "orphan", &attempt.state)
            .await
            .unwrap_err();
        assert!(matches!(error, SubscriptionError::OAuthClientUnavailable));
        assert_eq!(token_calls(), [custom_call("code"), custom_call("refresh")]);
        assert_eq!(store.list().await.len(), 1);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_google_sign_in_onboards_with_the_project_it_started_with() {
        let (endpoint, seen, task) = google_fixture(Some("fixture-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        let client = |project: &str| json!({"client_id":"custom-id","client_secret":"custom-secret","project_id":project});
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &client("first-project"))
            .unwrap();
        let attempt = manager
            .start(SubscriptionProvider::Antigravity, None)
            .await
            .unwrap();
        // The project changes before the callback: the sign-in keeps its own.
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &client("second-project"))
            .unwrap();
        let summary = manager
            .complete(&attempt.id, "project", &attempt.state)
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                token_call("code", "custom-id", "custom-secret"),
                "Bearer access-project".into(),
                "project first-project".into(),
            ]
        );
        assert_eq!(
            store.get(&summary.id).await.unwrap().project_id.as_deref(),
            Some("fixture-project")
        );
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_hand_edited_client_without_secret_is_a_clear_error() {
        let (endpoint, seen, task) = google_fixture(Some("fixture-project")).await;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("oauth-clients.json"),
            json!({"antigravity":{"client_id":"hand-id"}}).to_string(),
        )
        .unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        let view = &manager.oauth_clients()[0];
        assert_eq!(view.client_id.as_deref(), Some("hand-id"));
        assert!(!view.has_secret);
        assert!(!manager.supports(SubscriptionProvider::Antigravity));
        assert!(matches!(
            sign_in(&manager, "hand").await,
            Err(SubscriptionError::OAuthNotConfigured)
        ));

        // An account that names that client cannot borrow another secret.
        let hand = manager
            .import(
                json!({"type":"antigravity","email":"hand@example.test","access_token":"fixture",
                    "refresh_token":"fixture-refresh","client_id":"hand-id","project_id":"fixture-project",
                    "expired":"2099-01-01T00:00:00Z"}),
                None,
            )
            .await
            .unwrap();
        let error = manager.force_refresh(&hand.id).await.err().unwrap();
        assert!(matches!(error, SubscriptionError::OAuthClientUnavailable));
        // A CLI import pinned to the public id cannot refresh until the
        // Antigravity client is set up: no secret is borrowed, no call made.
        let mut record = antigravity_cli_record("cli@example.test");
        record["project_id"] = json!("fixture-project");
        let cli = manager.import(record, None).await.unwrap();
        let error = manager.force_refresh(&cli.id).await.err().unwrap();
        assert_eq!(
            error.to_string(),
            "set up the Antigravity sign-in client in Settings"
        );
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(store.list().await.len(), 2);
        task.abort();
        let _ = task.await;
    }

    fn antigravity_cli_record(email: &str) -> Value {
        json!({
            "token": {
                "access_token": "fixture-expired",
                "token_type": "Bearer",
                "refresh_token": "fixture-refresh",
                "expiry": "2020-01-01T00:00:00Z",
                "expires_in": 3599
            },
            "auth_method": "consumer",
            "id_token_email": email
        })
    }

    #[tokio::test]
    async fn importing_a_google_account_without_project_refreshes_and_discovers_it() {
        let (endpoint, seen, task) = google_fixture(Some("discovered-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (mut manager, store) = google_manager(&endpoint, directory.path()).await;
        // Detection or a paste has configured the Antigravity client.
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &antigravity_client())
            .unwrap();
        let summary = manager
            .import(antigravity_cli_record("fixture@example.test"), None)
            .await
            .unwrap();
        assert_eq!(summary.provider, SubscriptionProvider::Antigravity);
        assert_eq!(summary.email.as_deref(), Some("fixture@example.test"));
        let account = store.get(&summary.id).await.unwrap();
        assert_eq!(account.project_id.as_deref(), Some("discovered-project"));
        assert_eq!(account.plan.as_deref(), Some("free-tier"));
        assert_eq!(account.access_token, "fixture-rotated");
        assert_eq!(
            account.refresh_token.as_deref(),
            Some("fixture-rotated-refresh")
        );
        assert!(!account.needs_refresh());
        // The expired token is refreshed before Code Assist sees it.
        assert_eq!(
            *seen.lock().unwrap(),
            [antigravity_call("refresh"), "Bearer fixture-rotated".into()]
        );

        // A reimport of the same sign-in reuses the stored project offline.
        task.abort();
        let _ = task.await;
        manager.google.use_test_endpoint("http://127.0.0.1:9");
        let mut record = antigravity_cli_record("fixture@example.test");
        record["token"]["expiry"] = json!("2099-01-01T00:00:00Z");
        let again = manager.import(record, None).await.unwrap();
        assert_eq!(again.id, summary.id);
        assert_eq!(store.list().await.len(), 1);
        let account = store.get(&summary.id).await.unwrap();
        assert_eq!(account.project_id.as_deref(), Some("discovered-project"));

        // An explicit project skips discovery for Gemini too.
        let gemini = manager
            .import(
                json!({"type":"gemini","access_token":"fixture","project_id":"given"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&gemini.id).await.unwrap().project_id.as_deref(),
            Some("given")
        );
    }

    /// Credential files stored in `auth`.
    fn stored_files(directory: &std::path::Path) -> usize {
        std::fs::read_dir(directory.join("auth"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count()
    }

    #[tokio::test]
    async fn an_import_without_a_discoverable_project_is_stored_with_the_reason() {
        let (endpoint, _, task) = google_fixture(None).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &antigravity_client())
            .unwrap();
        let summary = manager
            .import(antigravity_cli_record("fixture@example.test"), None)
            .await
            .unwrap();
        let account = store.get(&summary.id).await.unwrap();
        assert_eq!(account.project_id, None);
        assert_eq!(
            account.last_error.as_deref(),
            Some(
                "no Cloud project was found at import: Google did not return a Cloud project for this account; check account setup in Antigravity"
            )
        );
        assert_eq!(summary.status, "error");
        // The tokens refreshed for the lookup are kept, rotated refresh token
        // included.
        assert_eq!(account.access_token, "fixture-rotated");
        assert_eq!(
            account.refresh_token.as_deref(),
            Some("fixture-rotated-refresh")
        );
        assert!(
            !serde_json::to_string(&account)
                .unwrap()
                .contains("fixture-private")
        );
        assert_eq!(stored_files(directory.path()), 1);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn an_import_is_stored_when_google_cannot_be_reached() {
        let (endpoint, seen, task) = google_fixture(Some("discovered-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (mut manager, store) = google_manager(&endpoint, directory.path()).await;
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &antigravity_client())
            .unwrap();
        // The token refresh works, then Code Assist is unreachable: the
        // account keeps the rotated tokens.
        manager
            .google
            .use_test_codeassist_endpoint("http://127.0.0.1:1");
        let summary = manager
            .import(antigravity_cli_record("first@example.test"), None)
            .await
            .unwrap();
        let account = store.get(&summary.id).await.unwrap();
        assert_eq!(account.project_id, None);
        assert!(
            account
                .last_error
                .as_deref()
                .unwrap()
                .starts_with("no Cloud project was found at import: ")
        );
        assert_eq!(
            account.refresh_token.as_deref(),
            Some("fixture-rotated-refresh")
        );
        assert_eq!(*seen.lock().unwrap(), [antigravity_call("refresh")]);
        // Google is not reached at all: the account is stored as imported.
        task.abort();
        let _ = task.await;
        manager.google.use_test_endpoint("http://127.0.0.1:1");
        let summary = manager
            .import(antigravity_cli_record("second@example.test"), None)
            .await
            .unwrap();
        let account = store.get(&summary.id).await.unwrap();
        assert_eq!(account.project_id, None);
        assert!(account.last_error.is_some());
        assert_eq!(account.access_token, "fixture-expired");
        assert_eq!(account.refresh_token.as_deref(), Some("fixture-refresh"));
        assert_eq!(stored_files(directory.path()), 2);
    }

    #[tokio::test]
    async fn an_imported_cli_record_refreshes_with_the_configured_antigravity_client() {
        let (endpoint, seen, task) = google_fixture(Some("discovered-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        // Detection or a paste has configured the Antigravity client.
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &antigravity_client())
            .unwrap();
        // Without an expiry the access token may be dead: it is refreshed
        // before discovery, with the Antigravity client the CLI signed in with.
        let mut record = antigravity_cli_record("fixture@example.test");
        record["token"].as_object_mut().unwrap().remove("expiry");
        let summary = manager.import(record, None).await.unwrap();
        let account = store.get(&summary.id).await.unwrap();
        assert_eq!(
            account.client_id.as_deref(),
            Some(super::super::google::ANTIGRAVITY_CLIENT)
        );
        assert_eq!(account.project_id.as_deref(), Some("discovered-project"));
        assert_eq!(account.access_token, "fixture-rotated");
        manager.force_refresh(&summary.id).await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                antigravity_call("refresh"),
                "Bearer fixture-rotated".into(),
                antigravity_call("refresh")
            ]
        );
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_typed_antigravity_import_refreshes_with_the_configured_antigravity_client() {
        let (endpoint, seen, task) = google_fixture(Some("discovered-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &antigravity_client())
            .unwrap();
        // CLIProxyAPI's format: typed, no client named.
        let summary = manager
            .import(
                json!({"type":"antigravity","email":"typed@example.test","access_token":"fixture",
                    "refresh_token":"fixture-refresh","project_id":"fixture-project",
                    "expired":"2099-01-01T00:00:00Z"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&summary.id).await.unwrap().client_id.as_deref(),
            Some(super::super::google::ANTIGRAVITY_CLIENT)
        );
        manager.force_refresh(&summary.id).await.unwrap();
        // An account stored before imports were pinned has no client: it uses
        // the configured Antigravity client too.
        store
            .modify(&summary.id, |account| account.client_id = None)
            .await
            .unwrap();
        manager.force_refresh(&summary.id).await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [antigravity_call("refresh"), antigravity_call("refresh")]
        );
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_gemini_import_without_a_client_is_kept_as_before_discovery() {
        let (endpoint, seen, task) = google_fixture(Some("discovered-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        assert!(!manager.supports(SubscriptionProvider::Gemini));
        // An expired token no client can refresh is stored as it is,
        // without a project and without any Google call.
        let expired = manager
            .import(
                json!({"type":"gemini","email":"expired@example.test","access_token":"fixture-expired",
                    "refresh_token":"fixture-refresh","expired":"2020-01-01T00:00:00Z"}),
                None,
            )
            .await
            .unwrap();
        let account = store.get(&expired.id).await.unwrap();
        assert_eq!(account.project_id, None);
        assert_eq!(account.access_token, "fixture-expired");
        assert!(seen.lock().unwrap().is_empty());
        // A token without a known expiry still looks its project up as it
        // is, also when the file names Gemini CLI's public client.
        for (email, client) in [
            ("unknown@example.test", None),
            (
                "public@example.test",
                Some(super::super::google::GEMINI_CLIENT),
            ),
        ] {
            let mut record = json!({"type":"gemini","email":email,"access_token":"fixture-live",
                "refresh_token":"fixture-refresh"});
            if let Some(client) = client {
                record["client_id"] = json!(client);
            }
            let summary = manager.import(record, None).await.unwrap();
            let account = store.get(&summary.id).await.unwrap();
            assert_eq!(account.project_id.as_deref(), Some("discovered-project"));
            assert_eq!(account.access_token, "fixture-live");
        }
        assert_eq!(
            *seen.lock().unwrap(),
            ["Bearer fixture-live", "Bearer fixture-live"]
        );
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn an_import_naming_an_unknown_custom_client_is_stored_and_says_why() {
        let (endpoint, seen, task) = google_fixture(Some("discovered-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        for provider in ["antigravity", "gemini"] {
            let summary = manager
                .import(
                    json!({"type":provider,"email":"custom@example.test","access_token":"fixture-expired",
                        "refresh_token":"fixture-refresh","client_id":"unknown-custom-id",
                        "expired":"2020-01-01T00:00:00Z"}),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                summary.last_error.as_deref(),
                Some(
                    "no Cloud project was found at import: the file names a Google OAuth client douane does not have; set that client in douane first, or add its client_secret to the file"
                )
            );
            let account = store.get(&summary.id).await.unwrap();
            assert_eq!(account.client_id.as_deref(), Some("unknown-custom-id"));
            assert_eq!(account.project_id, None);
        }
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(stored_files(directory.path()), 2);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn an_import_whose_refresh_is_refused_is_stored_with_the_reason() {
        let (endpoint, seen, task) = google_fixture(Some("discovered-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &antigravity_client())
            .unwrap();
        let mut record = antigravity_cli_record("fixture@example.test");
        record["token"]["refresh_token"] = json!("fixture-revoked");
        let summary = manager.import(record, None).await.unwrap();
        assert_eq!(
            summary.last_error.as_deref(),
            Some(
                "no Cloud project was found at import: the provider revoked this account's session; reconnect the account"
            )
        );
        let account = store.get(&summary.id).await.unwrap();
        assert_eq!(account.refresh_token.as_deref(), Some("fixture-revoked"));
        assert_eq!(account.project_id, None);
        // The refresh was asked once, and Code Assist never.
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(stored_files(directory.path()), 1);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn an_expired_import_without_refresh_token_is_stored_without_any_call() {
        let (endpoint, seen, task) = google_fixture(Some("discovered-project")).await;
        let directory = tempfile::tempdir().unwrap();
        let (manager, store) = google_manager(&endpoint, directory.path()).await;
        let mut record = antigravity_cli_record("fixture@example.test");
        record["token"]
            .as_object_mut()
            .unwrap()
            .remove("refresh_token");
        let summary = manager.import(record, None).await.unwrap();
        assert_eq!(summary.status, "expired");
        assert_eq!(
            summary.last_error.as_deref(),
            Some(
                "no Cloud project was found at import: the access token has expired and there is no refresh token; sign in again"
            )
        );
        assert_eq!(store.get(&summary.id).await.unwrap().project_id, None);
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(stored_files(directory.path()), 1);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn the_import_route_stores_an_account_whose_project_is_missing() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let (endpoint, _, task) = google_fixture(None).await;
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::settings::Config::default();
        config.server.auth_dir = directory.path().join("auth");
        config.server.database = directory.path().join("calls.sqlite");
        let mut state = crate::state::AppState::new(config, None).unwrap();
        let mut manager = OAuthManager::new(reqwest::Client::new(), state.accounts.clone());
        manager.google.use_test_env(|_| None);
        manager.google.use_test_endpoint(&endpoint);
        manager
            .set_oauth_client(SubscriptionProvider::Antigravity, &antigravity_client())
            .unwrap();
        Arc::get_mut(&mut state).unwrap().oauth = Arc::new(manager);
        let body = json!({"files":[{"name":"antigravity.json","content":antigravity_cli_record("fixture@example.test").to_string()}]});
        let response = crate::router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/admin/api/accounts/import")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["imported"], 1);
        assert_eq!(body["errors"], json!([]));
        assert_eq!(
            body["accounts"][0]["last_error"],
            "no Cloud project was found at import: Google did not return a Cloud project for this account; check account setup in Antigravity"
        );
        assert_eq!(stored_files(directory.path()), 1);
        task.abort();
        let _ = task.await;
    }
}
