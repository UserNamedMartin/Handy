//! OAuth access tokens for Vertex AI, minted from a local credentials file.
//!
//! Vertex does not take API keys here: a plain key is refused outright
//! ("API keys are not supported by this API"), and a key bound to a service
//! account is blocked by the default organization policy
//! `iam.managed.disableServiceAccountApiKeyCreation`. Two credential files work:
//!
//! - **`external_account`** (preferred): Workload Identity Federation. A local
//!   command prints a JWT signed with a local key; Google's STS trades it for an
//!   access token. No user login is involved, so nothing expires. This is what
//!   survives a Workspace domain's Google Cloud session control, which kills a
//!   gcloud login after about a day (`invalid_rapt`).
//! - **`authorized_user`**: what `gcloud auth application-default login` writes,
//!   a refresh token. Fine on a personal account; dies daily on a Workspace one.
//!
//! Service-account key files are not supported: the same org policies forbid
//! creating or uploading SA keys, and they would need RS256 signing in-process.

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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
    #[serde(default)]
    audience: String,
    #[serde(default)]
    subject_token_type: String,
    #[serde(default)]
    token_url: String,
    #[serde(default)]
    credential_source: Option<CredentialSource>,
}

#[derive(Deserialize)]
struct CredentialSource {
    executable: Option<ExecutableSource>,
}

#[derive(Deserialize)]
struct ExecutableSource {
    command: String,
}

/// What the executable prints (Google's executable-sourced credential format).
#[derive(Deserialize)]
struct ExecutableOutput {
    success: bool,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    message: String,
}

/// How a fresh access token is obtained.
enum Grant {
    /// A gcloud user login: trade the refresh token at Google's OAuth endpoint.
    RefreshToken {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
    /// Workload Identity Federation: run `command` for a JWT, trade it at STS.
    Federated {
        command: String,
        audience: String,
        subject_token_type: String,
        token_url: String,
    },
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
    grant: Grant,
    http: reqwest::Client,
    /// Expiry in wall-clock time, not `Instant`: on macOS `Instant` stops while
    /// the machine sleeps, so a token cached before a long lid-close still
    /// looked fresh afterwards and the first dictation came back 401.
    cached: Mutex<Option<(String, SystemTime)>>,
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
        let grant = grant_for(file)?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| anyhow!("Failed to build HTTP client for Google OAuth: {}", e))?;

        Ok(Self {
            path,
            grant,
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

    /// Drop the cached access token so the next call mints a fresh one. For a
    /// request the service answered 401: the token was revoked or the clock
    /// lied, and either way the cached copy is no longer worth trusting.
    pub fn invalidate(&self) {
        *self.cached.lock().unwrap() = None;
    }

    /// A valid access token, refreshed when the cached one is about to expire.
    /// Synchronous; safe to call from any thread (see [`super::block_on`]).
    pub fn access_token(&self) -> Result<String> {
        if let Some((token, valid_until)) = self.cached.lock().unwrap().as_ref() {
            if SystemTime::now() < *valid_until {
                return Ok(token.clone());
            }
        }

        let request = match &self.grant {
            Grant::RefreshToken {
                client_id,
                client_secret,
                refresh_token,
            } => self.http.post(TOKEN_URL).form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("refresh_token", refresh_token.as_str()),
            ]),
            Grant::Federated {
                command,
                audience,
                subject_token_type,
                token_url,
            } => {
                let subject_token = run_token_command(command)?;
                self.http.post(token_url.as_str()).form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
                    ("audience", audience.as_str()),
                    ("scope", "https://www.googleapis.com/auth/cloud-platform"),
                    (
                        "requested_token_type",
                        "urn:ietf:params:oauth:token-type:access_token",
                    ),
                    ("subject_token_type", subject_token_type.as_str()),
                    ("subject_token", subject_token.as_str()),
                ])
            }
        };
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
                // `invalid_rapt` is the common case on a Workspace account: the
                // domain's "Google Cloud session control" ends every gcloud
                // login after a fixed session length (16 h by default), and
                // only a Workspace admin can lengthen or remove it.
                let hint = if text.contains("invalid_rapt") {
                    " — the Workspace session for Google Cloud has expired; run \
                     `gcloud auth application-default login` again"
                } else {
                    ""
                };
                return Err(anyhow!(
                    "Google refused the Vertex credentials in {} ({}){}: {}",
                    path.display(),
                    status,
                    hint,
                    text
                ));
            }
            serde_json::from_str(&text)
                .map_err(|e| anyhow!("Unexpected Google OAuth response: {}", e))
        })?;

        let lifetime = Duration::from_secs(response.expires_in).saturating_sub(EXPIRY_MARGIN);
        *self.cached.lock().unwrap() =
            Some((response.access_token.clone(), SystemTime::now() + lifetime));
        Ok(response.access_token)
    }
}

fn grant_for(file: CredentialsFile) -> Result<Grant> {
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
            Ok(Grant::RefreshToken {
                client_id: file.client_id,
                client_secret: file.client_secret,
                refresh_token: file.refresh_token,
            })
        }
        "external_account" => {
            let command = file
                .credential_source
                .and_then(|source| source.executable)
                .map(|executable| executable.command)
                .filter(|command| !command.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!("Only executable-sourced external_account credentials are supported")
                })?;
            if file.audience.is_empty() || file.token_url.is_empty() {
                return Err(anyhow!(
                    "external_account credentials file is missing audience or token_url"
                ));
            }
            Ok(Grant::Federated {
                command,
                audience: file.audience,
                subject_token_type: if file.subject_token_type.is_empty() {
                    "urn:ietf:params:oauth:token-type:jwt".to_string()
                } else {
                    file.subject_token_type
                },
                token_url: file.token_url,
            })
        }
        "service_account" => Err(anyhow!(
            "Service-account key files are not supported; use an external_account (Workload Identity Federation) file"
        )),
        other => Err(anyhow!("Unsupported Vertex credentials type '{}'", other)),
    }
}

/// Run the credential's executable and return the JWT it prints.
fn run_token_command(command: &str) -> Result<String> {
    let mut parts = command.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("empty credential command"))?;
    let output = std::process::Command::new(expand_home(program))
        .args(parts)
        .output()
        .map_err(|e| anyhow!("Could not run the credential command {}: {}", program, e))?;
    if !output.status.success() {
        return Err(anyhow!(
            "Credential command {} failed: {}",
            program,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let parsed: ExecutableOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow!("Credential command {} printed something unexpected: {}", program, e))?;
    if !parsed.success || parsed.id_token.is_empty() {
        return Err(anyhow!(
            "Credential command {} reported failure: {}",
            program,
            parsed.message
        ));
    }
    Ok(parsed.id_token)
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
        assert!(matches!(grant_for(file), Ok(Grant::RefreshToken { .. })));
    }

    #[test]
    fn an_executable_external_account_file_is_accepted() {
        let file = parse(
            r#"{"type":"external_account","audience":"//iam.googleapis.com/x","token_url":"https://sts.googleapis.com/v1/token",
                "subject_token_type":"urn:ietf:params:oauth:token-type:jwt",
                "credential_source":{"executable":{"command":"~/mint-jwt","timeout_millis":10000}}}"#,
        );
        match grant_for(file) {
            Ok(Grant::Federated { command, audience, .. }) => {
                assert_eq!(command, "~/mint-jwt");
                assert_eq!(audience, "//iam.googleapis.com/x");
            }
            _ => panic!("expected a federated grant"),
        }
    }

    #[test]
    fn an_external_account_without_an_executable_is_refused() {
        let file = parse(
            r#"{"type":"external_account","audience":"a","token_url":"t","credential_source":{"file":"/tmp/x"}}"#,
        );
        assert!(grant_for(file).is_err());
    }

    #[test]
    fn a_service_account_file_is_refused_with_a_reason() {
        let file = parse(r#"{"type":"service_account","private_key":"…"}"#);
        let error = grant_for(file).err().unwrap().to_string();
        assert!(error.contains("not supported"), "{error}");
    }

    #[test]
    fn a_file_without_a_refresh_token_is_refused() {
        let file = parse(r#"{"type":"authorized_user","client_id":"c","client_secret":"s"}"#);
        assert!(grant_for(file).is_err());
    }

    #[test]
    fn a_failing_credential_command_surfaces_its_message() {
        let error = run_token_command("/usr/bin/false").unwrap_err().to_string();
        assert!(error.contains("failed"), "{error}");
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
