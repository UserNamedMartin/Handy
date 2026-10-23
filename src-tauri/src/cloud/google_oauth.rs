//! OAuth access tokens for Vertex AI, minted from a local credentials file.
//!
//! Vertex does not take API keys here: a plain key is refused outright
//! ("API keys are not supported by this API"), and a key bound to a service
//! account is blocked by the default organization policy
//! `iam.managed.disableServiceAccountApiKeyCreation`. What works everywhere is
//! the file `gcloud auth application-default login` writes — an
//! `authorized_user` credential holding a long-lived refresh token, which
//! trades for a one-hour access token at Google's token endpoint.
//!
//! Only `authorized_user` is supported. A service-account file would need RS256
//! JWT signing and a crypto dependency for a credential this fork has no use
//! for, so it is rejected with a message that says so rather than half-handled.

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The credential last loaded, keyed by its path and the file's modification
/// time. Every Live dictation builds its route afresh; without this each one
/// would re-read the file and spend a round trip on a new access token. The
/// mtime is in the key so a fresh `gcloud` login is picked up without a restart.
static SHARED: Lazy<Mutex<Option<(PathBuf, Option<SystemTime>, Arc<VertexCredentials>)>>> =
    Lazy::new(|| Mutex::new(None));

/// Refresh this long before Google's stated expiry, so a token never runs out
/// between being handed to a request and that request arriving — a Live
/// session in particular authenticates once, at connect.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    refresh_token: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// A refreshable OAuth credential with its current access token cached.
///
/// Shared by the batch engine and every Live session through an `Arc`, so a
/// dictation reuses the token the previous one fetched instead of paying an
/// extra round trip to Google's token endpoint each time.
pub struct VertexCredentials {
    path: PathBuf,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    http: reqwest::Client,
    cached: Mutex<Option<(String, Instant)>>,
}

impl VertexCredentials {
    /// Read and validate the file. Fails here — when the engine loads — rather
    /// than at the end of the user's first dictation.
    pub fn load(path: &str) -> Result<Self> {
        let path = expand_home(path.trim());
        if path.as_os_str().is_empty() {
            return Err(anyhow!(
                "No Vertex credentials file configured. Set its path in Settings → Gemini → Vertex AI."
            ));
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            anyhow!(
                "Cannot read Vertex credentials file {}: {}",
                path.display(),
                e
            )
        })?;
        let file: CredentialsFile = serde_json::from_str(&raw).map_err(|e| {
            anyhow!(
                "Vertex credentials file {} is not valid JSON: {}",
                path.display(),
                e
            )
        })?;
        check_kind(&file)?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| anyhow!("Failed to build HTTP client for Google OAuth: {}", e))?;

        Ok(Self {
            path,
            client_id: file.client_id,
            client_secret: file.client_secret,
            refresh_token: file.refresh_token,
            http,
            cached: Mutex::new(None),
        })
    }

    /// The credential for `path`, reused across dictations while the file is
    /// unchanged so its cached access token survives from one to the next.
    pub fn shared(path: &str) -> Result<Arc<Self>> {
        let resolved = expand_home(path.trim());
        let modified = std::fs::metadata(&resolved)
            .and_then(|meta| meta.modified())
            .ok();

        let mut shared = SHARED.lock().unwrap();
        if let Some((cached_path, cached_modified, credentials)) = shared.as_ref() {
            if *cached_path == resolved && *cached_modified == modified {
                return Ok(Arc::clone(credentials));
            }
        }
        let credentials = Arc::new(Self::load(path)?);
        *shared = Some((resolved, modified, Arc::clone(&credentials)));
        Ok(credentials)
    }

    /// A valid access token, refreshed when the cached one is about to expire.
    /// Synchronous; safe to call from any thread (see [`super::block_on`]).
    pub fn access_token(&self) -> Result<String> {
        if let Some((token, valid_until)) = self.cached.lock().unwrap().as_ref() {
            if Instant::now() < *valid_until {
                return Ok(token.clone());
            }
        }

        let request = self.http.post(TOKEN_URL).form(&[
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("refresh_token", self.refresh_token.as_str()),
        ]);
        let path = self.path.clone();
        let response: TokenResponse = super::block_on(async move {
            let response = request
                .send()
                .await
                .map_err(|e| anyhow!("Google OAuth request failed: {}", e))?;
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| anyhow!("Failed to read the Google OAuth response: {}", e))?;
            if !status.is_success() {
                // `invalid_grant` here means the refresh token was revoked or
                // expired — the fix is a fresh login, so say which file.
                return Err(anyhow!(
                    "Google refused the Vertex credentials in {} ({}): {}",
                    path.display(),
                    status,
                    text
                ));
            }
            serde_json::from_str(&text)
                .map_err(|e| anyhow!("Unexpected Google OAuth response: {}", e))
        })?;

        let lifetime = Duration::from_secs(response.expires_in).saturating_sub(EXPIRY_MARGIN);
        *self.cached.lock().unwrap() =
            Some((response.access_token.clone(), Instant::now() + lifetime));
        Ok(response.access_token)
    }
}

fn check_kind(file: &CredentialsFile) -> Result<()> {
    match file.kind.as_str() {
        "authorized_user" => {
            if file.client_id.is_empty()
                || file.client_secret.is_empty()
                || file.refresh_token.is_empty()
            {
                return Err(anyhow!(
                    "Vertex credentials file is missing client_id, client_secret or refresh_token"
                ));
            }
            Ok(())
        }
        "service_account" => Err(anyhow!(
            "Service-account credentials are not supported; use the file `gcloud auth application-default login` writes"
        )),
        other => Err(anyhow!("Unsupported Vertex credentials type '{}'", other)),
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> CredentialsFile {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn an_authorized_user_file_is_accepted() {
        let file = parse(
            r#"{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r"}"#,
        );
        assert!(check_kind(&file).is_ok());
    }

    #[test]
    fn a_service_account_file_is_refused_with_a_reason() {
        let file = parse(r#"{"type":"service_account","private_key":"…"}"#);
        let error = check_kind(&file).unwrap_err().to_string();
        assert!(error.contains("not supported"), "{error}");
    }

    #[test]
    fn a_file_without_a_refresh_token_is_refused() {
        let file = parse(r#"{"type":"authorized_user","client_id":"c","client_secret":"s"}"#);
        assert!(check_kind(&file).is_err());
    }

    #[test]
    fn home_is_expanded_only_at_the_start() {
        // Read HOME rather than set it: tests share one process environment.
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_home("~/a/b.json"), home.join("a/b.json"));
        assert_eq!(expand_home("/abs/~/x"), PathBuf::from("/abs/~/x"));
    }

    #[test]
    fn an_empty_path_names_the_setting_to_fill_in() {
        let error = VertexCredentials::load("  ").err().unwrap().to_string();
        assert!(error.contains("Vertex"), "{error}");
    }
}
