//! The pure core of Antigravity CLI detection: where the CLI is installed and
//! how its binary is scanned for Google client secret candidates. douane and
//! douanectl both compile this one file (douanectl through a `#[path]`
//! module), so the scan is never copied. It uses std only: no network, no
//! logging, and it never prints a candidate.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The literal prefix every Google installed-app client secret carries.
pub const SECRET_PREFIX: &[u8] = b"GOCSPX-";
/// A Google client secret is the prefix plus exactly this many characters from
/// `[A-Za-z0-9_-]`.
pub const SECRET_BODY_LEN: usize = 28;
/// Read the binary in 1 MiB chunks: fast, yet memory stays flat on a ~225 MB
/// binary.
const CHUNK: usize = 1 << 20;
/// Keep, and so validate, at most 16 distinct candidates. The real CLI embeds
/// three Google client secrets; the cap means a crafted binary can neither make
/// detection flood Google's token endpoint nor grow memory without bound. The
/// scan stops reading once it holds this many and has seen the client id.
pub const MAX_CANDIDATES: usize = 16;
/// Scan at most the first 512 MiB of a file (the real CLI is about 225 MB), so
/// a huge file passed as the CLI path cannot keep detection reading.
pub const MAX_SCAN_BYTES: u64 = 512 << 20;

/// What a scan of the CLI binary found.
pub struct Scan {
    /// The binary contains the Antigravity client id.
    pub client_id_present: bool,
    /// Distinct `GOCSPX-…` secret candidates, in first-seen order, at most
    /// `MAX_CANDIDATES`.
    pub candidates: Vec<String>,
}

impl Scan {
    fn complete(&self) -> bool {
        self.client_id_present && self.candidates.len() >= MAX_CANDIDATES
    }
}

/// Where an Antigravity CLI may be installed on this machine: existing files
/// only, most likely first, without duplicates. The environment is read
/// through `var` (`std::env::var_os` outside tests). The known install
/// locations come first: the CLI
/// installs itself to `%LOCALAPPDATA%\agy\bin\agy.exe` on Windows, and to
/// `~/.local/bin/agy` on Linux and macOS, where it also looks for its own
/// helper binaries. Then `agy` and `antigravity` on `PATH`.
pub fn install_paths(var: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if cfg!(windows) {
        if let Some(local) = var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local).join("agy").join("bin").join("agy.exe"));
        }
    } else if let Some(home) = var("HOME") {
        candidates.push(PathBuf::from(home).join(".local").join("bin").join("agy"));
    }
    if let Some(path) = var("PATH") {
        let directories: Vec<PathBuf> = std::env::split_paths(&path).collect();
        for name in ["agy", "antigravity"] {
            let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            candidates.extend(directories.iter().map(|directory| directory.join(&file)));
        }
    }
    let mut seen = Vec::new();
    let mut found = Vec::new();
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let key = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if !seen.contains(&key) {
            seen.push(key);
            found.push(candidate);
        }
    }
    found
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
fn scan_window(window: &[u8], client_id: &[u8], scan: &mut Scan) {
    if !scan.client_id_present && find(window, client_id).is_some() {
        scan.client_id_present = true;
    }
    let mut from = 0;
    while scan.candidates.len() < MAX_CANDIDATES {
        let Some(relative) = find(&window[from..], SECRET_PREFIX) else {
            break;
        };
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
        // At most MAX_CANDIDATES are held, so this lookup stays small.
        if !scan.candidates.iter().any(|known| known == text) {
            scan.candidates.push(text.to_owned());
        }
    }
}

fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Streams `reader` in overlapping chunks and collects the client id presence
/// and the secret candidates, without holding the whole input in memory. It
/// reads at most `MAX_SCAN_BYTES`, and stops early once it holds
/// `MAX_CANDIDATES` and has seen the client id.
pub fn scan_reader(reader: &mut impl Read, client_id: &str) -> std::io::Result<Scan> {
    scan_limited(reader, client_id, MAX_SCAN_BYTES)
}

/// `scan_reader` reading at most `limit` bytes.
fn scan_limited(reader: &mut impl Read, client_id: &str, limit: u64) -> std::io::Result<Scan> {
    let client_id = client_id.as_bytes();
    let mut reader = reader.take(limit);
    // Carry enough bytes that any needle split across a boundary is seen whole.
    let overlap = client_id
        .len()
        .max(SECRET_PREFIX.len() + SECRET_BODY_LEN)
        .saturating_sub(1);
    let mut scan = Scan {
        client_id_present: false,
        candidates: Vec::new(),
    };
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let read = read_full(&mut reader, &mut chunk)?;
        if read == 0 {
            break;
        }
        let mut window = Vec::with_capacity(carry.len() + read);
        window.extend_from_slice(&carry);
        window.extend_from_slice(&chunk[..read]);
        scan_window(&window, client_id, &mut scan);
        if scan.complete() || read < chunk.len() {
            break;
        }
        let keep = window.len().min(overlap);
        carry = window[window.len() - keep..].to_vec();
    }
    Ok(scan)
}

/// Streams the file at `path` and collects its client id presence and secret
/// candidates.
pub fn scan_file(path: &Path, client_id: &str) -> std::io::Result<Scan> {
    let file = std::fs::File::open(path)?;
    scan_reader(&mut std::io::BufReader::new(file), client_id)
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
    fn finds_a_secret_and_the_client_id_split_across_chunk_boundaries() {
        let right = secret("boundarySPLIT0123456789abcde");
        // The secret straddles the first chunk boundary, the client id the
        // second.
        let mut data = vec![b'.'; CHUNK - 4];
        data.extend_from_slice(right.as_bytes());
        data.resize(2 * CHUNK - 10, b'.');
        data.extend_from_slice(CLIENT_ID.as_bytes());
        let scan = scan_reader(&mut &data[..], CLIENT_ID).unwrap();
        assert!(scan.client_id_present);
        assert_eq!(scan.candidates, vec![right]);
    }

    #[test]
    fn reports_a_binary_without_the_client_id() {
        let data = secret("noClientIdHere01234567890123").into_bytes();
        let scan = scan_reader(&mut &data[..], CLIENT_ID).unwrap();
        assert!(!scan.client_id_present);
        assert_eq!(scan.candidates.len(), 1);
    }

    #[test]
    fn ignores_a_prefix_without_a_full_secret_body() {
        // The prefix with too few trailing secret characters is not a
        // candidate; a non-secret byte ends the run before 28 characters.
        let mut data = SECRET_PREFIX.to_vec();
        data.extend_from_slice(b"short\x00");
        data.extend_from_slice(CLIENT_ID.as_bytes());
        let scan = scan_reader(&mut &data[..], CLIENT_ID).unwrap();
        assert!(scan.candidates.is_empty());
        assert!(scan.client_id_present);
    }

    /// A reader that yields `chunk` forever and counts the bytes read.
    struct Endless {
        chunk: Vec<u8>,
        offset: usize,
        read: u64,
    }
    impl Read for Endless {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let mut written = 0;
            while written < buffer.len() {
                let rest = &self.chunk[self.offset..];
                let count = rest.len().min(buffer.len() - written);
                buffer[written..written + count].copy_from_slice(&rest[..count]);
                written += count;
                self.offset = (self.offset + count) % self.chunk.len();
            }
            self.read += written as u64;
            Ok(written)
        }
    }

    #[test]
    fn keeps_at_most_sixteen_candidates_and_stops_once_it_has_them() {
        let mut data = CLIENT_ID.as_bytes().to_vec();
        for index in 0..40 {
            data.push(0);
            data.extend_from_slice(secret(&format!("candidate{index:019}")).as_bytes());
        }
        let mut reader = Endless {
            chunk: data,
            offset: 0,
            read: 0,
        };
        let scan = scan_reader(&mut reader, CLIENT_ID).unwrap();
        assert!(scan.client_id_present);
        assert_eq!(scan.candidates.len(), MAX_CANDIDATES);
        assert_eq!(scan.candidates[0], secret("candidate0000000000000000000"));
        // It stopped after the first chunk instead of reading on.
        assert_eq!(reader.read, CHUNK as u64);
    }

    #[test]
    fn reads_at_most_the_scan_limit() {
        // Without the client id the scan never completes early, so only the
        // byte cap ends an endless input. `scan_reader` uses MAX_SCAN_BYTES;
        // a smaller cap keeps this test fast.
        let mut reader = Endless {
            chunk: vec![b'.'; 4096],
            offset: 0,
            read: 0,
        };
        let limit = 3 * CHUNK as u64 + 100;
        let scan = scan_limited(&mut reader, CLIENT_ID, limit).unwrap();
        assert!(!scan.client_id_present);
        assert!(scan.candidates.is_empty());
        assert_eq!(reader.read, limit);
        assert_eq!(MAX_SCAN_BYTES, 512 * 1024 * 1024);
    }

    #[test]
    fn install_paths_check_the_known_location_then_path_without_duplicates() {
        let directory = tempfile_dir();
        let exe = std::env::consts::EXE_SUFFIX;
        let (known, on_path) = (directory.join("known"), directory.join("on-path"));
        let known_file = if cfg!(windows) {
            known.join("agy").join("bin").join("agy.exe")
        } else {
            known.join(".local").join("bin").join("agy")
        };
        std::fs::create_dir_all(known_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&on_path).unwrap();
        std::fs::write(&known_file, b"cli").unwrap();
        std::fs::write(on_path.join(format!("antigravity{exe}")), b"cli").unwrap();
        // A directory named like the CLI is not a CLI.
        std::fs::create_dir_all(on_path.join(format!("agy{exe}"))).unwrap();
        let path =
            std::env::join_paths([known_file.parent().unwrap().to_path_buf(), on_path.clone()])
                .unwrap();
        let home = if cfg!(windows) {
            "LOCALAPPDATA"
        } else {
            "HOME"
        };
        let env = |name: &str| match name {
            "PATH" => Some(path.clone()),
            name if name == home => Some(known.clone().into_os_string()),
            _ => None,
        };
        assert_eq!(
            install_paths(&env),
            vec![known_file, on_path.join(format!("antigravity{exe}"))]
        );
        assert!(install_paths(&|_| None).is_empty());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// A fresh directory under the system temp dir; douanectl has no
    /// `tempfile` dependency, so this test makes its own.
    fn tempfile_dir() -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("antigravity-scan-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }
}
