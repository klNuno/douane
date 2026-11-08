//! Detects an installed Antigravity CLI and reads its OAuth client secret.
//! The CLI binary embeds Google installed-app clients; douane streams it (see
//! `antigravity_scan`, shared with douanectl), collects the secret candidates
//! that follow the `GOCSPX-` prefix and whether douane's Antigravity client id
//! is present, and the caller validates each candidate against Google's token
//! endpoint. Nothing here logs or returns a secret value.

use super::antigravity_scan::{self, Scan};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Why detection could not configure a client. No variant carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error(
        "no Antigravity CLI was found on the douane machine; run `douanectl oauth-client detect-antigravity` on a machine that has it, or set the client manually"
    )]
    NotFound,
    #[error("no file was found at the given Antigravity CLI path")]
    PathNotFound,
    #[error(
        "the Antigravity CLI here does not hold douane's Antigravity client; set the client manually"
    )]
    NoMatchingClient,
    #[error("Google did not accept this Antigravity client secret")]
    RejectedSecret,
    #[error("Google could not be reached to check the Antigravity client; try again")]
    GoogleUnreachable,
    #[error("the Antigravity CLI could not be read")]
    Unreadable,
    #[error(
        "this OAuth client is set by DOUANE_ANTIGRAVITY_* environment variables; change or unset them and restart douane"
    )]
    EnvManaged,
    #[error(
        "another Antigravity client is set; confirm replacing it (\"force\": true, or douanectl --force): accounts signed in with it must reconnect"
    )]
    Conflict,
    #[error("cannot save the detected Antigravity client")]
    Storage,
}

/// A detection request: `POST /oauth-clients/antigravity/detect` with an
/// optional JSON body.
#[derive(Debug, Default)]
pub struct DetectRequest {
    /// The CLI binary to scan instead of the known install locations.
    pub path: Option<PathBuf>,
    /// A secret douanectl already detected on another machine: douane checks
    /// it with Google instead of scanning a local CLI.
    pub client_secret: Option<String>,
    /// Replace a different client set by hand.
    pub force: bool,
}

impl DetectRequest {
    /// Reads the body: empty means detect at the known locations; otherwise a
    /// JSON object with optional `path`, `client_secret` and `force`.
    pub fn parse(body: &[u8]) -> Result<Self, DetectError> {
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self::default());
        }
        let body = serde_json::from_slice::<Value>(body)
            .ok()
            .filter(Value::is_object)
            .ok_or(DetectError::Invalid("request body must be a JSON object"))?;
        let text = |key: &str, max: usize, message: &'static str| match body.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value))
                if !value.trim().is_empty()
                    && value.trim().len() <= max
                    && !value.trim().chars().any(char::is_control) =>
            {
                Ok(Some(value.trim().to_owned()))
            }
            Some(_) => Err(DetectError::Invalid(message)),
        };
        let path = text(
            "path",
            4096,
            "path must be a non-empty single-line string of at most 4096 bytes",
        )?;
        let client_secret = text(
            "client_secret",
            512,
            "client_secret must be a non-empty single-line string of at most 512 bytes",
        )?;
        if path.is_some() && client_secret.is_some() {
            return Err(DetectError::Invalid(
                "send either path or client_secret, not both",
            ));
        }
        let force = match body.get("force") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(force)) => *force,
            Some(_) => return Err(DetectError::Invalid("force must be a boolean")),
        };
        Ok(Self {
            path: path.map(PathBuf::from),
            client_secret,
            force,
        })
    }
}

/// A validated Antigravity client secret, on its way to `oauth-clients.json`.
/// It is deliberately neither `Debug` nor `Display`, so it cannot reach a log
/// or a response.
pub struct DetectedClient {
    pub(crate) secret: String,
}

/// Scans the CLI binaries at `paths`, in order, and returns the first scan
/// that holds the client id. An explicit path is scanned alone and its read
/// errors are reported; among the known locations an unreadable file is
/// skipped, and only reported when no other file holds the client.
pub(crate) fn scan_installs(
    paths: &[PathBuf],
    client_id: &str,
    explicit: bool,
) -> Result<Scan, DetectError> {
    let mut failure = None;
    for path in paths {
        match scan_file(path, client_id) {
            Ok(scan) if scan.client_id_present => return Ok(scan),
            Ok(_) => {}
            Err(error) if explicit => return Err(error),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or(DetectError::NoMatchingClient))
}

fn scan_file(path: &Path, client_id: &str) -> Result<Scan, DetectError> {
    antigravity_scan::scan_file(path, client_id).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => DetectError::PathNotFound,
        _ => DetectError::Unreadable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detect_body_is_optional_and_checked() {
        let request = DetectRequest::parse(b"").unwrap();
        assert!(request.path.is_none() && request.client_secret.is_none() && !request.force);
        let request = DetectRequest::parse(br#"{"path":" /opt/agy ","force":true}"#).unwrap();
        assert_eq!(request.path.as_deref(), Some(Path::new("/opt/agy")));
        assert!(request.force);
        let request = DetectRequest::parse(br#"{"client_secret":"s","force":null}"#).unwrap();
        assert_eq!(request.client_secret.as_deref(), Some("s"));
        assert!(!request.force);
        for (body, message) in [
            (&b"not json"[..], "request body must be a JSON object"),
            (b"[]", "request body must be a JSON object"),
            (br#"{"force":"yes"}"#, "force must be a boolean"),
            (
                br#"{"path":"a","client_secret":"b"}"#,
                "send either path or client_secret, not both",
            ),
            (
                br#"{"client_secret":"a\nb"}"#,
                "client_secret must be a non-empty single-line string of at most 512 bytes",
            ),
            (
                br#"{"path":""}"#,
                "path must be a non-empty single-line string of at most 4096 bytes",
            ),
        ] {
            let error = DetectRequest::parse(body).unwrap_err();
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn the_first_install_holding_the_client_wins_and_bad_files_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let client = "fixture-client.apps.googleusercontent.com";
        let script = directory.path().join("antigravity");
        std::fs::write(&script, b"#!/bin/sh\nexec something\n").unwrap();
        let cli = directory.path().join("agy");
        std::fs::write(&cli, format!("noise{client}noise")).unwrap();
        let absent = directory.path().join("absent");
        let scan = scan_installs(&[absent.clone(), script.clone(), cli], client, false).unwrap();
        assert!(scan.client_id_present);
        // Nothing holds the client: no match, or the read error if any.
        assert!(matches!(
            scan_installs(std::slice::from_ref(&script), client, false),
            Err(DetectError::NoMatchingClient)
        ));
        assert!(matches!(
            scan_installs(&[script, absent.clone()], client, false),
            Err(DetectError::PathNotFound)
        ));
        assert!(matches!(
            scan_installs(&[absent], client, true),
            Err(DetectError::PathNotFound)
        ));
    }
}
