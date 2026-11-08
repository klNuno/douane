//! Detects an installed Antigravity CLI and reads its OAuth client secret.
//! The CLI binary embeds Google installed-app clients; douane streams it in
//! overlapping chunks, collects the secret candidates that follow the
//! `GOCSPX-` prefix and whether douane's Antigravity client id is present, and
//! the caller validates each candidate against Google's token endpoint.
//! Nothing here logs or returns a secret value.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The literal prefix every Google installed-app client secret carries.
const SECRET_PREFIX: &[u8] = b"GOCSPX-";
/// A Google client secret is the prefix plus exactly this many characters from
/// `[A-Za-z0-9_-]`.
const SECRET_BODY_LEN: usize = 28;
/// Read the binary in 1 MiB chunks: fast, yet memory stays flat on a ~225 MB
/// binary.
const CHUNK: usize = 1 << 20;
/// Validate at most this many distinct candidates, so a binary can never make
/// douane flood Google's token endpoint.
const MAX_CANDIDATES: usize = 16;

/// Why detection could not configure a client. No variant carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    #[error(
        "no Antigravity CLI was found on this machine; pass its path or set the client manually"
    )]
    NotFound,
    #[error(
        "the Antigravity CLI here does not hold douane's Antigravity client; set the client manually"
    )]
    NoMatchingClient,
    #[error("Google could not be reached to check the Antigravity client; try again")]
    GoogleUnreachable,
    #[error("the Antigravity CLI could not be read")]
    Unreadable,
    #[error(
        "this OAuth client is set by DOUANE_ANTIGRAVITY_* environment variables; change or unset them and restart douane"
    )]
    EnvManaged,
    #[error("cannot save the detected Antigravity client")]
    Storage,
}

/// What a scan of the CLI binary found.
pub(crate) struct Scan {
    /// The binary contains douane's Antigravity client id.
    pub(crate) client_id_present: bool,
    /// Distinct `GOCSPX-…` secret candidates, in first-seen order.
    pub(crate) candidates: Vec<String>,
}

/// The default Antigravity CLI path on this machine, if the platform has a
/// known one. Only Windows is known (`%LOCALAPPDATA%\agy\bin\agy.exe`); other
/// platforms rely on an explicit path.
pub(crate) fn default_install_path() -> Option<PathBuf> {
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

/// The first index in `haystack` where `needle` occurs.
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

/// Scans one window for the client id and secret candidates. Candidates whose
/// 28 characters run past the window end are left for the next, overlapping,
/// window to find whole.
fn scan_window(window: &[u8], client_id: &[u8], scan: &mut Scan, seen: &mut BTreeSet<String>) {
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

fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> Result<usize, DetectError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(DetectError::Unreadable),
        }
    }
    Ok(filled)
}

/// Streams `reader` in overlapping chunks and collects the client id presence
/// and the secret candidates, without holding the whole input in memory.
fn scan_reader(reader: &mut impl Read, client_id: &str) -> Result<Scan, DetectError> {
    let client_id = client_id.as_bytes();
    // Carry enough bytes that any needle split across a boundary is seen whole.
    let overlap = client_id
        .len()
        .max(SECRET_PREFIX.len() + SECRET_BODY_LEN)
        .saturating_sub(1);
    let mut scan = Scan {
        client_id_present: false,
        candidates: Vec::new(),
    };
    let mut seen = BTreeSet::new();
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

/// Streams the file at `path` and collects its client id presence and secret
/// candidates.
pub(crate) fn scan_file(path: &Path, client_id: &str) -> Result<Scan, DetectError> {
    let file = std::fs::File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => DetectError::NotFound,
        _ => DetectError::Unreadable,
    })?;
    let mut reader = std::io::BufReader::new(file);
    scan_reader(&mut reader, client_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `GOCSPX-…` secret at runtime, so no literal matching
    /// `GOCSPX-[A-Za-z0-9_-]{28}` ever appears in this source.
    fn secret(body: &str) -> String {
        assert_eq!(body.len(), SECRET_BODY_LEN);
        format!(
            "{}{body}",
            String::from_utf8(SECRET_PREFIX.to_vec()).unwrap()
        )
    }

    const CLIENT_ID: &str =
        "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";

    #[test]
    fn collects_the_client_id_and_distinct_secret_candidates() {
        let right = secret("abcdefghijklmnopqrstuvwxyz01");
        let wrong = secret("ABCDEFGHIJKLMNOPQRSTUVWXYZ98");
        let mut data = Vec::new();
        data.extend_from_slice(b"\x00\x01noise");
        data.extend_from_slice(right.as_bytes());
        data.extend_from_slice(b"padding");
        data.extend_from_slice(CLIENT_ID.as_bytes());
        data.extend_from_slice(b"more");
        data.extend_from_slice(wrong.as_bytes());
        // A repeat is not collected twice.
        data.extend_from_slice(right.as_bytes());
        let scan = scan_reader(&mut &data[..], CLIENT_ID).unwrap();
        assert!(scan.client_id_present);
        assert_eq!(scan.candidates, vec![right, wrong]);
    }

    #[test]
    fn finds_a_secret_split_across_a_chunk_boundary() {
        let right = secret("boundarySPLIT0123456789abcde");
        // Place the secret so it straddles the CHUNK boundary.
        let prefix_len = CHUNK - 4;
        let mut data = vec![b'.'; prefix_len];
        data.extend_from_slice(right.as_bytes());
        data.extend_from_slice(CLIENT_ID.as_bytes());
        let scan = scan_reader(&mut &data[..], CLIENT_ID).unwrap();
        assert!(scan.client_id_present);
        assert_eq!(scan.candidates, vec![right]);
    }

    #[test]
    fn reports_a_binary_without_the_client_id() {
        let only_secret = secret("noClientIdHere01234567890123");
        let data = only_secret.into_bytes();
        let scan = scan_reader(&mut &data[..], CLIENT_ID).unwrap();
        assert!(!scan.client_id_present);
        assert_eq!(scan.candidates.len(), 1);
    }

    #[test]
    fn ignores_a_prefix_without_a_full_secret_body() {
        // The prefix with too few trailing secret characters is not a
        // candidate; a non-secret byte ends the run before 28 characters.
        let mut data = b"GOCSPX-short\x00".to_vec();
        data.extend_from_slice(CLIENT_ID.as_bytes());
        let scan = scan_reader(&mut &data[..], CLIENT_ID).unwrap();
        assert!(scan.candidates.is_empty());
        assert!(scan.client_id_present);
    }
}
