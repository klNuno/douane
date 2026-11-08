//! Google's installed-app OAuth and Code Assist account onboarding.
//! Antigravity's public installed-app client id is the default id, but douane
//! ships no secret: a client is configured through env, `oauth-clients.json`,
//! the API, or detected from an installed Antigravity CLI.

use super::antigravity_detect::{self, DetectError};
use super::oauth::{google_verification_error, send_json};
use super::store::text;
use super::{SubscriptionError, SubscriptionProvider, timestamp};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, RwLock, TryLockError},
    time::{Duration, SystemTime},
};

const ANTIGRAVITY_VERSION: &str = "1.4.1";

/// Match the current ACP client protocol: Google gates models on this identity.
/// Keep Douane's host and platform explicit, and use it for every Code Assist call.
pub(crate) fn antigravity_user_agent() -> &'static str {
    static USER_AGENT: OnceLock<String> = OnceLock::new();
    USER_AGENT.get_or_init(|| {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            arch => arch,
        };
        format!(
            "antigravity/acp/{ANTIGRAVITY_VERSION} (aidev_client; os_type={os}; arch={arch}; host_path=douane/{}; proxy_client=antigravity/sdk)",
            env!("CARGO_PKG_VERSION")
        )
    })
}

/// Antigravity's public installed-app OAuth client id, as the Antigravity app
/// ships it. Douane keeps it as the default id, so a detected or pasted
/// Antigravity client uses exactly this id, but ships no secret of its own:
/// the secret is read from an installed Antigravity CLI (see
/// `antigravity_detect`) or pasted in Settings.
pub(crate) const ANTIGRAVITY_CLIENT: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
/// Gemini's public client id. Douane ships no Gemini secret.
pub(crate) const GEMINI_CLIENT: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";

/// Google providers with a configurable OAuth client: provider, default
/// client id and environment variable prefix. Douane ships no secret, so a
/// provider with neither an environment pair nor a file entry has no client
/// and no sign-in until one is configured.
const GOOGLE_CLIENTS: [(SubscriptionProvider, &str, &str); 2] = [
    (
        SubscriptionProvider::Antigravity,
        ANTIGRAVITY_CLIENT,
        "DOUANE_ANTIGRAVITY",
    ),
    (SubscriptionProvider::Gemini, GEMINI_CLIENT, "DOUANE_GEMINI"),
];

/// The largest `oauth-clients.json` douane reads.
const MAX_CLIENT_FILE: u64 = 65_536;

#[derive(Clone)]
struct ClientMetadata {
    id: String,
    secret: String,
}

/// Where a provider's OAuth client comes from. Environment variables win
/// over `oauth-clients.json`. Douane ships no built-in client, so a provider
/// with neither is `None` and cannot sign in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthClientSource {
    Env,
    File,
    None,
}

/// A provider's OAuth client as the management API shows it. It never
/// carries the client secret, only whether one is configured.
#[derive(Clone, Debug, Serialize)]
pub struct OAuthClientView {
    pub provider: SubscriptionProvider,
    pub source: OAuthClientSource,
    pub client_id: Option<String>,
    pub has_secret: bool,
    pub project_id: Option<String>,
    /// How a `file` client was obtained, e.g. `"antigravity-cli"` for one
    /// detected from an installed Antigravity CLI; `null` otherwise.
    pub origin: Option<String>,
    /// Set by environment variables, which the API cannot change.
    pub read_only: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthClientError {
    #[error("this provider has no configurable OAuth client")]
    Unsupported,
    #[error(
        "this OAuth client is set by {0}_* environment variables; change or unset them and restart douane"
    )]
    Environment(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("oauth-clients.json cannot be read; fix or remove it first")]
    Unreadable,
    #[error("cannot save the OAuth client configuration")]
    Storage,
}

/// The Google client and Cloud project a sign-in started with. Its code
/// exchange uses both, even if the configuration changed meanwhile.
#[derive(Clone, Debug)]
pub(crate) struct GoogleSignIn {
    pub(crate) client_id: String,
    pub(crate) project_id: Option<String>,
}

#[derive(Default)]
struct Clients {
    configured: BTreeMap<SubscriptionProvider, ClientMetadata>,
    views: Vec<OAuthClientView>,
    /// The state of `oauth-clients.json` when it was last read.
    stamp: FileStamp,
}

/// What tells a changed `oauth-clients.json` apart cheaply: its
/// modification time and length, or None when it is absent or cannot be
/// inspected.
type FileStamp = Option<(Option<SystemTime>, u64)>;

fn file_stamp(path: &Path) -> FileStamp {
    std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.modified().ok(), metadata.len()))
}

/// Resolves every Google client from `oauth-clients.json` and the
/// environment. The client id and secret always come together from one
/// source: the `_CLIENT_ID`/`_CLIENT_SECRET` variables, else the file entry.
/// A source that names no id uses the provider's public id. Douane ships no
/// secret, so a provider with neither an environment pair nor a file entry has
/// no client. The project is separate: `_PROJECT_ID` overrides it without
/// making the client read-only.
fn resolve_clients(file: &Value, env: &dyn Fn(&str) -> Option<String>) -> Clients {
    let mut clients = Clients::default();
    for (provider, default_id, prefix) in GOOGLE_CLIENTS {
        let var =
            |name: &str| env(&format!("{prefix}_{name}")).filter(|value| !value.trim().is_empty());
        let config = file.get(provider.as_str()).unwrap_or(&Value::Null);
        let (env_id, env_secret) = (var("CLIENT_ID"), var("CLIENT_SECRET"));
        let (file_id, file_secret) = (
            text(config, &["client_id"]),
            text(config, &["client_secret"]),
        );
        let (source, id, secret) = if env_id.is_some() || env_secret.is_some() {
            (OAuthClientSource::Env, env_id, env_secret)
        } else if file_id.is_some() || file_secret.is_some() {
            (OAuthClientSource::File, file_id, file_secret)
        } else {
            (OAuthClientSource::None, None, None)
        };
        let id = id.unwrap_or_else(|| default_id.into());
        let project = var("PROJECT_ID").or_else(|| text(config, &["project_id"]));
        // Only a file client records how it was obtained (e.g. the CLI).
        let origin = (source == OAuthClientSource::File)
            .then(|| text(config, &["origin"]))
            .flatten();
        clients.views.push(OAuthClientView {
            provider,
            source,
            client_id: (source != OAuthClientSource::None).then(|| id.clone()),
            has_secret: secret.is_some(),
            project_id: project,
            origin,
            read_only: source == OAuthClientSource::Env,
        });
        if let Some(secret) = secret {
            clients
                .configured
                .insert(provider, ClientMetadata { id, secret });
        }
    }
    clients
}

/// The environment as `resolve_clients` reads it.
fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The environment variable prefix of a provider with a configurable
/// client; any other provider has no client to change.
fn env_prefix(provider: SubscriptionProvider) -> Result<&'static str, OAuthClientError> {
    GOOGLE_CLIENTS
        .iter()
        .find(|client| client.0 == provider)
        .map(|client| client.2)
        .ok_or(OAuthClientError::Unsupported)
}

/// Whether `id` is the provider's public client id: Antigravity's public
/// client or Gemini CLI's, rather than a custom client.
pub(crate) fn is_public_client(provider: SubscriptionProvider, id: &str) -> bool {
    GOOGLE_CLIENTS
        .iter()
        .any(|client| client.0 == provider && client.1 == id)
}

/// Reads `oauth-clients.json`: an absent file is an empty configuration.
/// The error says why the file was refused, never what it contains.
fn read_client_file(path: &Path) -> Result<Value, &'static str> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(_) => return Err("its metadata cannot be read"),
        Ok(metadata) if !metadata.is_file() => return Err("it is not a regular file"),
        Ok(metadata) if metadata.len() >= MAX_CLIENT_FILE => return Err("it is 64 KiB or larger"),
        Ok(_) => {}
    }
    let bytes = std::fs::read(path).map_err(|_| "it cannot be read")?;
    let value = serde_json::from_slice::<Value>(&bytes).map_err(|_| "it is not valid JSON")?;
    if value.is_object() {
        Ok(value)
    } else {
        Err("it is not a JSON object")
    }
}

/// Reports an `oauth-clients.json` douane refused: its path and why, never
/// its contents.
fn warn_unreadable(path: &Path, reason: &str) {
    tracing::warn!(
        path = %path.display(),
        reason,
        "oauth-clients.json is ignored; Google sign-in clients keep their previous configuration"
    );
}

/// An optional request field: absent or null is None, otherwise a trimmed
/// single-line string of at most `max` bytes.
fn client_field(
    body: &Value,
    key: &str,
    max: usize,
    message: &'static str,
) -> Result<Option<String>, OAuthClientError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
                Err(OAuthClientError::Invalid(message))
            } else {
                Ok(Some(value.to_owned()))
            }
        }
        Some(_) => Err(OAuthClientError::Invalid(message)),
    }
}

/// Reads one environment variable; tests replace the process environment.
type Environment = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub(crate) struct GoogleOAuth {
    client: reqwest::Client,
    path: PathBuf,
    env: Environment,
    clients: RwLock<Clients>,
    /// Serializes changes to `oauth-clients.json`.
    write: Mutex<()>,
    token_url: String,
    userinfo_url: String,
    codeassist_url: String,
    daily_url: String,
    poll_delay: Duration,
}

impl GoogleOAuth {
    pub(crate) fn load(client: reqwest::Client, path: &Path) -> Self {
        // Inspected before it is read: a change in between is read again.
        let stamp = file_stamp(path);
        let file = read_client_file(path).unwrap_or_else(|reason| {
            warn_unreadable(path, reason);
            Value::Null
        });
        let mut clients = resolve_clients(&file, &process_env);
        clients.stamp = stamp;
        Self {
            client,
            path: path.to_owned(),
            env: Box::new(process_env),
            clients: RwLock::new(clients),
            write: Mutex::new(()),
            token_url: "https://oauth2.googleapis.com/token".into(),
            userinfo_url: "https://www.googleapis.com/oauth2/v2/userinfo?alt=json".into(),
            codeassist_url: "https://cloudcode-pa.googleapis.com/v1internal".into(),
            daily_url: "https://daily-cloudcode-pa.googleapis.com/v1internal".into(),
            poll_delay: Duration::from_secs(5),
        }
    }

    fn metadata(&self, provider: SubscriptionProvider) -> Option<ClientMetadata> {
        self.sync();
        self.clients
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .configured
            .get(&provider)
            .cloned()
    }

    pub(crate) fn supports(&self, provider: SubscriptionProvider) -> bool {
        self.sync();
        self.clients
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .configured
            .contains_key(&provider)
    }

    /// The Cloud project configured for a provider, if any.
    pub(crate) fn configured_project(&self, provider: SubscriptionProvider) -> Option<String> {
        self.sync();
        self.view(provider).ok().and_then(|view| view.project_id)
    }

    /// Applies a hand edit of `oauth-clients.json` before the clients are
    /// used, so sign-in, refreshes and the provider list follow the same
    /// file as `GET /oauth-clients`. Only the file's modification time and
    /// length are checked when it has not changed. A change through the API
    /// in progress applies the file itself, so it is not waited for.
    fn sync(&self) {
        let stamp = file_stamp(&self.path);
        if self.clients.read().unwrap_or_else(|e| e.into_inner()).stamp == stamp {
            return;
        }
        let _write = match self.write.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        self.reread(stamp);
    }

    /// Reads `oauth-clients.json` again and applies it; the caller holds
    /// `write`. A file that cannot be read is reported and changes nothing
    /// but the stamp, so it is reported once per change.
    fn reread(&self, stamp: FileStamp) {
        match read_client_file(&self.path) {
            Ok(file) => self.apply(&file, stamp),
            Err(reason) => {
                warn_unreadable(&self.path, reason);
                self.clients
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .stamp = stamp;
            }
        }
    }

    pub(crate) fn client_views(&self) -> Vec<OAuthClientView> {
        self.clients
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .views
            .clone()
    }

    /// Re-reads `oauth-clients.json` and the environment, applies them and
    /// returns the clients, so hand edits to the file show and take effect.
    /// A file that cannot be read is reported and changes nothing.
    pub(crate) fn reload_views(&self) -> Vec<OAuthClientView> {
        let _write = self.write.lock().unwrap_or_else(|e| e.into_inner());
        self.reread(file_stamp(&self.path));
        self.client_views()
    }

    fn view(&self, provider: SubscriptionProvider) -> Result<OAuthClientView, OAuthClientError> {
        self.client_views()
            .into_iter()
            .find(|view| view.provider == provider)
            .ok_or(OAuthClientError::Unsupported)
    }

    /// Uses `file`, read when `oauth-clients.json` had `stamp`.
    fn apply(&self, file: &Value, stamp: FileStamp) {
        let mut clients = resolve_clients(file, &self.env);
        clients.stamp = stamp;
        *self.clients.write().unwrap_or_else(|e| e.into_inner()) = clients;
    }

    /// Re-reads `oauth-clients.json` for a change, refusing a change to a
    /// client the environment sets. Nothing live changes here, so a failure
    /// leaves the clients as they were.
    fn writable(&self, provider: SubscriptionProvider) -> Result<Value, OAuthClientError> {
        let prefix = env_prefix(provider)?;
        let file = read_client_file(&self.path).map_err(|reason| {
            warn_unreadable(&self.path, reason);
            OAuthClientError::Unreadable
        })?;
        if resolve_clients(&file, &self.env)
            .views
            .iter()
            .any(|view| view.provider == provider && view.read_only)
        {
            return Err(OAuthClientError::Environment(prefix));
        }
        Ok(file)
    }

    /// Writes the file, then applies it: a failed write changes nothing.
    fn save(&self, file: &Value) -> Result<(), OAuthClientError> {
        if file.as_object().is_some_and(serde_json::Map::is_empty) {
            match std::fs::remove_file(&self.path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(OAuthClientError::Storage);
                }
                _ => {}
            }
        } else {
            let bytes = serde_json::to_vec_pretty(file).map_err(|_| OAuthClientError::Storage)?;
            super::store::write_private_file(&self.path, &bytes)
                .map_err(|_| OAuthClientError::Storage)?;
        }
        self.apply(file, file_stamp(&self.path));
        Ok(())
    }

    /// Stores a custom client for one provider in `oauth-clients.json` and
    /// uses it at once. `client_secret` may be omitted to keep the stored
    /// secret of the same client id.
    pub(crate) fn set_client(
        &self,
        provider: SubscriptionProvider,
        body: &Value,
    ) -> Result<OAuthClientView, OAuthClientError> {
        // A provider without a client is refused before its body is read,
        // as for an unknown provider.
        env_prefix(provider)?;
        if !body.is_object() {
            return Err(OAuthClientError::Invalid(
                "request body must be a JSON object",
            ));
        }
        let id = client_field(
            body,
            "client_id",
            512,
            "client_id must be a non-empty single-line string of at most 512 bytes",
        )?
        .ok_or(OAuthClientError::Invalid("client_id is required"))?;
        let secret = client_field(
            body,
            "client_secret",
            512,
            "client_secret must be a non-empty single-line string of at most 512 bytes",
        )?;
        let project = client_field(
            body,
            "project_id",
            256,
            "project_id must be a non-empty single-line string of at most 256 bytes, or null",
        )?;
        // How the client was obtained (e.g. "antigravity-cli"); internal.
        let origin = client_field(
            body,
            "origin",
            64,
            "origin must be a non-empty single-line string of at most 64 bytes",
        )?;
        let _write = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.writable(provider)?;
        let stored = file.get(provider.as_str()).unwrap_or(&Value::Null);
        let secret = match secret {
            Some(secret) => secret,
            None => text(stored, &["client_secret"])
                .filter(|_| text(stored, &["client_id"]).as_deref() == Some(id.as_str()))
                .ok_or(OAuthClientError::Invalid("client_secret is required"))?,
        };
        let mut entry = json!({"client_id": id, "client_secret": secret});
        if let Some(project) = project {
            entry["project_id"] = json!(project);
        }
        if let Some(origin) = origin {
            entry["origin"] = json!(origin);
        }
        file[provider.as_str()] = entry;
        self.save(&file)?;
        self.view(provider)
    }

    /// Removes a provider's custom client: it falls back to none, so sign-in
    /// is unavailable until a client is configured again.
    pub(crate) fn remove_client(
        &self,
        provider: SubscriptionProvider,
    ) -> Result<OAuthClientView, OAuthClientError> {
        let _write = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.writable(provider)?;
        if file
            .as_object_mut()
            .is_some_and(|object| object.remove(provider.as_str()).is_some())
        {
            self.save(&file)?;
        } else {
            self.apply(&file, file_stamp(&self.path));
        }
        self.view(provider)
    }

    /// The client id and secret for an account or a sign-in started with
    /// `client_id`: the configured client when its id matches. Douane ships no
    /// built-in client, so an account pinned to a client douane does not have
    /// cannot refresh: an Antigravity account pinned to the public id asks the
    /// operator to set up the Antigravity client, any other asks to reconnect.
    /// Without a recorded id (older imports) the configured client is used,
    /// when there is one; otherwise the sign-in client is not configured.
    fn client_pair(
        &self,
        provider: SubscriptionProvider,
        client_id: Option<&str>,
    ) -> Result<(String, String), SubscriptionError> {
        let current = self.metadata(provider);
        match client_id {
            Some(id) => match current {
                Some(metadata) if metadata.id == id => Ok((metadata.id, metadata.secret)),
                _ if provider == SubscriptionProvider::Antigravity
                    && is_public_client(provider, id) =>
                {
                    Err(SubscriptionError::OAuthClientNotSetUp)
                }
                _ => Err(SubscriptionError::OAuthClientUnavailable),
            },
            None => current
                .map(|metadata| (metadata.id, metadata.secret))
                .ok_or(SubscriptionError::OAuthNotConfigured),
        }
    }

    /// Detects an Antigravity CLI on this machine, validates its embedded
    /// client secret against Google, and saves the matching client through the
    /// normal set-client path (atomic write, hardened permissions, live
    /// reload). The scan runs on the blocking pool; validation and the save
    /// follow. The secret is never logged or returned.
    pub(crate) async fn detect_antigravity(
        &self,
        explicit: Option<String>,
    ) -> Result<OAuthClientView, DetectError> {
        // A client the environment sets cannot be changed: fail before the scan.
        if self
            .view(SubscriptionProvider::Antigravity)
            .map(|view| view.read_only)
            .unwrap_or(false)
        {
            return Err(DetectError::EnvManaged);
        }
        let path = match explicit.map(|path| path.trim().to_owned()) {
            Some(path) if !path.is_empty() => {
                let path = PathBuf::from(path);
                if !path.is_file() {
                    return Err(DetectError::NotFound);
                }
                path
            }
            _ => antigravity_detect::default_install_path().ok_or(DetectError::NotFound)?,
        };
        let scan = tokio::task::spawn_blocking(move || {
            antigravity_detect::scan_file(&path, ANTIGRAVITY_CLIENT)
        })
        .await
        .map_err(|_| DetectError::Storage)??;
        if !scan.client_id_present {
            return Err(DetectError::NoMatchingClient);
        }
        for candidate in scan.candidates {
            if self.validate_antigravity_secret(&candidate).await? {
                let body = json!({
                    "client_id": ANTIGRAVITY_CLIENT,
                    "client_secret": candidate,
                    "origin": "antigravity-cli",
                });
                return self
                    .set_client(SubscriptionProvider::Antigravity, &body)
                    .map_err(|error| match error {
                        OAuthClientError::Environment(_) => DetectError::EnvManaged,
                        _ => DetectError::Storage,
                    });
            }
        }
        Err(DetectError::NoMatchingClient)
    }

    /// True when `candidate` is the Antigravity client's secret: a refresh with
    /// a deliberately invalid refresh token answers `invalid_grant` for the
    /// right secret and `invalid_client` for a wrong one. Never logs the value.
    async fn validate_antigravity_secret(&self, candidate: &str) -> Result<bool, DetectError> {
        let response = self
            .client
            .post(&self.token_url)
            .form(&[
                ("client_id", ANTIGRAVITY_CLIENT),
                ("client_secret", candidate),
                ("grant_type", "refresh_token"),
                ("refresh_token", "1//invalid"),
            ])
            .send()
            .await
            .map_err(|_| DetectError::GoogleUnreachable)?;
        let body: Value = response
            .json()
            .await
            .map_err(|_| DetectError::GoogleUnreachable)?;
        Ok(text(&body, &["error"]).as_deref() == Some("invalid_grant"))
    }

    /// Replaces the process environment, so tests do not depend on the
    /// developer's own `DOUANE_*` variables, and resolves the clients again.
    #[cfg(test)]
    pub(crate) fn use_test_env(
        &mut self,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) {
        self.env = Box::new(env);
        let stamp = file_stamp(&self.path);
        let file = read_client_file(&self.path).unwrap_or(Value::Null);
        self.apply(&file, stamp);
    }

    /// Sends every Google call to one loopback fixture, without polling delay.
    #[cfg(test)]
    pub(crate) fn use_test_endpoint(&mut self, endpoint: &str) {
        self.token_url = format!("{endpoint}/token");
        self.userinfo_url = format!("{endpoint}/userinfo");
        self.codeassist_url = format!("{endpoint}/v1internal");
        self.daily_url = format!("{endpoint}/daily");
        self.poll_delay = Duration::ZERO;
    }

    /// The Google sign-in URL, and the client and project it starts with;
    /// the code exchange must use those same ones.
    pub(crate) fn authorize(
        &self,
        provider: SubscriptionProvider,
        state: &str,
        redirect: &str,
        challenge: &str,
    ) -> Result<(String, GoogleSignIn), SubscriptionError> {
        self.sync();
        // Read the client and its project together, from one configuration.
        let (metadata, project_id) = {
            let clients = self.clients.read().unwrap_or_else(|e| e.into_inner());
            let metadata = clients
                .configured
                .get(&provider)
                .cloned()
                .ok_or(SubscriptionError::OAuthNotConfigured)?;
            let project = clients
                .views
                .iter()
                .find(|view| view.provider == provider)
                .and_then(|view| view.project_id.clone());
            (metadata, project)
        };
        let mut scopes = vec![
            "https://www.googleapis.com/auth/cloud-platform",
            "https://www.googleapis.com/auth/userinfo.email",
            "https://www.googleapis.com/auth/userinfo.profile",
        ];
        if provider == SubscriptionProvider::Antigravity {
            scopes.extend([
                "https://www.googleapis.com/auth/cclog",
                "https://www.googleapis.com/auth/experimentsandconfigs",
            ]);
        }
        let scopes = scopes.join(" ");
        let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
            .map_err(|_| SubscriptionError::InvalidResponse)?;
        url.query_pairs_mut().extend_pairs([
            ("client_id", metadata.id.as_str()),
            ("redirect_uri", redirect),
            ("response_type", "code"),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("scope", scopes.as_str()),
            ("state", state),
        ]);
        if provider == SubscriptionProvider::Gemini
            && redirect == "https://codeassist.google.com/authcode"
        {
            url.query_pairs_mut().extend_pairs([
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
            ]);
        }
        Ok((
            url.into(),
            GoogleSignIn {
                client_id: metadata.id,
                project_id,
            },
        ))
    }

    /// Exchanges a sign-in code with the client, and onboards with the
    /// project, the sign-in was started with, even if the configuration
    /// changed since. The account records that client id, so its refreshes
    /// use the same client.
    pub(crate) async fn exchange(
        &self,
        provider: SubscriptionProvider,
        sign_in: &GoogleSignIn,
        code: &str,
        redirect: &str,
        verifier: &str,
    ) -> Result<Value, SubscriptionError> {
        let (id, secret) = self.client_pair(provider, Some(&sign_in.client_id))?;
        let project = sign_in.project_id.clone();
        let mut form = vec![
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
        ];
        if provider == SubscriptionProvider::Gemini
            && redirect == "https://codeassist.google.com/authcode"
        {
            form.push(("code_verifier", verifier));
        }
        let mut tokens = send_json(self.client.post(&self.token_url).form(&form)).await?;
        let access = text(&tokens, &["access_token"]).ok_or(SubscriptionError::InvalidResponse)?;
        let profile = send_json(self.client.get(&self.userinfo_url).bearer_auth(&access)).await?;
        let email = text(&profile, &["email"]).ok_or(SubscriptionError::InvalidResponse)?;
        let (project, tier) = self.onboard(provider, &access, project.as_deref()).await?;
        let expires = tokens
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|seconds| *seconds > 0)
            .map(|seconds| {
                timestamp(
                    time::OffsetDateTime::now_utc()
                        + time::Duration::seconds(seconds.min(31_536_000)),
                )
            });
        let object = tokens
            .as_object_mut()
            .ok_or(SubscriptionError::InvalidResponse)?;
        object.insert("type".into(), json!(provider.as_str()));
        object.insert("email".into(), json!(email));
        object.insert(
            "account_id".into(),
            profile.get("id").cloned().unwrap_or_else(|| json!(email)),
        );
        object.insert("project_id".into(), json!(project));
        object.insert("client_id".into(), json!(id));
        if let Some(tier) = tier {
            object.insert("plan".into(), json!(tier));
        }
        if let Some(expires) = expires {
            object.insert("expired".into(), json!(expires));
        }
        Ok(tokens)
    }

    /// A token refresh for an account. A stored secret goes with the stored
    /// client id only; without one, the account's client id picks the
    /// secret (see `client_pair`).
    pub(crate) fn refresh_request(
        &self,
        provider: SubscriptionProvider,
        refresh: &str,
        stored_id: Option<&str>,
        stored_secret: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, SubscriptionError> {
        let (id, secret) = match stored_secret {
            Some(secret) => (
                stored_id
                    .ok_or(SubscriptionError::InvalidImport(
                        "a Google client secret is stored without its client id",
                    ))?
                    .to_owned(),
                secret.to_owned(),
            ),
            None => self.client_pair(provider, stored_id)?,
        };
        Ok(self.client.post(&self.token_url).form(&[
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ]))
    }

    pub(crate) async fn onboard(
        &self,
        provider: SubscriptionProvider,
        access: &str,
        project: Option<&str>,
    ) -> Result<(String, Option<String>), SubscriptionError> {
        let metadata = if provider == SubscriptionProvider::Antigravity {
            json!({"ideType":"ANTIGRAVITY"})
        } else {
            json!({"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"})
        };
        let mut body = json!({"metadata":metadata});
        if let Some(project) = project {
            body["cloudaicompanionProject"] = json!(project);
        }
        let mut request = self
            .client
            .post(format!("{}:loadCodeAssist", self.codeassist_url))
            .bearer_auth(access)
            .json(&body);
        if provider == SubscriptionProvider::Antigravity {
            request = request.header("User-Agent", antigravity_user_agent());
        } else {
            request = request.header("User-Agent", crate::relay::GEMINI_USER_AGENT);
        }
        let loaded = send_json(request).await?;
        if let Some(error) = load_verification_error(&loaded) {
            return Err(error);
        }
        let tier = loaded
            .get("paidTier")
            .and_then(|tier| text(tier, &["id"]))
            .or_else(|| {
                loaded
                    .get("currentTier")
                    .and_then(|tier| text(tier, &["id"]))
            });
        if let Some(project) = project_id(&loaded) {
            return Ok((project, tier));
        }
        if provider == SubscriptionProvider::Gemini
            && loaded.get("currentTier").is_some_and(Value::is_object)
        {
            return project.map(|project| (project.to_owned(), tier)).ok_or(
                SubscriptionError::InvalidImport(
                    "Google account requires a configured Cloud project",
                ),
            );
        }
        // Subscription labels are separate from the activation tier sent upstream.
        let selected = loaded
            .get("allowedTiers")
            .and_then(Value::as_array)
            .and_then(|tiers| tiers.iter().find(|tier| tier["isDefault"] == true))
            .and_then(|tier| text(tier, &["id"]))
            .or_else(|| {
                if provider == SubscriptionProvider::Antigravity {
                    loaded
                        .get("currentTier")
                        .and_then(|tier| text(tier, &["id"]))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                if provider == SubscriptionProvider::Antigravity {
                    "free-tier".into()
                } else {
                    "legacy-tier".into()
                }
            });
        if provider == SubscriptionProvider::Gemini && selected != "free-tier" && project.is_none()
        {
            return Err(SubscriptionError::InvalidImport(
                "Google account requires a configured Cloud project",
            ));
        }
        let (endpoint, body) = if provider == SubscriptionProvider::Antigravity {
            (
                format!("{}:onboardUser", self.daily_url),
                json!({"tier_id":selected,"metadata":{"ide_type":"ANTIGRAVITY","ide_name":"antigravity","ide_version":ANTIGRAVITY_VERSION}}),
            )
        } else {
            let mut body = json!({"tierId":selected,"metadata":metadata});
            if selected != "free-tier" {
                body["cloudaicompanionProject"] = json!(project);
                body["metadata"]["duetProject"] = json!(project);
            }
            (format!("{}:onboardUser", self.codeassist_url), body)
        };
        let onboard_request = || {
            let mut request = self.client.post(&endpoint).bearer_auth(access).json(&body);
            if provider == SubscriptionProvider::Antigravity {
                request = request
                    .header("User-Agent", antigravity_user_agent())
                    .header("X-Goog-Api-Client", "gl-node/22.21.1");
            } else {
                request = request.header("User-Agent", crate::relay::GEMINI_USER_AGENT);
            }
            request
        };
        let mut operation = send_json(onboard_request()).await?;
        for attempt in 0..12 {
            if operation["done"] == true {
                if let Some(error) = completed_onboarding_error(&operation, &endpoint) {
                    return Err(error);
                }
                let Some(project) = operation.get("response").and_then(project_id) else {
                    // Never log the operation, profile, project, or upstream error text.
                    tracing::warn!(
                        provider = provider.as_str(),
                        activation_tier = activation_tier_category(&selected),
                        current_tier_present = loaded
                            .get("currentTier")
                            .is_some_and(serde_json::Value::is_object),
                        paid_tier_present = loaded
                            .get("paidTier")
                            .is_some_and(serde_json::Value::is_object),
                        allowed_tier_count = loaded
                            .get("allowedTiers")
                            .and_then(serde_json::Value::as_array)
                            .map_or(0, Vec::len),
                        ineligible_tier_count = loaded
                            .get("ineligibleTiers")
                            .and_then(serde_json::Value::as_array)
                            .map_or(0, Vec::len),
                        ineligible_reasons = ?ineligible_reason_categories(&loaded),
                        onboarding_status = diagnostic_enum(
                            operation.pointer("/response/status/statusCode"),
                            &["DEFAULT", "NOTICE", "WARNING"]
                        ),
                        help_link_present = operation.pointer("/response/status/helpLink/url").is_some(),
                        response_present = operation
                            .get("response")
                            .is_some_and(serde_json::Value::is_object),
                        "Google onboarding completed without a Cloud project"
                    );
                    return Err(SubscriptionError::GoogleProjectUnavailable);
                };
                return Ok((project, tier.or(Some(selected))));
            }
            if attempt == 11 {
                break;
            }
            tokio::time::sleep(self.poll_delay).await;
            if provider == SubscriptionProvider::Antigravity {
                operation = send_json(onboard_request()).await?;
            } else {
                let name = text(&operation, &["name"]).ok_or(SubscriptionError::InvalidResponse)?;
                if !name.starts_with("operations/")
                    || name.contains("..")
                    || name.contains('?')
                    || name.contains('#')
                {
                    return Err(SubscriptionError::InvalidResponse);
                }
                operation = send_json(
                    self.client
                        .get(format!("{}/{name}", self.codeassist_url))
                        .bearer_auth(access),
                )
                .await?;
            }
        }
        Err(SubscriptionError::InvalidImport(
            "Google account onboarding did not finish",
        ))
    }
}

fn activation_tier_category(tier: &str) -> &'static str {
    match tier {
        "free-tier" => "free-tier",
        "standard-tier" => "standard-tier",
        "legacy-tier" => "legacy-tier",
        _ => "other",
    }
}

fn ineligible_reason_categories(loaded: &Value) -> Vec<&'static str> {
    const REASONS: &[&str] = &[
        "UNKNOWN",
        "INELIGIBLE_ACCOUNT",
        "UNKNOWN_LOCATION",
        "UNSUPPORTED_LOCATION",
        "RESTRICTED_NETWORK",
        "RESTRICTED_AGE",
        "NON_USER_ACCOUNT",
        "DASHER_USER",
        "BYOID_USER",
        "RESTRICTED_DASHER_USER",
        "VALIDATION_REQUIRED",
        "GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT",
        "UNSUPPORTED_CLIENT",
    ];
    loaded
        .get("ineligibleTiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(16)
        .map(|tier| diagnostic_enum(tier.get("reasonCode"), REASONS))
        .collect()
}

fn diagnostic_enum(value: Option<&Value>, known: &'static [&'static str]) -> &'static str {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return "absent";
    };
    if let Some(code) = value.as_str() {
        known
            .iter()
            .copied()
            .find(|&known| known == code)
            .unwrap_or("other")
    } else {
        value
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| known.get(index).copied())
            .unwrap_or("other")
    }
}

fn load_verification_error(value: &Value) -> Option<SubscriptionError> {
    if value.get("currentTier").is_some_and(Value::is_object) {
        return None;
    }
    let blocked = value
        .get("ineligibleTiers")
        .and_then(Value::as_array)?
        .iter()
        .find(|tier| tier["reasonCode"] == "VALIDATION_REQUIRED")?;
    Some(SubscriptionError::verification_required(
        blocked.get("validationUrl").and_then(Value::as_str),
    ))
}

fn completed_onboarding_error(value: &Value, endpoint: &str) -> Option<SubscriptionError> {
    let error = value.get("error").filter(|error| !error.is_null())?;
    // Long-running operations use google.rpc.Status (7 = PERMISSION_DENIED).
    // Some HTTP adapters retain its corresponding HTTP status code instead.
    if matches!(error.get("code").and_then(Value::as_u64), Some(7 | 403)) {
        if let (Ok(source), Ok(body)) = (
            reqwest::Url::parse(endpoint),
            serde_json::to_vec(&json!({"error": error})),
        ) {
            if let Some(error) =
                google_verification_error(reqwest::StatusCode::FORBIDDEN, &source, &body)
            {
                return Some(error);
            }
        }
    }
    Some(SubscriptionError::InvalidImport(
        "Google account onboarding failed",
    ))
}

fn project_id(value: &Value) -> Option<String> {
    ["cloudaicompanionProject", "projectId", "project"]
        .into_iter()
        .find_map(|key| {
            value.get(key).and_then(|project| {
                project
                    .as_str()
                    .filter(|project| !project.trim().is_empty())
                    .map(str::to_owned)
                    .or_else(|| text(project, &["id"]))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    impl GoogleOAuth {
        fn insert_client(&mut self, provider: SubscriptionProvider, metadata: ClientMetadata) {
            self.clients
                .get_mut()
                .unwrap()
                .configured
                .insert(provider, metadata);
        }
    }

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: BTreeMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name| pairs.get(name).cloned()
    }

    fn view(clients: &Clients, provider: SubscriptionProvider) -> OAuthClientView {
        clients
            .views
            .iter()
            .find(|view| view.provider == provider)
            .unwrap()
            .clone()
    }

    #[test]
    fn neither_google_provider_has_a_client_by_default() {
        let clients = resolve_clients(&Value::Null, &fake_env(&[]));
        for provider in [
            SubscriptionProvider::Antigravity,
            SubscriptionProvider::Gemini,
        ] {
            let view = view(&clients, provider);
            assert_eq!(view.source, OAuthClientSource::None);
            assert_eq!(view.client_id, None);
            assert!(!view.has_secret);
            assert!(view.origin.is_none());
            assert!(!clients.configured.contains_key(&provider));
        }
    }

    #[test]
    fn client_id_and_secret_come_together_from_env_then_file() {
        let file = json!({
            "antigravity": {"client_id":"file-id","client_secret":"file-secret","project_id":"file-project"},
            "gemini": {"client_id":"file-gemini","client_secret":"file-gemini-secret"}
        });
        let clients = resolve_clients(&file, &fake_env(&[]));
        let antigravity = view(&clients, SubscriptionProvider::Antigravity);
        assert_eq!(antigravity.source, OAuthClientSource::File);
        assert_eq!(antigravity.client_id.as_deref(), Some("file-id"));
        assert_eq!(antigravity.project_id.as_deref(), Some("file-project"));
        assert_eq!(
            clients.configured[&SubscriptionProvider::Antigravity].secret,
            "file-secret"
        );
        assert_eq!(
            view(&clients, SubscriptionProvider::Gemini).source,
            OAuthClientSource::File
        );

        // An environment pair replaces the file pair whole.
        let clients = resolve_clients(
            &file,
            &fake_env(&[
                ("DOUANE_ANTIGRAVITY_CLIENT_ID", "env-id"),
                ("DOUANE_ANTIGRAVITY_CLIENT_SECRET", "env-secret"),
            ]),
        );
        let antigravity = view(&clients, SubscriptionProvider::Antigravity);
        assert_eq!(antigravity.source, OAuthClientSource::Env);
        assert!(antigravity.read_only);
        assert_eq!(antigravity.client_id.as_deref(), Some("env-id"));
        assert_eq!(antigravity.project_id.as_deref(), Some("file-project"));
        let metadata = &clients.configured[&SubscriptionProvider::Antigravity];
        assert_eq!(
            (metadata.id.as_str(), metadata.secret.as_str()),
            ("env-id", "env-secret")
        );

        // An environment secret alone never pairs with the file's id: it
        // goes with the provider's public id.
        let clients = resolve_clients(
            &file,
            &fake_env(&[("DOUANE_GEMINI_CLIENT_SECRET", "env-gemini-secret")]),
        );
        let gemini = view(&clients, SubscriptionProvider::Gemini);
        assert_eq!(gemini.source, OAuthClientSource::Env);
        assert_eq!(gemini.client_id.as_deref(), Some(GEMINI_CLIENT));
        let metadata = &clients.configured[&SubscriptionProvider::Gemini];
        assert_eq!(
            (metadata.id.as_str(), metadata.secret.as_str()),
            (GEMINI_CLIENT, "env-gemini-secret")
        );

        // An environment id alone never borrows the file's secret.
        let clients = resolve_clients(
            &file,
            &fake_env(&[("DOUANE_ANTIGRAVITY_CLIENT_ID", "env-id")]),
        );
        let antigravity = view(&clients, SubscriptionProvider::Antigravity);
        assert_eq!(antigravity.source, OAuthClientSource::Env);
        assert!(!antigravity.has_secret);
        assert!(
            !clients
                .configured
                .contains_key(&SubscriptionProvider::Antigravity)
        );

        // A project alone overrides only the project: the client keeps its
        // source and stays editable.
        let clients = resolve_clients(
            &json!({"gemini": {"client_id":"file-gemini","client_secret":"file-gemini-secret"}}),
            &fake_env(&[
                ("DOUANE_ANTIGRAVITY_PROJECT_ID", "env-project"),
                ("DOUANE_GEMINI_PROJECT_ID", "env-gemini-project"),
            ]),
        );
        let antigravity = view(&clients, SubscriptionProvider::Antigravity);
        assert_eq!(antigravity.source, OAuthClientSource::None);
        assert!(!antigravity.read_only);
        assert_eq!(antigravity.project_id.as_deref(), Some("env-project"));
        let gemini = view(&clients, SubscriptionProvider::Gemini);
        assert_eq!(gemini.source, OAuthClientSource::File);
        assert!(!gemini.read_only);
        assert_eq!(gemini.client_id.as_deref(), Some("file-gemini"));
        assert_eq!(gemini.project_id.as_deref(), Some("env-gemini-project"));

        // A file client id without a secret has no secret: douane ships none.
        let clients = resolve_clients(
            &json!({"antigravity":{"client_id":"other"}}),
            &fake_env(&[]),
        );
        let antigravity = view(&clients, SubscriptionProvider::Antigravity);
        assert_eq!(antigravity.source, OAuthClientSource::File);
        assert!(!antigravity.has_secret);
        assert!(
            !clients
                .configured
                .contains_key(&SubscriptionProvider::Antigravity)
        );
        // The public id named in the file with its secret is used as given.
        let clients = resolve_clients(
            &json!({"antigravity":{"client_id":ANTIGRAVITY_CLIENT,"client_secret":"file-secret"}}),
            &fake_env(&[]),
        );
        assert_eq!(
            clients.configured[&SubscriptionProvider::Antigravity].secret,
            "file-secret"
        );
    }

    /// A loopback token endpoint that records the client id and secret of
    /// every refresh.
    async fn token_fixture() -> (
        String,
        Arc<std::sync::Mutex<Vec<(String, String)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker_seen = seen.clone();
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(
                move |axum::Form(form): axum::Form<BTreeMap<String, String>>| {
                    let seen = worker_seen.clone();
                    async move {
                        seen.lock()
                            .unwrap()
                            .push((form["client_id"].clone(), form["client_secret"].clone()));
                        axum::Json(json!({"access_token":"fixture-rotated","expires_in":3600}))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (endpoint, seen, task)
    }

    #[tokio::test]
    async fn a_refresh_never_pairs_one_clients_id_with_another_clients_secret() {
        let (endpoint, seen, task) = token_fixture().await;
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.use_test_endpoint(&endpoint);
        let configure = |google: &mut GoogleOAuth, file: Value| {
            *google.clients.get_mut().unwrap() = resolve_clients(&file, &fake_env(&[]));
        };
        let refresh = |google: &GoogleOAuth, id: Option<&str>, secret: Option<&str>| {
            google.refresh_request(
                SubscriptionProvider::Antigravity,
                "fixture-refresh",
                id,
                secret,
            )
        };

        // A client configured with the public id (as detection sets it)
        // refreshes accounts pinned to that id.
        configure(
            &mut google,
            json!({"antigravity":{"client_id":ANTIGRAVITY_CLIENT,"client_secret":"detected-secret"}}),
        );
        send_json(refresh(&google, Some(ANTIGRAVITY_CLIENT), None).unwrap())
            .await
            .unwrap();

        // Beside a custom client, its own accounts and accounts without a
        // recorded client refresh with it; a stored pair is used as is.
        configure(
            &mut google,
            json!({"antigravity":{"client_id":"custom-id","client_secret":"custom-secret"}}),
        );
        send_json(refresh(&google, Some("custom-id"), None).unwrap())
            .await
            .unwrap();
        send_json(refresh(&google, None, None).unwrap())
            .await
            .unwrap();
        send_json(refresh(&google, Some("own-id"), Some("own-secret")).unwrap())
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                (ANTIGRAVITY_CLIENT, "detected-secret"),
                ("custom-id", "custom-secret"),
                ("custom-id", "custom-secret"),
                ("own-id", "own-secret"),
            ]
            .map(|(id, secret)| (id.to_owned(), secret.to_owned()))
        );

        // An account pinned to the public id with no matching client asks to
        // set up the Antigravity client; other failures are clear too, before
        // any request.
        for (id, secret, message) in [
            (
                Some(ANTIGRAVITY_CLIENT),
                None,
                "set up the Antigravity sign-in client in Settings",
            ),
            (
                Some("removed-id"),
                None,
                "this account was signed in with a Google client douane no longer has; reconnect it",
            ),
            (
                None,
                Some("own-secret"),
                "invalid authentication file: a Google client secret is stored without its client id",
            ),
        ] {
            let error = refresh(&google, id, secret).unwrap_err();
            assert_eq!(error.to_string(), message);
        }

        // A Gemini account that recorded no client uses the configured one.
        configure(
            &mut google,
            json!({"gemini":{"client_id":"gemini-id","client_secret":"gemini-secret"}}),
        );
        send_json(
            google
                .refresh_request(SubscriptionProvider::Gemini, "fixture-refresh", None, None)
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            seen.lock().unwrap().last().unwrap(),
            &("gemini-id".to_owned(), "gemini-secret".to_owned())
        );
        // Gemini has no client of its own to fall back on.
        let error = google
            .refresh_request(
                SubscriptionProvider::Gemini,
                "fixture-refresh",
                Some(GEMINI_CLIENT),
                None,
            )
            .unwrap_err();
        assert!(matches!(error, SubscriptionError::OAuthClientUnavailable));
        assert_eq!(seen.lock().unwrap().len(), 5);
        task.abort();
        let _ = task.await;
    }

    /// Collects what `tracing` writes while `run` runs.
    fn captured_logs(run: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buffer(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn an_unreadable_client_file_is_reported_without_its_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oauth-clients.json");
        std::fs::write(
            &path,
            br#"{"gemini":{"client_secret":"leaked-fixture-secret""#,
        )
        .unwrap();
        let logs = captured_logs(|| {
            let mut google = GoogleOAuth::load(reqwest::Client::new(), &path);
            google.use_test_env(|_| None);
            assert!(!google.supports(SubscriptionProvider::Gemini));
        });
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("oauth-clients.json is ignored"), "{logs}");
        assert!(logs.contains(&path.display().to_string()), "{logs}");
        assert!(logs.contains("it is not valid JSON"), "{logs}");
        assert!(!logs.contains("leaked-fixture-secret"), "{logs}");

        std::fs::write(&path, b"[]").unwrap();
        let logs = captured_logs(|| {
            GoogleOAuth::load(reqwest::Client::new(), &path);
        });
        assert!(logs.contains("it is not a JSON object"), "{logs}");
        // An absent file is simply empty.
        std::fs::remove_file(&path).unwrap();
        let logs = captured_logs(|| {
            GoogleOAuth::load(reqwest::Client::new(), &path);
        });
        assert!(logs.is_empty(), "{logs}");
    }

    #[tokio::test]
    async fn imported_antigravity_refresh_without_a_recorded_client_uses_the_configured_one() {
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(
                |axum::Form(form): axum::Form<BTreeMap<String, String>>| async move {
                    assert_eq!(form["grant_type"], "refresh_token");
                    assert_eq!(form["client_id"], ANTIGRAVITY_CLIENT);
                    assert_eq!(form["client_secret"], "detected-secret");
                    assert_eq!(form["refresh_token"], "fixture-refresh");
                    axum::Json(json!({"access_token":"fixture-rotated","expires_in":3600}))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.use_test_env(|_| None);
        google.use_test_endpoint(&endpoint);
        // A client configured with the public id, as detection sets it.
        *google.clients.get_mut().unwrap() = resolve_clients(
            &json!({"antigravity":{"client_id":ANTIGRAVITY_CLIENT,"client_secret":"detected-secret"}}),
            &fake_env(&[]),
        );
        let tokens = send_json(
            google
                .refresh_request(
                    SubscriptionProvider::Antigravity,
                    "fixture-refresh",
                    None,
                    None,
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(tokens["access_token"], "fixture-rotated");
        task.abort();
        let _ = task.await;
    }

    #[test]
    fn custom_clients_are_written_privately_and_applied_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oauth-clients.json");
        let mut google = GoogleOAuth::load(reqwest::Client::new(), &path);
        // Keep the test independent of the developer's environment.
        google.use_test_env(|_| None);
        assert!(!google.supports(SubscriptionProvider::Gemini));
        let view = google
            .set_client(
                SubscriptionProvider::Gemini,
                &json!({"client_id":" custom-id ","client_secret":"custom-secret","project_id":"custom-project"}),
            )
            .unwrap();
        assert_eq!(view.source, OAuthClientSource::File);
        assert_eq!(view.client_id.as_deref(), Some("custom-id"));
        assert_eq!(view.project_id.as_deref(), Some("custom-project"));
        assert!(view.has_secret);
        assert!(google.supports(SubscriptionProvider::Gemini));
        let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["gemini"]["client_secret"], "custom-secret");
        assert_private(&path);

        // The secret may be omitted to keep the stored one for the same id.
        google
            .set_client(
                SubscriptionProvider::Gemini,
                &json!({"client_id":"custom-id"}),
            )
            .unwrap();
        let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["gemini"]["client_secret"], "custom-secret");
        assert!(stored["gemini"].get("project_id").is_none());
        for (body, message) in [
            (
                json!({"client_id":"another-id"}),
                "client_secret is required",
            ),
            (json!({"client_secret":"x"}), "client_id is required"),
            (
                json!({"client_id":"id","client_secret":7}),
                "client_secret must be a non-empty single-line string of at most 512 bytes",
            ),
            (json!(["client_id"]), "request body must be a JSON object"),
        ] {
            let error = google
                .set_client(SubscriptionProvider::Gemini, &body)
                .unwrap_err();
            assert_eq!(error.to_string(), message);
        }
        // The public Antigravity id can be saved as a custom client (a pasted
        // or detected client uses exactly that id); `origin` is recorded.
        let view = google
            .set_client(
                SubscriptionProvider::Antigravity,
                &json!({"client_id":ANTIGRAVITY_CLIENT,"client_secret":"detected-secret","origin":"antigravity-cli"}),
            )
            .unwrap();
        assert_eq!(view.source, OAuthClientSource::File);
        assert_eq!(view.client_id.as_deref(), Some(ANTIGRAVITY_CLIENT));
        assert_eq!(view.origin.as_deref(), Some("antigravity-cli"));
        assert!(view.has_secret);
        // A provider without a client is refused before its body is read.
        for body in [json!({"client_id":"id","client_secret":"s"}), json!([])] {
            assert!(matches!(
                google.set_client(SubscriptionProvider::Claude, &body),
                Err(OAuthClientError::Unsupported)
            ));
        }

        let view = google.remove_client(SubscriptionProvider::Gemini).unwrap();
        assert_eq!(view.source, OAuthClientSource::None);
        assert!(!google.supports(SubscriptionProvider::Gemini));
        let view = google
            .remove_client(SubscriptionProvider::Antigravity)
            .unwrap();
        assert_eq!(view.source, OAuthClientSource::None);
        assert!(!google.supports(SubscriptionProvider::Antigravity));
        assert!(!path.exists());

        // A change refused because the file cannot be read changes nothing
        // live, and the file is left as it is.
        google
            .set_client(
                SubscriptionProvider::Gemini,
                &json!({"client_id":"custom-id","client_secret":"custom-secret"}),
            )
            .unwrap();
        std::fs::write(&path, b"not json").unwrap();
        let before = serde_json::to_value(google.client_views()).unwrap();
        assert!(matches!(
            google.set_client(
                SubscriptionProvider::Antigravity,
                &json!({"client_id":"id","client_secret":"s"})
            ),
            Err(OAuthClientError::Unreadable)
        ));
        assert!(matches!(
            google.remove_client(SubscriptionProvider::Gemini),
            Err(OAuthClientError::Unreadable)
        ));
        assert_eq!(serde_json::to_value(google.client_views()).unwrap(), before);
        assert!(google.supports(SubscriptionProvider::Gemini));
        // Reading the clients again reports the file and keeps them too.
        assert_eq!(serde_json::to_value(google.reload_views()).unwrap(), before);
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");

        // A hand edit shows, and applies, on the next read.
        std::fs::write(
            &path,
            json!({"antigravity":{"client_id":"hand-id","client_secret":"hand-secret"}})
                .to_string(),
        )
        .unwrap();
        let views = google.reload_views();
        assert_eq!(views[0].client_id.as_deref(), Some("hand-id"));
        assert_eq!(views[1].source, OAuthClientSource::None);
        assert!(!google.supports(SubscriptionProvider::Gemini));
    }

    #[test]
    fn hand_edits_apply_on_use_and_a_bad_file_is_reported_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oauth-clients.json");
        let mut google = GoogleOAuth::load(reqwest::Client::new(), &path);
        google.use_test_env(|_| None);
        assert!(!google.supports(SubscriptionProvider::Gemini));
        // No client list read in between: using the clients applies the file.
        std::fs::write(
            &path,
            json!({"gemini":{"client_id":"hand-id","client_secret":"hand-secret"}}).to_string(),
        )
        .unwrap();
        assert!(google.supports(SubscriptionProvider::Gemini));
        assert_eq!(
            google
                .client_pair(SubscriptionProvider::Gemini, Some("hand-id"))
                .unwrap(),
            ("hand-id".to_owned(), "hand-secret".to_owned())
        );
        // A broken file keeps the clients and is reported once, not on
        // every use.
        std::fs::write(&path, b"{broken").unwrap();
        let logs = captured_logs(|| {
            for _ in 0..3 {
                assert!(google.supports(SubscriptionProvider::Gemini));
            }
        });
        assert_eq!(
            logs.matches("oauth-clients.json is ignored").count(),
            1,
            "{logs}"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(!google.supports(SubscriptionProvider::Gemini));
    }

    /// Only the current user can read the file: mode 0600 on Unix, an
    /// owner-only ACL without inherited entries on Windows.
    pub(crate) fn assert_private(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        #[cfg(windows)]
        {
            let output = std::process::Command::new("icacls")
                .arg(path)
                .output()
                .unwrap();
            assert!(output.status.success());
            let acl = String::from_utf8_lossy(&output.stdout).to_string();
            let user = std::env::var("USERNAME").unwrap();
            let entries: Vec<&str> = acl
                .lines()
                .map(|line| {
                    line.trim()
                        .trim_start_matches(&*path.to_string_lossy())
                        .trim()
                })
                .filter(|line| line.contains(":("))
                .collect();
            assert_eq!(entries.len(), 1, "{acl}");
            assert!(entries[0].contains(&user), "{acl}");
            assert!(!entries[0].contains("(I)"), "{acl}");
        }
    }

    #[test]
    fn onboarding_diagnostics_only_contain_known_categories() {
        let loaded = json!({"ineligibleTiers": [
            {"reasonCode":"VALIDATION_REQUIRED","validationUrl":"https://accounts.google.com/signin/continue?private=value"},
            {"reasonCode":"GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT"},
            {"reasonCode":"UNSUPPORTED_CLIENT"},
            {"reasonCode":"private-provider-value"},
            {"reasonMessage":"private-provider-message"}
        ]});
        assert_eq!(
            ineligible_reason_categories(&loaded),
            vec![
                "VALIDATION_REQUIRED",
                "GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT",
                "UNSUPPORTED_CLIENT",
                "other",
                "absent"
            ]
        );
        assert_eq!(activation_tier_category("private-plan"), "other");
        assert_eq!(
            diagnostic_enum(Some(&json!(2)), &["DEFAULT", "NOTICE", "WARNING"]),
            "WARNING"
        );
        assert_eq!(
            diagnostic_enum(Some(&json!(999)), &["DEFAULT", "NOTICE", "WARNING"]),
            "other"
        );
        assert_eq!(
            diagnostic_enum(
                Some(&json!("private-status")),
                &["DEFAULT", "NOTICE", "WARNING"]
            ),
            "other"
        );
    }

    #[tokio::test]
    async fn google_code_exchange_profiles_and_onboards_a_managed_project() {
        let polls = Arc::new(AtomicUsize::new(0));
        let worker_polls = polls.clone();
        let app = axum::Router::new()
            .route("/token",axum::routing::post(|axum::Form(form): axum::Form<BTreeMap<String,String>>|async move{
                assert_eq!(form["grant_type"],"authorization_code");assert_eq!(form["client_id"],"fixture-client");
                assert_eq!(form["redirect_uri"],"https://codeassist.google.com/authcode");assert_eq!(form["code_verifier"],"fixture-verifier");
                axum::Json(json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_in":3600}))}))
            .route("/userinfo",axum::routing::get(||async{axum::Json(json!({"email":"fixture@example.test","id":"google-subject"}))}))
            .route("/v1internal:loadCodeAssist",axum::routing::post(||async{axum::Json(json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]}))}))
            .route("/v1internal:onboardUser",axum::routing::post(|axum::Json(body):axum::Json<Value>|async move{
                assert_eq!(body["tierId"],"free-tier");assert!(body.get("cloudaicompanionProject").is_none());
                assert_eq!(body["metadata"]["pluginType"],"GEMINI");
                axum::Json(json!({"name":"operations/fixture","done":false}))}))
            .route("/v1internal/operations/fixture",axum::routing::get(move||{let polls=worker_polls.clone();async move{
                polls.fetch_add(1,Ordering::SeqCst);axum::Json(json!({"done":true,"response":{"cloudaicompanionProject":{"id":"managed-project"}}}))
            }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.insert_client(
            SubscriptionProvider::Gemini,
            ClientMetadata {
                id: "fixture-client".into(),
                secret: "fixture-public-client-metadata".into(),
            },
        );
        google.token_url = format!("{endpoint}/token");
        google.userinfo_url = format!("{endpoint}/userinfo");
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.poll_delay = Duration::ZERO;
        let value = google
            .exchange(
                SubscriptionProvider::Gemini,
                &GoogleSignIn {
                    client_id: "fixture-client".into(),
                    project_id: None,
                },
                "fixture-code",
                "https://codeassist.google.com/authcode",
                "fixture-verifier",
            )
            .await
            .unwrap();
        assert_eq!(value["project_id"], "managed-project");
        assert_eq!(value["email"], "fixture@example.test");
        assert_eq!(value["type"], "gemini");
        assert!(value.get("client_secret").is_none());
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn antigravity_onboards_and_refreshes_with_runtime_client_metadata() {
        let polls = Arc::new(AtomicUsize::new(0));
        let worker_polls = polls.clone();
        let app = axum::Router::new()
            .route("/token", axum::routing::post(|axum::Form(form):axum::Form<BTreeMap<String,String>>|async move {
                assert_eq!(form["grant_type"],"refresh_token");assert_eq!(form["client_id"],"fixture-antigravity");
                assert_eq!(form["refresh_token"],"fixture-refresh");
                axum::Json(json!({"access_token":"fixture-rotated","expires_in":3600}))
            }))
            .route("/v1internal:loadCodeAssist",axum::routing::post(|headers:axum::http::HeaderMap, axum::Json(body):axum::Json<Value>|async move {
                assert_eq!(body["metadata"]["ideType"],"ANTIGRAVITY");
                assert_eq!(headers["user-agent"], antigravity_user_agent());
                axum::Json(json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]}))
            }))
            .route("/daily:onboardUser",axum::routing::post(move|headers:axum::http::HeaderMap, axum::Json(body):axum::Json<Value>|{
                let polls=worker_polls.clone();async move {
                    assert_eq!(body["tier_id"],"free-tier");assert_eq!(body["metadata"]["ide_type"],"ANTIGRAVITY");
                    assert_eq!(body["metadata"]["ide_version"], ANTIGRAVITY_VERSION);
                    assert_eq!(headers["user-agent"], antigravity_user_agent());
                    assert_eq!(headers["x-goog-api-client"],"gl-node/22.21.1");
                    let ready = polls.fetch_add(1,Ordering::SeqCst) > 0;
                    axum::Json(if ready {json!({"done":true,"response":{"cloudaicompanionProject":"antigravity-project"}})}else{json!({"done":false})})
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.insert_client(
            SubscriptionProvider::Antigravity,
            ClientMetadata {
                id: "fixture-antigravity".into(),
                secret: "fixture-public-metadata".into(),
            },
        );
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.daily_url = format!("{endpoint}/daily");
        google.token_url = format!("{endpoint}/token");
        google.poll_delay = Duration::ZERO;
        let (project, tier) = google
            .onboard(SubscriptionProvider::Antigravity, "fixture-access", None)
            .await
            .unwrap();
        assert_eq!(project, "antigravity-project");
        assert_eq!(tier.as_deref(), Some("free-tier"));
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        let tokens = send_json(
            google
                .refresh_request(
                    SubscriptionProvider::Antigravity,
                    "fixture-refresh",
                    Some("fixture-antigravity"),
                    None,
                )
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(tokens["access_token"], "fixture-rotated");
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn antigravity_onboarding_separates_activation_tier_from_paid_plan() {
        let loaded = Arc::new(std::sync::Mutex::new(Value::Null));
        let worker_loaded = loaded.clone();
        let requested = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let worker_requested = requested.clone();
        let app = axum::Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                axum::routing::post(move || {
                    let loaded = worker_loaded.clone();
                    async move { axum::Json(loaded.lock().unwrap().clone()) }
                }),
            )
            .route(
                "/daily:onboardUser",
                axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                    let requested = worker_requested.clone();
                    async move {
                        requested
                            .lock()
                            .unwrap()
                            .push(body["tier_id"].as_str().unwrap().to_owned());
                        axum::Json(json!({
                            "done": true,
                            "response": {"cloudaicompanionProject": {"id": "fixture-project"}}
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.daily_url = format!("{endpoint}/daily");
        google.poll_delay = Duration::ZERO;

        for (response, expected_tier) in [
            (
                json!({"currentTier":{"id":"standard-tier"},"paidTier":{"id":"g1-pro-tier"}}),
                "standard-tier",
            ),
            (json!({"paidTier":{"id":"g1-pro-tier"}}), "free-tier"),
            (
                json!({"currentTier":{"id":"standard-tier"},"paidTier":{"id":"g1-pro-tier"},"allowedTiers":[{"id":"free-tier","isDefault":true}]}),
                "free-tier",
            ),
        ] {
            *loaded.lock().unwrap() = response;
            let (project, plan) = google
                .onboard(SubscriptionProvider::Antigravity, "fixture-access", None)
                .await
                .unwrap();
            assert_eq!(requested.lock().unwrap().last().unwrap(), expected_tier);
            assert_eq!(project, "fixture-project");
            assert_eq!(plan.as_deref(), Some("g1-pro-tier"));
        }
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn completed_onboarding_without_a_project_is_an_activation_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let app = axum::Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                axum::routing::post(|| async {
                    axum::Json(json!({"currentTier": {"id": "standard-tier"}}))
                }),
            )
            .route(
                "/daily:onboardUser",
                axum::routing::post(move || {
                    let calls = worker_calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({"done": true, "response": {"private": "fixture-secret"}}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.daily_url = format!("{endpoint}/daily");
        google.poll_delay = Duration::ZERO;

        let error = google
            .onboard(SubscriptionProvider::Antigravity, "fixture-access", None)
            .await
            .unwrap_err();
        task.abort();
        let _ = task.await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            error.to_string(),
            "Google did not return a Cloud project for this account; check account setup in Antigravity"
        );
        assert!(!format!("{error:?}").contains("fixture-secret"));
    }

    #[tokio::test]
    async fn google_load_verification_stops_before_onboarding_without_exposing_provider_data() {
        let verification_url =
            "https://accounts.google.com/signin/continue?fixture=verification-challenge";
        let loaded = Arc::new(std::sync::Mutex::new(Value::Null));
        let worker_loaded = loaded.clone();
        let onboard_calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = onboard_calls.clone();
        let app = axum::Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                axum::routing::post(move || {
                    let loaded = worker_loaded.clone();
                    async move { axum::Json(loaded.lock().unwrap().clone()) }
                }),
            )
            .route(
                "/daily:onboardUser",
                axum::routing::post(move || {
                    let calls = worker_calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({
                            "done": true,
                            "response": {"cloudaicompanionProject": "fixture-project"}
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.codeassist_url = format!("{endpoint}/v1internal");
        google.daily_url = format!("{endpoint}/daily");
        google.poll_delay = Duration::ZERO;

        for provider in [
            SubscriptionProvider::Antigravity,
            SubscriptionProvider::Gemini,
        ] {
            for url in [
                Some(verification_url),
                Some("https://evil.example/challenge"),
                None,
            ] {
                *loaded.lock().unwrap() = json!({
                    "currentTier": null,
                    "ineligibleTiers": [{
                        "reasonCode": "VALIDATION_REQUIRED",
                        "reasonMessage": "provider-body-private",
                        "validationUrl": url
                    }]
                });
                let error = google
                    .onboard(provider, "fixture-access", None)
                    .await
                    .unwrap_err();
                assert!(error.is_verification_required());
                assert_eq!(
                    error.verification_url(),
                    url.filter(|url| *url == verification_url)
                );
                for rendered in [error.to_string(), format!("{error:?}")] {
                    assert!(!rendered.contains("provider-body-private"));
                    assert!(!rendered.contains("verification-challenge"));
                    assert!(!rendered.contains("evil.example"));
                }
            }
        }
        assert_eq!(onboard_calls.load(Ordering::SeqCst), 0);

        // Eligibility for one tier can coexist with a verification gate for another.
        // Keep the current eligible account connected, as Google's setup does.
        for paid_tier in [Value::Null, json!({})] {
            *loaded.lock().unwrap() = json!({
                "currentTier": {"id": "standard-tier"},
                "paidTier": paid_tier,
                "cloudaicompanionProject": "fixture-existing-project",
                "ineligibleTiers": [{
                    "reasonCode": "VALIDATION_REQUIRED",
                    "validationUrl": verification_url
                }]
            });
            let result = google
                .onboard(SubscriptionProvider::Antigravity, "fixture-access", None)
                .await
                .unwrap();
            assert_eq!(
                result,
                (
                    "fixture-existing-project".into(),
                    Some("standard-tier".into())
                )
            );
        }
        assert_eq!(onboard_calls.load(Ordering::SeqCst), 0);
        task.abort();
        let _ = task.await;
    }

    #[test]
    fn google_completed_onboarding_classifies_only_trusted_permission_verification() {
        let source = "https://daily-cloudcode-pa.googleapis.com/v1internal:onboardUser";
        let verification_url = "https://accounts.google.com/signin/continue?fixture=challenge";
        for code in [7, 403] {
            let operation = json!({
                "done": true,
                "error": {
                    "code": code,
                    "message": "provider-body-private",
                    "details": [{
                        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                        "reason": "VALIDATION_REQUIRED",
                        "domain": "cloudcode-pa.googleapis.com",
                        "metadata": {"validation_link": verification_url}
                    }]
                }
            });
            let error = completed_onboarding_error(&operation, source).unwrap();
            assert!(error.is_verification_required());
            assert_eq!(error.verification_url(), Some(verification_url));
            assert!(!error.to_string().contains("provider-body-private"));
            assert!(!format!("{error:?}").contains("fixture=challenge"));

            let error =
                completed_onboarding_error(&operation, "https://evil.example/onboardUser").unwrap();
            assert!(matches!(
                error,
                SubscriptionError::InvalidImport("Google account onboarding failed")
            ));
            let mut non_permission = operation;
            non_permission["error"]["code"] = json!(13);
            let error = completed_onboarding_error(&non_permission, source).unwrap();
            assert!(matches!(
                error,
                SubscriptionError::InvalidImport("Google account onboarding failed")
            ));
        }
        assert!(
            completed_onboarding_error(&json!({"done": true, "response": {}}), source).is_none()
        );
    }

    #[test]
    fn no_config_means_no_advertised_google_login_and_oob_has_pkce() {
        let directory = tempfile::tempdir().unwrap();
        let mut google = GoogleOAuth::load(
            reqwest::Client::new(),
            &directory.path().join("absent.json"),
        );
        google.clients.get_mut().unwrap().configured.clear();
        assert!(!google.supports(SubscriptionProvider::Antigravity));
        google.insert_client(
            SubscriptionProvider::Gemini,
            ClientMetadata {
                id: "fixture-client".into(),
                secret: "fixture-metadata".into(),
            },
        );
        let (url, sign_in) = google
            .authorize(
                SubscriptionProvider::Gemini,
                "state",
                "https://codeassist.google.com/authcode",
                "challenge",
            )
            .unwrap();
        assert_eq!(sign_in.client_id, "fixture-client");
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!url.contains("fixture-metadata"));
    }

    /// A `GOCSPX-…` secret built at runtime, so no literal matching
    /// `GOCSPX-[A-Za-z0-9_-]{28}` appears in this source.
    fn detect_secret(body: &str) -> String {
        assert_eq!(body.len(), 28);
        format!("GOCSPX-{body}")
    }

    /// A loopback token endpoint that answers `invalid_grant` for the right
    /// secret and `invalid_client` for any other, as Google does for a bad
    /// refresh token. Records how many times it was called.
    async fn detect_fixture(
        right: String,
    ) -> (
        String,
        Arc<std::sync::Mutex<usize>>,
        tokio::task::JoinHandle<()>,
    ) {
        let calls = Arc::new(std::sync::Mutex::new(0usize));
        let worker = calls.clone();
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(
                move |axum::Form(form): axum::Form<BTreeMap<String, String>>| {
                    let calls = worker.clone();
                    let right = right.clone();
                    async move {
                        *calls.lock().unwrap() += 1;
                        let error = if form.get("client_secret") == Some(&right) {
                            "invalid_grant"
                        } else {
                            "invalid_client"
                        };
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            axum::Json(json!({ "error": error })),
                        )
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (endpoint, calls, task)
    }

    /// Writes a synthetic CLI binary holding the client id and `secrets`.
    fn synthetic_binary(directory: &Path, present: bool, secrets: &[String]) -> PathBuf {
        let mut data: Vec<u8> = b"ELF noise before".to_vec();
        if present {
            data.extend_from_slice(ANTIGRAVITY_CLIENT.as_bytes());
        }
        for secret in secrets {
            data.extend_from_slice(b"\x00padding\x00");
            data.extend_from_slice(secret.as_bytes());
        }
        data.extend_from_slice(b"\x00tail");
        let path = directory.join("agy.bin");
        std::fs::write(&path, data).unwrap();
        path
    }

    #[tokio::test]
    async fn detection_validates_and_saves_the_matching_client_without_exposing_the_secret() {
        let right = detect_secret("rightrightrightrightright012");
        let wrong = detect_secret("wrongwrongwrongwrongwrong987");
        let (endpoint, calls, task) = detect_fixture(right.clone()).await;
        let directory = tempfile::tempdir().unwrap();
        let clients_path = directory.path().join("oauth-clients.json");
        let mut google = GoogleOAuth::load(reqwest::Client::new(), &clients_path);
        google.use_test_env(|_| None);
        google.use_test_endpoint(&endpoint);
        // The wrong secret is tried first, then the right one.
        let binary = synthetic_binary(directory.path(), true, &[wrong, right.clone()]);
        let view = google
            .detect_antigravity(Some(binary.display().to_string()))
            .await
            .unwrap();
        assert_eq!(view.source, OAuthClientSource::File);
        assert_eq!(view.client_id.as_deref(), Some(ANTIGRAVITY_CLIENT));
        assert_eq!(view.origin.as_deref(), Some("antigravity-cli"));
        assert!(view.has_secret);
        // The secret never appears in the view.
        assert!(!serde_json::to_string(&view).unwrap().contains("GOCSPX-"));
        // It is saved and used, but kept out of the view and the client list.
        let saved = std::fs::read_to_string(&clients_path).unwrap();
        assert!(saved.contains(&right));
        assert!(
            !serde_json::to_string(&google.client_views())
                .unwrap()
                .contains("GOCSPX-")
        );
        assert_eq!(*calls.lock().unwrap(), 2);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn detection_reports_no_match_an_env_managed_client_and_an_unreachable_google() {
        let right = detect_secret("abcdefghijklmnopqrstuvwx0123");
        let (endpoint, _calls, task) = detect_fixture(right.clone()).await;
        let directory = tempfile::tempdir().unwrap();
        let clients_path = directory.path().join("oauth-clients.json");
        let mut google = GoogleOAuth::load(reqwest::Client::new(), &clients_path);
        google.use_test_env(|_| None);
        google.use_test_endpoint(&endpoint);

        // A binary without the client id is no match.
        let without_id = synthetic_binary(directory.path(), false, &[right.clone()]);
        assert!(matches!(
            google
                .detect_antigravity(Some(without_id.display().to_string()))
                .await,
            Err(DetectError::NoMatchingClient)
        ));
        // A binary with the id but no matching secret is no match.
        let wrong = detect_secret("zzzzzzzzzzzzzzzzzzzzzzzz9999");
        let no_secret = synthetic_binary(directory.path(), true, &[wrong]);
        assert!(matches!(
            google
                .detect_antigravity(Some(no_secret.display().to_string()))
                .await,
            Err(DetectError::NoMatchingClient)
        ));
        // A missing path is not found.
        assert!(matches!(
            google
                .detect_antigravity(Some(
                    directory.path().join("absent.bin").display().to_string()
                ))
                .await,
            Err(DetectError::NotFound)
        ));
        // Nothing was saved by a failed detection.
        assert!(!clients_path.exists());

        // Google unreachable while validating.
        google.use_test_endpoint("http://127.0.0.1:1");
        let binary = synthetic_binary(directory.path(), true, &[right]);
        assert!(matches!(
            google
                .detect_antigravity(Some(binary.display().to_string()))
                .await,
            Err(DetectError::GoogleUnreachable)
        ));

        // An env-managed client is refused before any scan.
        google.use_test_endpoint(&endpoint);
        google.use_test_env(|name| {
            (name == "DOUANE_ANTIGRAVITY_CLIENT_ID").then(|| "env-id".to_owned())
        });
        assert!(matches!(
            google.detect_antigravity(None).await,
            Err(DetectError::EnvManaged)
        ));
        task.abort();
        let _ = task.await;
    }
}
