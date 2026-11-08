//! Detects an installed Antigravity CLI on this machine and reads its OAuth
//! client secret, so `douanectl` can configure a remote douane that has no CLI.
//! The binary is streamed in overlapping chunks; candidates that follow the
//! `GOCSPX-` prefix are validated against Google's token endpoint. The secret
//! is only ever sent to the configured douane server, never printed.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Antigravity's public installed-app OAuth client id (not a secret). Kept in
/// sync with the douane server's `ANTIGRAVITY_CLIENT`.
pub const ANTIGRAVITY_CLIENT: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
/// Google's token endpoint, used to validate a candidate secret.
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const SECRET_PREFIX: &[u8] = b"GOCSPX-";
const SECRET_BODY_LEN: usize = 28;
const CHUNK: usize = 1 << 20;
const MAX_CANDIDATES: usize = 16;

/// A validated Antigravity client. The secret is sent to douane, never printed.
pub struct Detected {
    pub client_id: String,
    pub client_secret: String,
}

struct Scan {
    client_id_present: bool,
    candidates: Vec<String>,
}

/// The default Antigravity CLI path on this machine, if known.
pub fn default_install_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let path = PathBuf::from(local).join("agy").join("bin").join("agy.exe");
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn is_secret_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let last = haystack.len() - needle.len();
    let mut index = 0;
    while index <= last {
        if haystack[index] == first && &haystack[index..index + needle.len()] == needle {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn scan_window(
    window: &[u8],
    client_id: &[u8],
    scan: &mut Scan,
    seen: &mut std::collections::BTreeSet<String>,
) {
    if !scan.client_id_present && find(window, client_id).is_some() {
        scan.client_id_present = true;
    }
    let mut from = 0;
    while let Some(relative) = find(&window[from..], SECRET_PREFIX) {
        let start = from + relative;
        let body = start + SECRET_PREFIX.len();
        let end = body + SECRET_BODY_LEN;
        from = body;
        if end > window.len() || !window[body..end].iter().all(|byte| is_secret_char(*byte)) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&window[start..end]) else {
            continue;
        };
        if seen.insert(text.to_owned()) && scan.candidates.len() < MAX_CANDIDATES {
            scan.candidates.push(text.to_owned());
        }
    }
}

fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("reading the Antigravity CLI"),
        }
    }
    Ok(filled)
}

fn scan_reader(reader: &mut impl Read) -> Result<Scan> {
    let client_id = ANTIGRAVITY_CLIENT.as_bytes();
    let overlap = client_id
        .len()
        .max(SECRET_PREFIX.len() + SECRET_BODY_LEN)
        .saturating_sub(1);
    let mut scan = Scan {
        client_id_present: false,
        candidates: Vec::new(),
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let read = read_full(reader, &mut chunk)?;
        if read == 0 {
            break;
        }
        let mut window = Vec::with_capacity(carry.len() + read);
        window.extend_from_slice(&carry);
        window.extend_from_slice(&chunk[..read]);
        scan_window(&window, client_id, &mut scan, &mut seen);
        let keep = window.len().min(overlap);
        carry = window[window.len() - keep..].to_vec();
        if read < chunk.len() {
            break;
        }
    }
    Ok(scan)
}

fn scan_file(path: &Path) -> Result<Scan> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening the Antigravity CLI at {}", path.display()))?;
    scan_reader(&mut std::io::BufReader::new(file))
}

/// True when `candidate` is the Antigravity client's secret: a refresh with a
/// deliberately invalid refresh token answers `invalid_grant` for the right
/// secret and `invalid_client` for a wrong one. Never prints the value.
async fn validate(client: &reqwest::Client, token_url: &str, candidate: &str) -> Result<bool> {
    let response = client
        .post(token_url)
        .form(&[
            ("client_id", ANTIGRAVITY_CLIENT),
            ("client_secret", candidate),
            ("grant_type", "refresh_token"),
            ("refresh_token", "1//invalid"),
        ])
        .send()
        .await
        .context("reaching Google to validate the Antigravity client")?;
    let body: Value = response
        .json()
        .await
        .context("reading Google's validation response")?;
    Ok(body.get("error").and_then(Value::as_str) == Some("invalid_grant"))
}

/// Finds the Antigravity CLI, scans it, and returns the validated client pair.
pub async fn detect(
    client: &reqwest::Client,
    token_url: &str,
    path: Option<PathBuf>,
) -> Result<Detected> {
    let path = match path {
        Some(path) => {
            if !path.is_file() {
                bail!("no Antigravity CLI at {}", path.display());
            }
            path
        }
        None => default_install_path()
            .context("no Antigravity CLI found on this machine; pass --path to its binary")?,
    };
    let scan = scan_file(&path)?;
    if !scan.client_id_present {
        bail!(
            "the Antigravity CLI at {} does not hold douane's Antigravity client",
            path.display()
        );
    }
    for candidate in scan.candidates {
        if validate(client, token_url, &candidate).await? {
            return Ok(Detected {
                client_id: ANTIGRAVITY_CLIENT.to_owned(),
                client_secret: candidate,
            });
        }
    }
    bail!("no matching Antigravity client secret was found in the CLI; set the client manually")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `GOCSPX-…` secret built at runtime, so no literal matching
    /// `GOCSPX-[A-Za-z0-9_-]{28}` appears in this source.
    fn secret(body: &str) -> String {
        assert_eq!(body.len(), SECRET_BODY_LEN);
        format!(
            "{}{body}",
            String::from_utf8(SECRET_PREFIX.to_vec()).unwrap()
        )
    }

    #[test]
    fn scans_the_client_id_and_distinct_candidates() {
        let first = secret("aaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let second = secret("bbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let mut data = b"noise".to_vec();
        data.extend_from_slice(ANTIGRAVITY_CLIENT.as_bytes());
        data.extend_from_slice(b"\x00");
        data.extend_from_slice(first.as_bytes());
        data.extend_from_slice(b"\x00");
        data.extend_from_slice(second.as_bytes());
        data.extend_from_slice(b"\x00");
        data.extend_from_slice(first.as_bytes());
        let scan = scan_reader(&mut &data[..]).unwrap();
        assert!(scan.client_id_present);
        assert_eq!(scan.candidates, vec![first, second]);
    }

    #[test]
    fn finds_a_candidate_split_across_a_chunk_boundary() {
        let value = secret("cccccccccccccccccccccccccccc");
        let mut data = vec![b'.'; CHUNK - 3];
        data.extend_from_slice(value.as_bytes());
        data.extend_from_slice(ANTIGRAVITY_CLIENT.as_bytes());
        let scan = scan_reader(&mut &data[..]).unwrap();
        assert!(scan.client_id_present);
        assert_eq!(scan.candidates, vec![value]);
    }
}
