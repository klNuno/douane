//! Detects an installed Antigravity CLI on this machine and reads its OAuth
//! client secret, so `douanectl` can configure a remote douane that has no CLI.
//! The scan itself is douane's own (`antigravity_scan.rs`, compiled here
//! through a `#[path]` module, never copied); candidates are validated against
//! Google's token endpoint. The secret is only ever sent to the configured
//! douane server, never printed.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

#[path = "../../douane/src/subscriptions/antigravity_scan.rs"]
mod scan;

/// Antigravity's public installed-app OAuth client id (not a secret). Kept in
/// sync with the douane server's `ANTIGRAVITY_CLIENT`.
pub const ANTIGRAVITY_CLIENT: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
/// Google's token endpoint, used to validate a candidate secret.
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// How long validation may take: each Google call, and all of them together,
/// so detection can never hang.
pub struct Limits {
    pub request: Duration,
    pub deadline: Duration,
}

/// The same limits as the douane server.
pub const LIMITS: Limits = Limits {
    request: Duration::from_secs(15),
    deadline: Duration::from_secs(60),
};

/// A validated Antigravity client. The secret is sent to douane, never printed.
pub struct Detected {
    pub client_secret: String,
}

/// Whether `candidate` is the Antigravity client's secret: a refresh with a
/// deliberately invalid refresh token answers `invalid_grant` for the right
/// secret and `invalid_client` for a wrong one. Any other answer (a timeout,
/// a 429 or 5xx, another error code) says nothing about the secret and fails.
/// Never prints the value.
async fn validate(
    client: &reqwest::Client,
    token_url: &str,
    candidate: &str,
    timeout: Duration,
) -> Result<bool> {
    const UNREACHABLE: &str = "Google could not check the Antigravity client; try again";
    let response = client
        .post(token_url)
        .timeout(timeout)
        .form(&[
            ("client_id", ANTIGRAVITY_CLIENT),
            ("client_secret", candidate),
            ("grant_type", "refresh_token"),
            ("refresh_token", "1//invalid"),
        ])
        .send()
        .await
        .context(UNREACHABLE)?;
    let status = response.status();
    if status.as_u16() == 429 || status.is_server_error() {
        bail!("{UNREACHABLE} (HTTP {})", status.as_u16());
    }
    let body: Value = response.json().await.context(UNREACHABLE)?;
    match body.get("error").and_then(Value::as_str) {
        Some("invalid_grant") => Ok(true),
        Some("invalid_client") => Ok(false),
        _ => bail!(UNREACHABLE),
    }
}

/// Finds the Antigravity CLI, scans it, and returns the validated client.
pub async fn detect(
    client: &reqwest::Client,
    token_url: &str,
    path: Option<PathBuf>,
    limits: &Limits,
) -> Result<Detected> {
    let paths = match path {
        Some(path) if !path.is_file() => bail!("no Antigravity CLI at {}", path.display()),
        Some(path) => vec![path],
        None => {
            let paths = scan::install_paths(&|name| std::env::var_os(name));
            if paths.is_empty() {
                bail!("no Antigravity CLI found on this machine; pass --path to its binary");
            }
            paths
        }
    };
    let mut found = None;
    for path in &paths {
        let scan = scan::scan_file(path, ANTIGRAVITY_CLIENT)
            .with_context(|| format!("reading the Antigravity CLI at {}", path.display()))?;
        if scan.client_id_present {
            found = Some(scan);
            break;
        }
    }
    let Some(scan) = found else {
        bail!(
            "the Antigravity CLI at {} does not hold douane's Antigravity client",
            paths[0].display()
        );
    };
    let validated = tokio::time::timeout(limits.deadline, async {
        for candidate in scan.candidates {
            if validate(client, token_url, &candidate, limits.request).await? {
                return Ok(Some(candidate));
            }
        }
        Ok::<_, anyhow::Error>(None)
    })
    .await
    .map_err(|_| anyhow::anyhow!("Google did not answer in time; try again"))??;
    match validated {
        Some(client_secret) => Ok(Detected { client_secret }),
        None => bail!(
            "no matching Antigravity client secret was found in the CLI; set the client manually"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `GOCSPX-…` secret built at runtime, so no literal matching
    /// `GOCSPX-[A-Za-z0-9_-]{28}` appears in this source.
    fn secret(body: &str) -> String {
        assert_eq!(body.len(), scan::SECRET_BODY_LEN);
        format!(
            "{}{body}",
            String::from_utf8(scan::SECRET_PREFIX.to_vec()).unwrap()
        )
    }

    /// A synthetic CLI holding the client id and `secrets`, in its own
    /// directory.
    fn synthetic_cli(secrets: &[String]) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("douanectl-detect-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut data = ANTIGRAVITY_CLIENT.as_bytes().to_vec();
        for secret in secrets {
            data.push(0);
            data.extend_from_slice(secret.as_bytes());
        }
        let path = directory.join("agy");
        std::fs::write(&path, data).unwrap();
        path
    }

    /// A loopback token endpoint answering every request with `status` and
    /// `body`, or never answering at all when `body` is None.
    fn token_endpoint(status: u16, body: Option<&'static str>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { return };
                let mut buffer = vec![0u8; 8192];
                let _ = socket.read(&mut buffer);
                match body {
                    Some(body) => {
                        let response = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes());
                    }
                    // Keep the connection open and silent.
                    None => held.push(socket),
                }
            }
        });
        url
    }

    const QUICK: Limits = Limits {
        request: Duration::from_millis(300),
        deadline: Duration::from_secs(5),
    };

    #[tokio::test]
    async fn a_silent_google_times_out_instead_of_hanging() {
        let cli = synthetic_cli(&[secret("aaaaaaaaaaaaaaaaaaaaaaaaaaaa")]);
        let url = token_endpoint(200, None);
        let client = reqwest::Client::new();
        let started = std::time::Instant::now();
        let error = detect(&client, &url, Some(cli.clone()), &QUICK)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("try again"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        // The overall deadline ends it even when each call may wait longer.
        let slow = Limits {
            request: Duration::from_secs(30),
            deadline: Duration::from_millis(300),
        };
        let error = detect(&client, &url, Some(cli), &slow).await.err().unwrap();
        assert_eq!(
            error.to_string(),
            "Google did not answer in time; try again"
        );
    }

    #[tokio::test]
    async fn only_invalid_client_means_a_wrong_secret() {
        let cli = synthetic_cli(&[secret("bbbbbbbbbbbbbbbbbbbbbbbbbbbb")]);
        let client = reqwest::Client::new();
        for (status, body) in [
            (503, r#"{"error":"backend_error"}"#),
            (429, r#"{"error":"rate_limited"}"#),
            (400, r#"{"error":"temporarily_unavailable"}"#),
            (200, "not json"),
        ] {
            let url = token_endpoint(status, Some(body));
            let error = detect(&client, &url, Some(cli.clone()), &QUICK)
                .await
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("try again"),
                "{status}: {error:#}"
            );
        }
        let url = token_endpoint(401, Some(r#"{"error":"invalid_client"}"#));
        let error = detect(&client, &url, Some(cli.clone()), &QUICK)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("no matching"), "{error:#}");
        let url = token_endpoint(400, Some(r#"{"error":"invalid_grant"}"#));
        let detected = detect(&client, &url, Some(cli), &QUICK).await.unwrap();
        assert_eq!(
            detected.client_secret,
            secret("bbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }
}
